//! The per-module AT worker: one blocking thread per modem, owning that
//! modem's serial port for as long as it lives.
//!
//! Split out of `modules::mod` because this is the whole other half of the
//! card pool — it shares nothing with `CardPool` except the two message types
//! in `modules::protocol`. `CardPool` runs on tokio and never touches a serial
//! port; everything here runs on `spawn_blocking` and never touches the slot
//! map. Keeping them in one file made that boundary invisible.
//!
//! The loop state that used to be threaded through `run_module_loop`'s eight
//! parameters (which needed an `#[allow(clippy::too_many_arguments)]`) now
//! lives on [`ModuleWorker`]. The pure helpers below it stay free functions
//! deliberately: they are unit-tested directly against a mocked `AtCommander`
//! or a bare channel pair, and making them methods would force every one of
//! those tests to build a whole worker first.

use crate::alerts;
use crate::metrics;
use crate::modules::at_commander::{AtCommander, AtResponse};
use crate::modules::card::{CardInstance, CardState};
use crate::modules::discovery::DiscoveredModule;
use crate::modules::protocol::{BridgeEvent, ModuleCmd};
use crate::modules::restart_policy::RestartMode;
use crate::modules::slot::usable_phone;
use crate::store::calls::CallRecord;
use crate::store::StoreCommand;
use chrono::Utc;
use std::time::Duration;
use tokio::sync::mpsc;

pub(crate) struct CallContext {
    /// The caller for an inbound call, the dialled number for an outbound one.
    caller_id: String,
    sip_destination: String,
    started_at: chrono::DateTime<Utc>,
    direction: crate::store::calls::CallDirection,
}

pub(crate) struct ModuleAudioInit {
    pub(crate) rx_gain: Option<u32>,
    pub(crate) eec_mode: Option<u32>,
}

/// Everything a worker needs that comes from `AppConfig` or the store rather
/// than from the modem itself. Bundled because `CardPool` spawns workers from
/// three separate places (startup, retry, hotplug rescan) and used to rebuild
/// this same argument list at each one.
pub(crate) struct WorkerSetup {
    pub(crate) store_tx: crossbeam_channel::Sender<StoreCommand>,
    pub(crate) ring_capacity: usize,
    pub(crate) audio: ModuleAudioInit,
    pub(crate) at_worker_unresponsive_threshold: Duration,
    /// specs/034-alert-identity: the card's phone number, already read via
    /// `AT+CNUM` during `try_init_module` and passed in here so the worker does
    /// not issue a second, redundant query on a serial port this repo has a
    /// history of contention on. `None` when the SIM's EF_MSISDN is blank.
    pub(crate) phone_number: Option<String>,
    /// `[metrics].signal_poll_interval_seconds` (specs/122-management-webui);
    /// `0` disables signal polling.
    pub(crate) signal_poll_interval_secs: u64,
}

/// How often the idle-tick liveness probe (`AT`) is sent while otherwise
/// quiet — comfortably below any reasonable `at_worker_unresponsive_sec`
/// threshold (default 60s) so a real stall is caught within one threshold
/// window, not two.
const AT_WORKER_PROBE_INTERVAL: Duration = Duration::from_secs(15);

/// gh#160: how often a line in `Cleanup` asks the modem (`AT+CLCC`) whether
/// the call it just hung up is really gone. A floor, not a cadence: the loop
/// only comes round once per URC read, which blocks for up to the port
/// timeout when the modem is quiet.
const CLEANUP_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// gh#160: a line still listing its call this long after `AT+CHUP` raises a
/// critical alert, once, and keeps waiting.
const CLEANUP_ALERT_AFTER: Duration = Duration::from_secs(10);

/// gh#160: a line still listing its call this long after `AT+CHUP` has its
/// radio cycled (`AT+CFUN=0` -> `1`). If the modem then lists no call, the
/// worker exits so the pool re-initialises the slot; if it still lists one
/// (or cannot say), the module is reset (`AT+CFUN=1,1`). Without this,
/// `Cleanup` would have no way out and the line would stay out of service
/// indefinitely. A modem that refuses the reset keeps the line busy and is
/// tried again after another interval: freeing a line that may still carry a
/// call is the failure this state exists to prevent.
const CLEANUP_RESTART_AFTER: Duration = Duration::from_secs(60);

/// A line waiting for the modem to confirm a locally hung-up call is gone
/// (gh#160). Present exactly while `card.state == CardState::Cleanup`.
struct CleanupWait {
    started: std::time::Instant,
    /// `None` until the first `AT+CLCC` after the hang-up.
    last_poll: Option<std::time::Instant>,
    /// Whether the `CLEANUP_ALERT_AFTER` alert has fired, so it fires once and
    /// a later confirmation can send the matching recovery.
    alerted: bool,
    /// Restart attempts the modem refused (it kept the call and rejected
    /// `AT+CFUN=1,1`). The next attempt is due at
    /// `CLEANUP_RESTART_AFTER * (refused_restarts + 1)`.
    refused_restarts: u32,
}

/// Opens the modem and runs its URC loop until the modem is rebooted, the
/// control channel closes, or the serial port errors.
pub(crate) fn run_module_loop(
    module: DiscoveredModule,
    setup: WorkerSetup,
    event_tx: mpsc::UnboundedSender<BridgeEvent>,
    cmd_rx: crossbeam_channel::Receiver<ModuleCmd>,
) -> Result<(), String> {
    ModuleWorker::open(module, setup, event_tx, cmd_rx)?.run()
}

pub(crate) struct ModuleWorker {
    module: DiscoveredModule,
    at: AtCommander,
    card: CardInstance,
    /// specs/034-alert-identity: the *last known* number of this modem's SIM —
    /// seeded from the read `try_init_module` already did, then refreshed from
    /// the card itself on every inbound SMS ([`refresh_sim_phone_number`]).
    ///
    /// The SMS path always emits a freshly-read value, so it names the card
    /// that actually received the message. The critical-alert paths take this
    /// cached value as-is: they fire when the AT channel may be wedged (the
    /// "worker unresponsive" alert is precisely that case) or mid call-teardown,
    /// where adding an AT round-trip would block or time out. Last-known is the
    /// best obtainable there, and a stale number on a "this module is broken"
    /// alert is far less consequential than one on a forwarded message.
    ///
    /// `None` when the SIM has no number provisioned ⇒ alerts render `unknown`.
    phone_number: Option<String>,
    store_tx: crossbeam_channel::Sender<StoreCommand>,
    event_tx: mpsc::UnboundedSender<BridgeEvent>,
    cmd_rx: crossbeam_channel::Receiver<ModuleCmd>,
    call_ctx: Option<CallContext>,
    at_worker_unresponsive_threshold: Duration,
    /// specs/022-discord-critical-alerts FR-003: last time an AT command on
    /// this module succeeded, and whether we've already alerted for it being
    /// unresponsive (so we don't re-alert every tick — FR-013).
    last_at_success: std::time::Instant,
    last_at_probe: std::time::Instant,
    at_worker_alerted: bool,
    /// See [`WorkerSetup::signal_poll_interval_secs`].
    signal_poll_interval_secs: u64,
    last_signal_poll: std::time::Instant,
    /// See [`CleanupWait`].
    cleanup: Option<CleanupWait>,
}

impl ModuleWorker {
    /// Opens the serial port and applies the one-time modem configuration
    /// (echo off, CLIP/CMGF/CNMI/CREG URCs, USB audio routing, gains).
    fn open(
        module: DiscoveredModule,
        setup: WorkerSetup,
        event_tx: mpsc::UnboundedSender<BridgeEvent>,
        cmd_rx: crossbeam_channel::Receiver<ModuleCmd>,
    ) -> Result<Self, String> {
        let mut at = AtCommander::open(&module.serial_port).map_err(|e| e.to_string())?;

        at.send_command("ATE0").ok();
        at.send_command("AT+CLIP=1").ok();
        // PDU mode, not text mode: text mode cannot represent UCS-2 (TS
        // 27.005 §3.1 falls back to hex-encoding both the body and a
        // non-plain-numeric `<oa>` when the message's DCS doesn't match the
        // terminal charset, which nothing here ever decoded) and can't
        // decode identically to every other inbound route — see
        // `sms::reader::decode_pdu_line`'s docs and specs/045's SMS-04.
        // Unlike the other best-effort setup commands here, a failure of
        // this one is not survivable: `handle_cmti` unconditionally hands
        // every AT+CMGR response to the PDU decoder, so a modem stuck in
        // text mode would have its SMS silently misdecoded (or fail to
        // decode) instead of merely losing a nice-to-have (review finding,
        // PR #73) — logged as an error, not swallowed by `.ok()`.
        set_pdu_sms_mode(&mut at, &module.id);
        at.send_command("AT+CNMI=2,1,0,0,0").ok();
        at.send_command("AT+CREG=1").ok();
        at.send_command("AT+CEREG=1").ok();
        route_audio_to_usb(&mut at, &module.id);
        if let Some(gain) = setup.audio.rx_gain {
            set_rx_gain(&mut at, &module.id, gain);
        }
        if let Some(mode) = setup.audio.eec_mode {
            set_eec_mode(&mut at, &module.id, mode);
        }

        if let Ok((rssi, _ber)) = at.check_signal() {
            tracing::info!(module = %module.id, rssi = rssi, "signal quality");
        }

        // specs/034-alert-identity: the card's number was already read via
        // AT+CNUM in `try_init_module`; reuse it rather than querying again.
        let phone_number = setup.phone_number;

        let card = CardInstance::new(
            module.id.clone(),
            module.serial_port.clone(),
            module.audio_device.clone(),
            setup.ring_capacity,
        );

        tracing::info!(module = %module.id, "module worker started, monitoring for events");
        metrics::ACTIVE_CALLS
            .with_label_values(&[&module.id, "cs"])
            .set(0.0);

        let now = std::time::Instant::now();
        Ok(Self {
            module,
            at,
            card,
            phone_number,
            store_tx: setup.store_tx,
            event_tx,
            cmd_rx,
            call_ctx: None,
            at_worker_unresponsive_threshold: setup.at_worker_unresponsive_threshold,
            last_at_success: now,
            last_at_probe: now,
            at_worker_alerted: false,
            signal_poll_interval_secs: setup.signal_poll_interval_secs,
            last_signal_poll: now,
            cleanup: None,
        })
    }

    fn run(mut self) -> Result<(), String> {
        loop {
            self.tick_liveness_probe();
            if self.tick_cleanup() {
                return Ok(());
            }

            // If cmd_tx side was dropped (slot restarted), try_recv will see a disconnect.
            // We check via a separate try_recv for the disconnect error.
            match self.cmd_rx.try_recv() {
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    tracing::info!(module = %self.module.id, "control channel closed, worker exiting");
                    return Ok(());
                }
                Ok(cmd) => {
                    if self.apply_cmd(cmd) {
                        return Ok(());
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {}
            }

            let line = match read_line_from_at(&mut self.at) {
                Ok(l) => l,
                Err(e) => {
                    if e.contains("timeout") || e.contains("TimedOut") {
                        continue;
                    }
                    tracing::error!(module = %self.module.id, error = %e, "serial read error");
                    return Err(e);
                }
            };

            let trimmed = line.trim().to_string();
            if trimmed.is_empty() {
                continue;
            }

            tracing::trace!(module = %self.module.id, urc = %trimmed, "received");
            self.dispatch_urc(&trimmed);
        }
    }

    /// Sends the liveness probe, returning whether the modem answered.
    ///
    /// Usually a bare `AT`. When a signal poll is due (specs/122-management-
    /// webui) it is `AT+CSQ` plus `AT+QENG` instead, and a `+CSQ` answer
    /// counts as liveness exactly as `AT`'s `OK` does -- no extra command is
    /// added to an idle modem. Never during a call: see
    /// [`signal::poll_due`](crate::modules::signal::poll_due).
    fn probe_modem(&mut self) -> bool {
        use crate::modules::signal;
        let due = signal::poll_due(
            self.signal_poll_interval_secs,
            self.last_signal_poll.elapsed(),
            // A line in `Cleanup` may still hold its call (gh#160).
            self.call_ctx.is_some() || self.cleanup.is_some(),
        );
        if !due {
            return matches!(self.at.send_command("AT"), Ok(AtResponse::Ok(_)));
        }
        self.last_signal_poll = std::time::Instant::now();
        let (reading, answered) = signal::read_signal_checked(&mut self.at);
        if answered {
            metrics::set_signal(&self.module.id, "cs", &reading);
            crate::webui::signal::update(&self.module.id, reading);
        }
        answered
    }

    /// specs/022-discord-critical-alerts FR-003: probe the modem every
    /// `AT_WORKER_PROBE_INTERVAL` and alert once (not every tick) if it has
    /// been silent past the configured threshold.
    fn tick_liveness_probe(&mut self) {
        if self.last_at_probe.elapsed() < AT_WORKER_PROBE_INTERVAL {
            return;
        }
        self.last_at_probe = std::time::Instant::now();
        if self.probe_modem() {
            self.last_at_success = std::time::Instant::now();
            if self.at_worker_alerted {
                self.at_worker_alerted = false;
                let _ = self
                    .event_tx
                    .send(BridgeEvent::CriticalAlert(alerts::CriticalEvent {
                        category: alerts::AlertCategory::ModuleLifecycle,
                        unit_id: Some(self.module.id.clone()),
                        description: "AT command worker responsive again".to_string(),
                        phone_number: self.phone_number.clone(),
                        at: Utc::now(),
                        kind: alerts::CriticalEventKind::Recovered,
                    }));
            }
        } else if !self.at_worker_alerted
            && at_worker_unresponsive(
                self.last_at_success,
                std::time::Instant::now(),
                self.at_worker_unresponsive_threshold,
            )
        {
            self.at_worker_alerted = true;
            tracing::error!(module = %self.module.id, "AT command worker unresponsive");
            let _ = self
                .event_tx
                .send(BridgeEvent::CriticalAlert(alerts::CriticalEvent {
                    category: alerts::AlertCategory::ModuleLifecycle,
                    unit_id: Some(self.module.id.clone()),
                    description: format!(
                        "AT command worker unresponsive for over {}s",
                        self.at_worker_unresponsive_threshold.as_secs()
                    ),
                    phone_number: self.phone_number.clone(),
                    at: Utc::now(),
                    kind: alerts::CriticalEventKind::Failure,
                }));
        }
    }

    /// Applies one pool-issued command. Returns `true` when the worker must
    /// exit — both `Reboot` variants leave the modem re-enumerating, so this
    /// thread must release the port and let `CardPool` re-init the slot.
    fn apply_cmd(&mut self, cmd: ModuleCmd) -> bool {
        match cmd {
            ModuleCmd::SetMode(mode, resp_tx) => {
                let result = self.at.set_network_mode(mode).map_err(|e| e.to_string());
                let _ = resp_tx.send(result);
                false
            }
            ModuleCmd::Reboot(RestartMode::Full) => {
                tracing::info!(module = %self.module.id, "rebooting modem (AT+CFUN=1,1)");
                self.at.reboot();
                true
            }
            ModuleCmd::Reboot(RestartMode::Radio) => {
                tracing::info!(module = %self.module.id, "radio-cycling modem (AT+CFUN=0 -> 1)");
                self.at.radio_restart();
                true
            }
            ModuleCmd::Dial(number, resp_tx) => {
                let result = apply_dial_cmd(&mut self.at, &mut self.card, &number);
                if result.is_ok() {
                    record_call_start_outbound(&self.module.id, &number, &mut self.call_ctx);
                }
                let _ = resp_tx.send(result);
                false
            }
            ModuleCmd::Hangup => {
                tracing::info!(module = %self.module.id, "hanging up a call that could not be bridged");
                self.hang_up_and_clean_up("failed");
                false
            }
            ModuleCmd::SipPeerHangup => {
                match self.card.state {
                    CardState::Bridged | CardState::Answering => {
                        tracing::info!(module = %self.module.id, "SIP peer disconnected, hanging up GSM");
                        self.hang_up_and_clean_up("answered");
                    }
                    // Already waiting for confirmation; that wait ends in a
                    // `LineIdle` of its own.
                    CardState::Cleanup => {}
                    // The GSM side ended first (`NO CARRIER` won the race),
                    // so there is nothing to hang up. The pool is still
                    // holding the line busy until it hears back.
                    CardState::Idle | CardState::Ringing => self.emit_line_idle(),
                }
                false
            }
        }
    }

    /// Hangs up the call on this line (`AT+CHUP`), closes out its call record
    /// and holds the line in `Cleanup` until the modem confirms the call is
    /// gone (gh#160). A local `AT+CHUP` often produces no `NO CARRIER`, so
    /// going straight to `Idle` let a `RING` or a `Dial` reuse a line the
    /// modem still had a call on.
    ///
    /// Checks the call list once straight away: EC20's `AT+CHUP` normally
    /// completes before its `OK`, so the usual case leaves here `Idle`.
    fn hang_up_and_clean_up(&mut self, status: &str) {
        if let Err(e) = self.at.hangup() {
            tracing::warn!(module = %self.module.id, error = %e, "AT+CHUP failed, waiting for the modem to drop the call");
        }
        record_call_end(
            &self.module.id,
            &self.event_tx,
            &self.store_tx,
            &mut self.call_ctx,
            status,
            self.phone_number.as_deref(),
        );
        self.card.state = CardState::Cleanup;
        // A second hang-up while already waiting keeps the original clock, so
        // repeated commands cannot postpone the alert or the restart.
        if self.cleanup.is_none() {
            self.cleanup = Some(CleanupWait {
                started: std::time::Instant::now(),
                last_poll: None,
                alerted: false,
                refused_restarts: 0,
            });
        }
        // Cannot reach the restart threshold on the first check.
        let _ = self.tick_cleanup();
    }

    /// Advances a pending `Cleanup` (gh#160). Returns `true` when the worker
    /// must exit, which is only after a restart at `CLEANUP_RESTART_AFTER`
    /// that left no call listed, or a module reset the modem did not refuse.
    fn tick_cleanup(&mut self) -> bool {
        let Some(wait) = self.cleanup.as_mut() else {
            return false;
        };
        if wait
            .last_poll
            .is_some_and(|t| t.elapsed() < CLEANUP_POLL_INTERVAL)
        {
            return false;
        }
        wait.last_poll = Some(std::time::Instant::now());

        match self.at.has_voice_call() {
            Ok(false) => {
                self.finish_cleanup("confirmed_clcc");
                return false;
            }
            Ok(true) => {}
            // A failed query is not an empty list: keep the line busy.
            Err(e) => {
                tracing::warn!(module = %self.module.id, error = %e, "AT+CLCC failed during hang-up cleanup");
            }
        }
        self.check_cleanup_deadlines()
    }

    /// The alert and restart thresholds for a call the modem still lists.
    fn check_cleanup_deadlines(&mut self) -> bool {
        let Some(wait) = self.cleanup.as_mut() else {
            return false;
        };
        let elapsed = wait.started.elapsed();
        if elapsed >= CLEANUP_RESTART_AFTER * (wait.refused_restarts + 1) {
            return self.restart_stuck_line(elapsed);
        }
        if elapsed >= CLEANUP_ALERT_AFTER && !wait.alerted {
            wait.alerted = true;
            tracing::error!(
                module = %self.module.id,
                elapsed_s = elapsed.as_secs(),
                "modem still lists the call after AT+CHUP, keeping the line busy"
            );
            metrics::CS_HANGUP_CLEANUP_TOTAL
                .with_label_values(&[&self.module.id, "stuck_alerted"])
                .inc();
            self.send_lifecycle_alert(
                format!(
                    "call still up {}s after hang-up, line held busy",
                    elapsed.as_secs()
                ),
                alerts::CriticalEventKind::Failure,
            );
        }
        false
    }

    /// Cycles the radio on a line whose call outlived `AT+CHUP`, escalating to
    /// a module reset if the call survives that too. Returns `true` (exit, so
    /// the pool re-initialises the slot) only once no call can remain: the
    /// modem lists none after the radio cycle, or it did not refuse the reset.
    /// `AT+CFUN=0` itself is fire-and-forget, so the call list afterwards is
    /// what decides, not that command's reply.
    fn restart_stuck_line(&mut self, elapsed: Duration) -> bool {
        // The Failure below needs its matching Recovered if the call later
        // clears on this worker (`finish_cleanup`), even when this tick is the
        // first to see the line past the alert threshold.
        if let Some(wait) = self.cleanup.as_mut() {
            wait.alerted = true;
        }
        tracing::error!(
            module = %self.module.id,
            elapsed_s = elapsed.as_secs(),
            "modem still lists the call after AT+CHUP, cycling the radio"
        );
        metrics::CS_HANGUP_CLEANUP_TOTAL
            .with_label_values(&[&self.module.id, "radio_restart"])
            .inc();
        self.send_lifecycle_alert(
            format!(
                "call still up {}s after hang-up, cycling the radio",
                elapsed.as_secs()
            ),
            alerts::CriticalEventKind::Failure,
        );
        self.at.radio_restart();
        match self.at.has_voice_call() {
            Ok(false) => return self.leave_for_reinit(),
            Ok(true) => tracing::error!(
                module = %self.module.id,
                "modem still lists the call after a radio cycle, resetting the module"
            ),
            Err(e) => tracing::error!(
                module = %self.module.id,
                error = %e,
                "cannot confirm the call ended after a radio cycle, resetting the module"
            ),
        }
        metrics::CS_HANGUP_CLEANUP_TOTAL
            .with_label_values(&[&self.module.id, "module_reset"])
            .inc();
        match self.at.try_reboot() {
            Ok(()) => self.leave_for_reinit(),
            Err(e) => {
                if let Some(wait) = self.cleanup.as_mut() {
                    wait.refused_restarts += 1;
                }
                tracing::error!(
                    module = %self.module.id,
                    error = %e,
                    "modem refused the reset, keeping the line busy"
                );
                metrics::CS_HANGUP_CLEANUP_TOTAL
                    .with_label_values(&[&self.module.id, "reset_refused"])
                    .inc();
                self.send_lifecycle_alert(
                    format!("modem refused a reset with a call still up, line held busy: {e}"),
                    alerts::CriticalEventKind::Failure,
                );
                false
            }
        }
    }

    /// The worker is about to exit after a restart; the replacement starts
    /// with no call.
    fn leave_for_reinit(&mut self) -> bool {
        metrics::ACTIVE_CALLS
            .with_label_values(&[&self.module.id, "cs"])
            .set(0.0);
        true
    }

    /// The modem confirmed the hung-up call is gone: free the line.
    fn finish_cleanup(&mut self, outcome: &str) {
        let wait = self.cleanup.take();
        self.card.state = CardState::Idle;
        metrics::ACTIVE_CALLS
            .with_label_values(&[&self.module.id, "cs"])
            .set(0.0);
        metrics::CS_HANGUP_CLEANUP_TOTAL
            .with_label_values(&[&self.module.id, outcome])
            .inc();
        let elapsed_ms = wait.as_ref().map_or(0, |w| w.started.elapsed().as_millis());
        tracing::info!(module = %self.module.id, outcome, elapsed_ms, "hang-up confirmed, line idle");
        if wait.is_some_and(|w| w.alerted) {
            self.send_lifecycle_alert(
                "call finally cleared after hang-up, line idle again".to_string(),
                alerts::CriticalEventKind::Recovered,
            );
        }
        self.emit_line_idle();
    }

    fn emit_line_idle(&self) {
        let _ = self.event_tx.send(BridgeEvent::LineIdle {
            module_id: self.module.id.clone(),
        });
    }

    fn send_lifecycle_alert(&self, description: String, kind: alerts::CriticalEventKind) {
        let _ = self
            .event_tx
            .send(BridgeEvent::CriticalAlert(alerts::CriticalEvent {
                category: alerts::AlertCategory::ModuleLifecycle,
                unit_id: Some(self.module.id.clone()),
                description,
                phone_number: self.phone_number.clone(),
                at: Utc::now(),
                kind,
            }));
    }

    fn dispatch_urc(&mut self, trimmed: &str) {
        if trimmed == "RING" {
            self.handle_ring();
        } else if trimmed == "NO CARRIER" {
            self.handle_hangup();
        } else if trimmed.starts_with("+CMTI:") {
            self.handle_cmti(trimmed);
        } else if trimmed.starts_with("+CREG:") || trimmed.starts_with("+CEREG:") {
            self.handle_creg_urc(trimmed);
        }
    }

    fn handle_ring(&mut self) {
        if self.card.state != CardState::Idle {
            return;
        }

        tracing::info!(module = %self.module.id, "incoming call (RING)");
        self.card.state = CardState::Ringing;
        metrics::CALLS_TOTAL
            .with_label_values(&[&self.module.id, "incoming", "cs"])
            .inc();

        let caller_id = extract_caller_id(&mut self.at);

        match self.at.answer_call() {
            Ok(()) => {
                self.card.state = CardState::Answering;
                tracing::info!(
                    module = %self.module.id,
                    caller = %caller_id,
                    "call answered, requesting SIP bridge"
                );

                self.call_ctx = Some(CallContext {
                    caller_id: caller_id.clone(),
                    sip_destination: String::new(),
                    started_at: Utc::now(),
                    direction: crate::store::calls::CallDirection::Inbound,
                });

                let _ = self.event_tx.send(BridgeEvent::Ring {
                    module_id: self.module.id.clone(),
                    caller_id,
                    audio_device: self.module.audio_device.clone(),
                });

                self.card.state = CardState::Bridged;
                metrics::ACTIVE_CALLS
                    .with_label_values(&[&self.module.id, "cs"])
                    .set(1.0);
                metrics::CALLS_TOTAL
                    .with_label_values(&[&self.module.id, "answered", "cs"])
                    .inc();
            }
            Err(e) => {
                tracing::error!(module = %self.module.id, error = %e, "failed to answer call");
                self.card.state = CardState::Idle;
                metrics::CALLS_TOTAL
                    .with_label_values(&[&self.module.id, "missed", "cs"])
                    .inc();
            }
        }
    }

    fn handle_hangup(&mut self) {
        // gh#160: the confirmation a `Cleanup` was waiting for. The call was
        // already recorded and the SIP side already torn down when it was
        // hung up, so neither happens again here.
        if self.card.state == CardState::Cleanup {
            self.finish_cleanup("confirmed_no_carrier");
            return;
        }
        if self.card.state == CardState::Bridged || self.card.state == CardState::Answering {
            tracing::info!(module = %self.module.id, "call ended (NO CARRIER)");
            metrics::ACTIVE_CALLS
                .with_label_values(&[&self.module.id, "cs"])
                .set(0.0);
            let _ = self.event_tx.send(BridgeEvent::Hangup {
                module_id: self.module.id.clone(),
            });
            record_call_end(
                &self.module.id,
                &self.event_tx,
                &self.store_tx,
                &mut self.call_ctx,
                "answered",
                self.phone_number.as_deref(),
            );
        } else if self.card.state == CardState::Ringing {
            record_call_end(
                &self.module.id,
                &self.event_tx,
                &self.store_tx,
                &mut self.call_ctx,
                "missed",
                self.phone_number.as_deref(),
            );
        }
        self.card.state = CardState::Idle;
    }

    fn handle_cmti(&mut self, line: &str) {
        tracing::info!(module = %self.module.id, notification = line, "SMS notification received");
        metrics::SMS_RECEIVED_TOTAL
            .with_label_values(&[&self.module.id, "cs"])
            .inc();

        let Some(idx_str) = line.split(',').next_back() else {
            return;
        };
        let Ok(idx) = idx_str.trim().parse::<u32>() else {
            return;
        };

        // PDU-mode read, decoded through the same parser every other inbound
        // route uses (`crate::ims::sms_pdu`, via `crate::sms::reader`) rather
        // than a second, incomplete text-mode parser of this route's own —
        // see `open`'s `AT+CMGF=0` comment for why text mode isn't safe here.
        match crate::sms::reader::read_sms(&mut self.at, idx) {
            Ok(sms) => {
                tracing::debug!(module = %self.module.id, index = idx, sender = %sms.sender, "SMS read");

                let received_at = Utc::now().to_rfc3339();

                // specs/034-alert-identity: read the number off the card that
                // just produced this message, rather than trusting a copy
                // cached at worker open (which a SIM swap invalidates) or one
                // looked up from the slot map at dispatch time (which races
                // with slot recovery). `AT+CNUM` does not touch SMS storage, so
                // issuing it here leaves the pending delete-by-index valid.
                refresh_sim_phone_number(&mut self.at, &mut self.phone_number);

                let _ = self.event_tx.send(BridgeEvent::SmsReceived {
                    module_id: self.module.id.clone(),
                    sender: sms.sender,
                    body: sms.body,
                    received_at,
                    phone_number: self.phone_number.clone(),
                });

                // Deleted only after the event is queued, so the message is
                // never dropped from the SIM before it is persisted downstream
                // (specs/006-sms-discord-forward).
                let del_cmd = format!("AT+CMGD={idx}");
                self.at.send_command(&del_cmd).ok();
            }
            Err(e) => {
                tracing::warn!(module = %self.module.id, error = %e, "failed to read SMS");
            }
        }
    }

    fn handle_creg_urc(&mut self, line: &str) {
        // URC format: +CREG: <stat> (no leading comma since it's a URC, not a response)
        let stat_str = line
            .split_once(':')
            .map(|(_, rest)| rest)
            .unwrap_or("")
            .trim()
            .split(',')
            .next()
            .unwrap_or("")
            .trim();
        let stat: u8 = stat_str.parse().unwrap_or(0);
        // 0=not registered, 2=searching, 3=denied → network loss
        if stat == 0 || stat == 2 || stat == 3 {
            tracing::warn!(module = %self.module.id, stat = stat, "network registration lost");
            let _ = self.event_tx.send(BridgeEvent::NetworkLost {
                module_id: self.module.id.clone(),
            });
        }
    }
}

/// specs/022-discord-critical-alerts FR-003. Duration-based, matching the
/// same shape `metrics::ingest`'s staleness/health checks use elsewhere in
/// this feature (research.md R4) — real `Instant` arithmetic, no mock, so
/// tests can construct `last_success` in the past instead of sleeping.
fn at_worker_unresponsive(
    last_success: std::time::Instant,
    now: std::time::Instant,
    threshold: Duration,
) -> bool {
    now.duration_since(last_success) >= threshold
}

/// The body of `ModuleCmd::Dial` handling, pulled out of the worker's
/// match arm so it's unit-testable against a mocked `AtCommander` the same
/// way `at_commander`'s own tests mock the serial stream
/// (specs/025-outbound-calling, contracts/control-cmd-dial.md).
///
/// Same-thread serialization is the race guard: the worker only ever
/// processes one `ModuleCmd` at a time, so this check and the dial that
/// follows cannot race against another `Dial` for this line
/// (research.md R-003) — no separate provisional-claim step is needed here,
/// unlike the cross-process case.
fn apply_dial_cmd(
    at: &mut AtCommander,
    card: &mut CardInstance,
    number: &str,
) -> Result<(), String> {
    if card.state != CardState::Idle {
        return Err("line busy".to_string());
    }
    let result = at.dial(number).map_err(|e| e.to_string());
    if result.is_ok() {
        // Reuses `Answering` as "call setup in progress, not yet Bridged"
        // rather than adding a new state — the full outbound state machine
        // (progress relay, teardown) is specs/025-outbound-calling
        // T019/T020, not yet wired to this loop.
        card.state = CardState::Answering;
    }
    result
}

fn read_line_from_at(at: &mut AtCommander) -> Result<String, String> {
    match at.read_line_raw() {
        Ok(line) => Ok(line),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("timeout") {
                Ok(String::new())
            } else {
                Err(msg)
            }
        }
    }
}

/// Re-reads the number of the SIM *currently* in this modem and updates
/// `cached` (specs/034-alert-identity).
///
/// The MSISDN belongs to the SIM, not to the modem or the slot, so any cached
/// copy goes stale the moment a card is swapped — the bug class behind three
/// successive review findings on this feature. The SMS path therefore refreshes
/// from the same AT session that just read the message, which is the only way
/// to know the number and the message came off the same card.
///
/// A *successful* read always wins, including when it reports no number: a
/// replacement SIM with a blank EF_MSISDN must render `unknown` rather than
/// inherit the removed card's number (FR-005 — never a value that looks real
/// but isn't). Only a read that fails outright leaves `cached` alone, since a
/// broken AT channel says nothing about which SIM is present.
fn refresh_sim_phone_number(at: &mut AtCommander, cached: &mut Option<String>) {
    if let Ok(fresh) = at.query_phone_number() {
        *cached = usable_phone(&fresh);
    }
}

fn extract_caller_id(at: &mut AtCommander) -> String {
    for _ in 0..5 {
        match at.read_line_raw() {
            Ok(line) => {
                let trimmed = line.trim();
                if let Some(clip_data) = trimmed.strip_prefix("+CLIP:") {
                    let parts: Vec<&str> = clip_data.split(',').collect();
                    if let Some(number) = parts.first() {
                        return number.trim().trim_matches('"').to_string();
                    }
                }
                if trimmed == "RING" || trimmed.is_empty() {
                    continue;
                }
            }
            Err(_) => break,
        }
    }
    "unknown".to_string()
}

/// The outbound mirror of `handle_ring`'s own call-start bookkeeping —
/// called once `apply_dial_cmd` confirms `ATD` was accepted. Without this,
/// an outbound call was invisible in call history (`call_ctx` stayed
/// `None`, so `record_call_end`'s `if let Some(ctx) = call_ctx.take()`
/// silently did nothing on teardown) while `ACTIVE_CALLS` never got set to
/// `1.0` in the first place, only spuriously reset to `0.0` later
/// (specs/025-outbound-calling review). `caller_id` holds the dialed
/// destination here rather than an inbound caller's number — the same
/// field, repurposed for the direction it wasn't originally written for,
/// rather than widening `CallContext`'s shape for one new caller.
fn record_call_start_outbound(
    module_id: &str,
    destination: &str,
    call_ctx: &mut Option<CallContext>,
) {
    *call_ctx = Some(CallContext {
        caller_id: destination.to_string(),
        sip_destination: String::new(),
        started_at: Utc::now(),
        direction: crate::store::calls::CallDirection::Outbound,
    });
    metrics::ACTIVE_CALLS
        .with_label_values(&[module_id, "cs"])
        .set(1.0);
    metrics::CALLS_TOTAL
        .with_label_values(&[module_id, "outgoing", "cs"])
        .inc();
}

fn record_call_end(
    module_id: &str,
    event_tx: &mpsc::UnboundedSender<BridgeEvent>,
    store_tx: &crossbeam_channel::Sender<StoreCommand>,
    call_ctx: &mut Option<CallContext>,
    status: &str,
    phone_number: Option<&str>,
) {
    if let Some(ctx) = call_ctx.take() {
        let duration = Utc::now()
            .signed_duration_since(ctx.started_at)
            .num_seconds() as f64;

        metrics::CALL_DURATION_SECONDS
            .with_label_values(&[module_id, "cs"])
            .observe(duration);

        // specs/022-discord-critical-alerts FR-002/Clarifications Q4: only
        // the `CallStatus::Missed` outcome (never bridged) alerts — a call
        // that bridged but had broken audio (`Failed`) is a distinct,
        // already-tracked outcome and is deliberately excluded here.
        if status == "missed" {
            let _ = event_tx.send(BridgeEvent::CriticalAlert(alerts::CriticalEvent {
                category: alerts::AlertCategory::MissedCall,
                unit_id: Some(module_id.to_string()),
                description: format!("call from {} was never answered", ctx.caller_id),
                phone_number: phone_number.map(str::to_string),
                at: Utc::now(),
                kind: alerts::CriticalEventKind::Failure,
            }));
        }

        let record = CallRecord {
            module_id: module_id.to_string(),
            caller_id: ctx.caller_id,
            started_at: ctx.started_at.to_rfc3339(),
            duration_seconds: duration,
            status: status.to_string(),
            sip_destination: ctx.sip_destination,
            transport: crate::store::Transport::Cs,
            direction: ctx.direction,
            loss_percent: None,
            jitter_ms: None,
            rtt_ms: None,
        };
        if let Err(e) = store_tx.send(StoreCommand::InsertCall(record)) {
            tracing::error!(error = %e, "failed to send call record to store");
        }
    }
}

/// How many times [`set_pdu_sms_mode`] retries `AT+CMGF=0` before giving up
/// and logging an error — a modem fresh off the preceding `AT+CLIP=1` has
/// occasionally been seen to answer the very next command with a stray or
/// desynced reply, which a single attempt would misreport as a real
/// failure to enter PDU mode.
const CMGF_SET_ATTEMPTS: u32 = 3;
const CMGF_SET_RETRY_DELAY: Duration = Duration::from_millis(300);

/// `AT+CMGF=0`, switching SMS handling to PDU mode — see `open`'s call site
/// for why a failure here (even after retrying) is logged as an error
/// rather than swallowed like this file's other best-effort setup commands.
fn set_pdu_sms_mode(at: &mut AtCommander, module_id: &str) {
    for attempt in 0..CMGF_SET_ATTEMPTS {
        if matches!(at.send_command("AT+CMGF=0"), Ok(AtResponse::Ok(_))) {
            return;
        }
        if attempt + 1 < CMGF_SET_ATTEMPTS {
            std::thread::sleep(CMGF_SET_RETRY_DELAY);
        }
    }
    tracing::error!(
        module = %module_id,
        attempts = CMGF_SET_ATTEMPTS,
        "AT+CMGF=0 (PDU mode) failed after retrying; inbound SMS on this line will not decode correctly"
    );
}

fn route_audio_to_usb(at: &mut AtCommander, module_id: &str) {
    match at.send_command("AT+QPCMV=1,2") {
        Ok(AtResponse::Ok(_)) => {
            tracing::info!(module = %module_id, "voice audio routed to USB (AT+QPCMV=1,2)");
        }
        _ => {
            tracing::warn!(module = %module_id, "AT+QPCMV=1,2 failed, trying AT+QPCMV=1,0");
            match at.send_command("AT+QPCMV=1,0") {
                Ok(AtResponse::Ok(_)) => {
                    tracing::info!(module = %module_id, "voice audio routed to USB (AT+QPCMV=1,0)");
                }
                _ => {
                    tracing::error!(
                        module = %module_id,
                        "failed to route voice audio to USB — audio will not work"
                    );
                }
            }
        }
    }
}

fn set_rx_gain(at: &mut AtCommander, module_id: &str, gain: u32) {
    let cmd = format!("AT+QRXGAIN={gain}");
    match at.send_command(&cmd) {
        Ok(AtResponse::Ok(_)) => {
            tracing::info!(module = %module_id, gain, "EC20 receive gain set (AT+QRXGAIN)");
        }
        _ => {
            tracing::warn!(module = %module_id, gain, "AT+QRXGAIN command failed; using modem default");
        }
    }
}

fn set_eec_mode(at: &mut AtCommander, module_id: &str, mode: u32) {
    let cmd = format!("AT+QEEC=2,{mode}");
    match at.send_command(&cmd) {
        Ok(AtResponse::Ok(_)) => {
            tracing::info!(module = %module_id, mode, "EC20 echo-canceller mode set (AT+QEEC)");
        }
        _ => {
            tracing::warn!(module = %module_id, mode, "AT+QEEC command failed; using modem default");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- specs/122-management-webui: signal polling on the liveness tick ----

    /// A worker over a scripted fake modem. `replies` answer the commands the
    /// worker sends, in order; the returned counter says how many it saw.
    fn worker_with_modem(
        module_id: &str,
        interval_secs: u64,
        replies: Vec<&'static str>,
    ) -> (ModuleWorker, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let (worker, seen, _events, _store) =
            worker_with_modem_and_channels(module_id, interval_secs, replies);
        (worker, seen)
    }

    type Events = mpsc::UnboundedReceiver<BridgeEvent>;
    type Store = crossbeam_channel::Receiver<StoreCommand>;

    /// [`worker_with_modem`], also handing back what the worker sends to the
    /// pool and to the store.
    fn worker_with_modem_and_channels(
        module_id: &str,
        interval_secs: u64,
        replies: Vec<&'static str>,
    ) -> (
        ModuleWorker,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        Events,
        Store,
    ) {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (client, mut modem) = std::os::unix::net::UnixStream::pair().unwrap();
        let seen = std::sync::Arc::new(AtomicUsize::new(0));
        let seen_in_thread = seen.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            for reply in replies {
                match modem.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                seen_in_thread.fetch_add(1, Ordering::SeqCst);
                if modem.write_all(reply.as_bytes()).is_err() {
                    return;
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        });
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (_cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (store_tx, store_rx) = crossbeam_channel::unbounded();
        let module = DiscoveredModule {
            id: module_id.to_string(),
            serial_port: std::path::PathBuf::from("/dev/null"),
            audio_device: String::new(),
            usb_serial: String::new(),
        };
        let long_ago = std::time::Instant::now() - Duration::from_secs(3600);
        let worker = ModuleWorker {
            card: CardInstance::new(
                module.id.clone(),
                module.serial_port.clone(),
                module.audio_device.clone(),
                16,
            ),
            module,
            at: AtCommander::from_stream(client, Duration::from_millis(300)),
            phone_number: None,
            store_tx,
            event_tx,
            cmd_rx,
            call_ctx: None,
            at_worker_unresponsive_threshold: Duration::from_secs(60),
            last_at_success: long_ago,
            last_at_probe: long_ago,
            at_worker_alerted: false,
            signal_poll_interval_secs: interval_secs,
            last_signal_poll: long_ago,
            cleanup: None,
        };
        (worker, seen, event_rx, store_rx)
    }

    const CSQ_OK: &str = "\r\n+CSQ: 21,99\r\n\r\nOK\r\n";
    const QENG_LTE: &str = "\r\n+QENG: \"servingcell\",\"NOCONN\",\"LTE\",\"FDD\",404,43,6BD6814,247,1357,3,3,3,D55E,-95,-13,-66,41,28\r\n\r\nOK\r\n";

    #[test]
    fn a_due_poll_replaces_the_bare_probe_and_counts_as_liveness() {
        let (mut w, seen) = worker_with_modem("ec20-SIGPOL", 60, vec![CSQ_OK, QENG_LTE]);
        w.tick_liveness_probe();
        assert_eq!(
            seen.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "AT+CSQ and AT+QENG, with no separate AT"
        );
        assert!(
            w.last_at_success.elapsed() < Duration::from_secs(5),
            "a +CSQ answer is a liveness success"
        );
        let reading = crate::webui::signal::all()["ec20-SIGPOL"].clone();
        assert_eq!(reading.rssi_dbm, Some(-71));
        assert_eq!(reading.rsrp_dbm, Some(-95));
        let out = prometheus::TextEncoder::new()
            .encode_to_string(&prometheus::gather())
            .unwrap();
        assert!(out.contains(
            "gsm_sip_bridge_signal_rssi_dbm{module=\"ec20-SIGPOL\",transport=\"cs\"} -71"
        ));
    }

    #[test]
    fn no_poll_while_a_call_is_active() {
        let (mut w, seen) = worker_with_modem("ec20-SIGCALL", 60, vec!["\r\nOK\r\n"]);
        w.call_ctx = Some(CallContext {
            caller_id: "+919000000001".into(),
            sip_destination: String::new(),
            started_at: Utc::now(),
            direction: crate::store::calls::CallDirection::Inbound,
        });
        w.tick_liveness_probe();
        // Just the bare `AT`: one command, not the CSQ/QENG pair.
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!crate::webui::signal::all().contains_key("ec20-SIGCALL"));
    }

    #[test]
    fn polling_disabled_keeps_the_bare_probe() {
        let (mut w, seen) = worker_with_modem("ec20-SIGOFF", 0, vec!["\r\nOK\r\n"]);
        w.tick_liveness_probe();
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!crate::webui::signal::all().contains_key("ec20-SIGOFF"));
    }

    #[test]
    fn a_csq_99_exports_no_series() {
        let (mut w, _) = worker_with_modem(
            "ec20-SIG99",
            60,
            vec!["\r\n+CSQ: 99,99\r\n\r\nOK\r\n", "\r\nERROR\r\n"],
        );
        w.tick_liveness_probe();
        let out = prometheus::TextEncoder::new()
            .encode_to_string(&prometheus::gather())
            .unwrap();
        assert!(
            !out.lines()
                .any(|l| l.starts_with("gsm_sip_bridge_signal_rssi_dbm{module=\"ec20-SIG99\"")),
            "unknown signal is a gap, not a number"
        );
    }

    /// Mock stream for `AtCommander`, mirroring `at_commander`'s own private
    /// `MockStream`: reads from a fixed byte buffer, discards writes.
    struct MockAtStream {
        reader: std::io::Cursor<Vec<u8>>,
    }

    impl MockAtStream {
        fn new(response: &str) -> Self {
            Self {
                reader: std::io::Cursor::new(response.as_bytes().to_vec()),
            }
        }
    }

    impl std::io::Read for MockAtStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            std::io::Read::read(&mut self.reader, buf)
        }
    }

    impl std::io::Write for MockAtStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn mock_at(response: &str) -> AtCommander {
        AtCommander::from_stream(MockAtStream::new(response), Duration::from_secs(1))
    }

    /// A wedged port: every read times out, which is what a real serial port
    /// does when the modem stops answering (`read_response` turns this into an
    /// `Err`, unlike EOF — which it reports as an empty `Ok` response).
    struct MockAtTimeoutStream;

    impl std::io::Read for MockAtTimeoutStream {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "mock timeout",
            ))
        }
    }

    impl std::io::Write for MockAtTimeoutStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn mock_at_wedged() -> AtCommander {
        AtCommander::from_stream(MockAtTimeoutStream, Duration::from_secs(1))
    }

    fn mock_card(state: CardState) -> CardInstance {
        let mut card = CardInstance::new(
            "card0".to_string(),
            std::path::PathBuf::from("/dev/null"),
            "hw:0,0".to_string(),
            8,
        );
        card.state = state;
        card
    }

    // --- specs/034-alert-identity: SIM identity must follow the card ---

    /// The SIM-swap case behind the review findings: a card replaced under a
    /// live worker must report the number now in the modem, not the one cached
    /// when that worker opened.
    #[test]
    fn refresh_sim_phone_number_replaces_a_stale_cached_number() {
        let mut at = mock_at("+CNUM: \"\",\"+919000000002\",145\r\nOK\r\n");
        let mut cached = Some("+919000000001".to_string());
        refresh_sim_phone_number(&mut at, &mut cached);
        assert_eq!(cached.as_deref(), Some("+919000000002"));
    }

    /// A replacement SIM with no number provisioned must clear the cache, so
    /// the alert renders `unknown` instead of inheriting the removed card's
    /// number (FR-005: never a value that looks real but isn't).
    #[test]
    fn refresh_sim_phone_number_clears_when_the_new_sim_has_no_number() {
        let mut at = mock_at("OK\r\n");
        let mut cached = Some("+919000000001".to_string());
        refresh_sim_phone_number(&mut at, &mut cached);
        assert_eq!(cached, None);
    }

    /// A failed read says nothing about which SIM is present, so the last known
    /// number survives rather than being clobbered by a wedged AT channel.
    #[test]
    fn refresh_sim_phone_number_keeps_the_last_known_number_when_the_read_fails() {
        let mut at = mock_at_wedged();
        let mut cached = Some("+919000000001".to_string());
        refresh_sim_phone_number(&mut at, &mut cached);
        assert_eq!(cached.as_deref(), Some("+919000000001"));
    }

    #[test]
    fn apply_dial_cmd_dials_when_idle() {
        let mut at = mock_at("OK\r\n");
        let mut card = mock_card(CardState::Idle);
        assert!(apply_dial_cmd(&mut at, &mut card, "+15551234567").is_ok());
        assert_eq!(card.state, CardState::Answering);
    }

    #[test]
    fn apply_dial_cmd_refuses_when_not_idle() {
        let mut at = mock_at("OK\r\n");
        let mut card = mock_card(CardState::Bridged);
        let result = apply_dial_cmd(&mut at, &mut card, "+15551234567");
        assert_eq!(result, Err("line busy".to_string()));
        // State must not be perturbed by a refused attempt.
        assert_eq!(card.state, CardState::Bridged);
    }

    #[test]
    fn apply_dial_cmd_reports_at_failure_and_leaves_state_idle() {
        let mut at = mock_at("ERROR\r\n");
        let mut card = mock_card(CardState::Idle);
        assert!(apply_dial_cmd(&mut at, &mut card, "+15551234567").is_err());
        assert_eq!(card.state, CardState::Idle);
    }

    /// specs/022-discord-critical-alerts FR-003 (T010): duration-based, no
    /// sleeping — `last_success` is constructed in the past directly.
    #[test]
    fn at_worker_unresponsive_true_once_threshold_elapsed() {
        let now = std::time::Instant::now();
        let last_success = now - Duration::from_secs(61);
        assert!(at_worker_unresponsive(
            last_success,
            now,
            Duration::from_secs(60)
        ));
    }

    #[test]
    fn at_worker_unresponsive_false_within_threshold() {
        let now = std::time::Instant::now();
        let last_success = now - Duration::from_secs(30);
        assert!(!at_worker_unresponsive(
            last_success,
            now,
            Duration::from_secs(60)
        ));
    }

    #[test]
    fn at_worker_unresponsive_false_exactly_at_boundary_minus_one() {
        let now = std::time::Instant::now();
        let last_success = now - Duration::from_secs(59);
        assert!(!at_worker_unresponsive(
            last_success,
            now,
            Duration::from_secs(60)
        ));
    }

    /// specs/022-discord-critical-alerts FR-002/Clarifications Q4 (T027):
    /// `record_call_end`'s "missed" branch dispatches exactly one
    /// `MissedCall` `CriticalAlert`; "answered" dispatches none.
    #[test]
    fn record_call_end_missed_dispatches_one_missed_call_alert() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<BridgeEvent>();
        let (store_tx, _store_rx) = crossbeam_channel::unbounded::<StoreCommand>();
        let mut call_ctx = Some(CallContext {
            caller_id: "+911234567890".to_string(),
            sip_destination: String::new(),
            started_at: Utc::now(),
            direction: crate::store::calls::CallDirection::Inbound,
        });

        record_call_end("card0", &event_tx, &store_tx, &mut call_ctx, "missed", None);

        let event = event_rx.try_recv().expect("expected one dispatched event");
        let BridgeEvent::CriticalAlert(e) = event else {
            panic!("expected a CriticalAlert event");
        };
        assert_eq!(e.category, alerts::AlertCategory::MissedCall);
        assert_eq!(e.unit_id.as_deref(), Some("card0"));
        assert_eq!(e.kind, alerts::CriticalEventKind::Failure);
        assert!(e.description.contains("+911234567890"));
        assert!(
            event_rx.try_recv().is_err(),
            "must dispatch exactly one event"
        );
    }

    #[test]
    fn record_call_end_answered_dispatches_no_alert() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<BridgeEvent>();
        let (store_tx, _store_rx) = crossbeam_channel::unbounded::<StoreCommand>();
        let mut call_ctx = Some(CallContext {
            caller_id: "+911234567890".to_string(),
            sip_destination: String::new(),
            started_at: Utc::now(),
            direction: crate::store::calls::CallDirection::Inbound,
        });

        record_call_end(
            "card0",
            &event_tx,
            &store_tx,
            &mut call_ctx,
            "answered",
            None,
        );

        assert!(event_rx.try_recv().is_err(), "no alert for answered calls");
    }

    /// specs/025-outbound-calling review: an outbound call used to be
    /// invisible in call history because nothing ever populated
    /// `call_ctx` for it, so `record_call_end`'s `if let Some(ctx) =
    /// call_ctx.take()` silently did nothing on teardown regardless of
    /// what status it was called with. Exercises the full
    /// start-then-end lifecycle and checks a real `InsertCall` record
    /// comes out the other end.
    #[test]
    fn an_outbound_call_produces_a_real_call_history_record() {
        let (event_tx, _event_rx) = mpsc::unbounded_channel::<BridgeEvent>();
        let (store_tx, store_rx) = crossbeam_channel::unbounded::<StoreCommand>();
        let mut call_ctx: Option<CallContext> = None;
        assert!(call_ctx.is_none(), "no call in progress yet");

        record_call_start_outbound("card0", "+15551234567", &mut call_ctx);
        assert!(
            call_ctx.is_some(),
            "call_ctx must be populated once ATD is accepted"
        );

        record_call_end(
            "card0",
            &event_tx,
            &store_tx,
            &mut call_ctx,
            "answered",
            None,
        );
        assert!(
            call_ctx.is_none(),
            "record_call_end must take the context, same as the inbound path"
        );

        let StoreCommand::InsertCall(record) = store_rx
            .try_recv()
            .expect("a call record must have been sent to the store")
        else {
            panic!("expected InsertCall");
        };
        assert_eq!(record.caller_id, "+15551234567");
        assert_eq!(record.status, "answered");
        assert_eq!(record.module_id, "card0");
        assert_eq!(
            record.direction,
            crate::store::calls::CallDirection::Outbound,
            "the call history must say this call was placed, not received"
        );
    }

    /// specs/025-outbound-calling review: `ModuleCmd::Hangup` (sent when the
    /// SIP side rejects a call right after `ATD` was accepted) used to skip
    /// `record_call_end` entirely — the attempt vanished from history and
    /// `call_ctx` stayed `Some` with a stale context until the next
    /// `handle_ring` silently overwrote it. `ModuleCmd::Hangup`'s handler
    /// now calls `record_call_end(..., "failed")`, the same helper this
    /// records exercises directly.
    #[test]
    fn a_hung_up_outbound_attempt_produces_a_failed_call_history_record() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<BridgeEvent>();
        let (store_tx, store_rx) = crossbeam_channel::unbounded::<StoreCommand>();
        let mut call_ctx: Option<CallContext> = None;

        record_call_start_outbound("card0", "+15551234567", &mut call_ctx);
        record_call_end("card0", &event_tx, &store_tx, &mut call_ctx, "failed", None);

        assert!(
            call_ctx.is_none(),
            "the stale context must not survive to the next call"
        );
        assert!(
            event_rx.try_recv().is_err(),
            "a failed (not missed) outbound attempt must not alert"
        );

        let StoreCommand::InsertCall(record) = store_rx
            .try_recv()
            .expect("a call record must have been sent to the store")
        else {
            panic!("expected InsertCall");
        };
        assert_eq!(record.caller_id, "+15551234567");
        assert_eq!(record.status, "failed");
        assert_eq!(record.module_id, "card0");
    }

    // ---- gh#160: a hung-up line stays in Cleanup until the modem confirms ----

    const OK: &str = "\r\nOK\r\n";
    const ERROR: &str = "\r\nERROR\r\n";
    const CLCC_LISTED: &str = "\r\n+CLCC: 1,1,0,0,0,\"+919000000001\",145\r\n\r\nOK\r\n";

    fn bridged(w: &mut ModuleWorker) {
        w.card.state = CardState::Bridged;
        w.call_ctx = Some(CallContext {
            caller_id: "+919000000001".into(),
            sip_destination: String::new(),
            started_at: Utc::now(),
            direction: crate::store::calls::CallDirection::Inbound,
        });
    }

    fn drain(events: &mut Events) -> Vec<BridgeEvent> {
        std::iter::from_fn(|| events.try_recv().ok()).collect()
    }

    fn line_idles(events: &[BridgeEvent]) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, BridgeEvent::LineIdle { .. }))
            .count()
    }

    fn failure_alerts(events: &[BridgeEvent]) -> usize {
        events
            .iter()
            .filter(|e| {
                matches!(e, BridgeEvent::CriticalAlert(a)
                    if a.kind == alerts::CriticalEventKind::Failure)
            })
            .count()
    }

    fn cleanup_count(module: &str, outcome: &str) -> f64 {
        metrics::CS_HANGUP_CLEANUP_TOTAL
            .with_label_values(&[module, outcome])
            .get()
    }

    /// Forces the next `tick_cleanup` to poll, as if `ago` had passed since
    /// the hang-up.
    fn backdate_cleanup(w: &mut ModuleWorker, ago: Duration) {
        let wait = w.cleanup.as_mut().expect("in cleanup");
        wait.started = std::time::Instant::now() - ago;
        wait.last_poll = None;
    }

    #[test]
    fn a_sip_peer_hangup_confirmed_by_an_empty_call_list_frees_the_line() {
        let (mut w, seen, mut events, store) =
            worker_with_modem_and_channels("ec20-CLN1", 0, vec![OK, OK]);
        bridged(&mut w);

        w.apply_cmd(ModuleCmd::SipPeerHangup);

        assert_eq!(
            seen.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "CHUP, CLCC"
        );
        assert_eq!(w.card.state, CardState::Idle);
        assert!(w.cleanup.is_none());
        let events = drain(&mut events);
        assert_eq!(line_idles(&events), 1);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, BridgeEvent::Hangup { .. })),
            "the SIP side is already gone; a Hangup event would tear down another line's call"
        );
        let Ok(StoreCommand::InsertCall(record)) = store.try_recv() else {
            panic!("expected the call to be recorded");
        };
        assert_eq!(record.status, "answered");
        assert_eq!(cleanup_count("ec20-CLN1", "confirmed_clcc"), 1.0);
    }

    #[test]
    fn a_call_still_listed_holds_the_line_until_no_carrier() {
        let (mut w, _, mut events, store) =
            worker_with_modem_and_channels("ec20-CLN2", 0, vec![OK, CLCC_LISTED]);
        bridged(&mut w);

        w.apply_cmd(ModuleCmd::SipPeerHangup);
        assert_eq!(w.card.state, CardState::Cleanup);
        assert_eq!(line_idles(&drain(&mut events)), 0);

        w.dispatch_urc("NO CARRIER");

        assert_eq!(w.card.state, CardState::Idle);
        let events = drain(&mut events);
        assert_eq!(line_idles(&events), 1);
        assert!(!events
            .iter()
            .any(|e| matches!(e, BridgeEvent::Hangup { .. })));
        assert_eq!(store.try_iter().count(), 1, "the call is recorded once");
        assert_eq!(cleanup_count("ec20-CLN2", "confirmed_no_carrier"), 1.0);
    }

    #[test]
    fn a_call_that_clears_on_a_later_poll_frees_the_line() {
        let (mut w, _, mut events, _) =
            worker_with_modem_and_channels("ec20-CLN3", 0, vec![OK, CLCC_LISTED, OK]);
        bridged(&mut w);
        w.apply_cmd(ModuleCmd::SipPeerHangup);
        assert_eq!(w.card.state, CardState::Cleanup);

        assert!(!w.tick_cleanup(), "not due yet: no poll, no exit");
        assert_eq!(w.card.state, CardState::Cleanup);
        backdate_cleanup(&mut w, Duration::from_secs(1));
        assert!(!w.tick_cleanup());

        assert_eq!(w.card.state, CardState::Idle);
        assert_eq!(line_idles(&drain(&mut events)), 1);
    }

    #[test]
    fn a_failed_call_list_query_is_not_an_empty_list() {
        let (mut w, _, mut events, _) =
            worker_with_modem_and_channels("ec20-CLN4", 0, vec![OK, ERROR]);
        bridged(&mut w);

        w.apply_cmd(ModuleCmd::SipPeerHangup);

        assert_eq!(w.card.state, CardState::Cleanup);
        assert_eq!(line_idles(&drain(&mut events)), 0);
    }

    #[test]
    fn a_stuck_call_alerts_once_and_keeps_the_line_busy() {
        let (mut w, _, mut events, _) = worker_with_modem_and_channels(
            "ec20-CLN5",
            0,
            vec![OK, CLCC_LISTED, CLCC_LISTED, CLCC_LISTED, OK],
        );
        bridged(&mut w);
        w.apply_cmd(ModuleCmd::SipPeerHangup);

        backdate_cleanup(&mut w, CLEANUP_ALERT_AFTER + Duration::from_secs(1));
        assert!(!w.tick_cleanup());
        backdate_cleanup(&mut w, CLEANUP_ALERT_AFTER + Duration::from_secs(2));
        assert!(!w.tick_cleanup());

        assert_eq!(w.card.state, CardState::Cleanup);
        let stuck = drain(&mut events);
        assert_eq!(failure_alerts(&stuck), 1, "alerted once, not every poll");
        assert_eq!(line_idles(&stuck), 0);
        assert_eq!(cleanup_count("ec20-CLN5", "stuck_alerted"), 1.0);

        // The call finally clears: the line frees and the alert recovers.
        backdate_cleanup(&mut w, CLEANUP_ALERT_AFTER + Duration::from_secs(3));
        assert!(!w.tick_cleanup());
        assert_eq!(w.card.state, CardState::Idle);
        let cleared = drain(&mut events);
        assert_eq!(line_idles(&cleared), 1);
        assert!(cleared
            .iter()
            .any(|e| matches!(e, BridgeEvent::CriticalAlert(a)
            if a.kind == alerts::CriticalEventKind::Recovered)));
    }

    fn stuck_past_restart(w: &mut ModuleWorker) {
        w.card.state = CardState::Cleanup;
        w.cleanup = Some(CleanupWait {
            started: std::time::Instant::now() - CLEANUP_RESTART_AFTER,
            last_poll: None,
            alerted: true,
            refused_restarts: 0,
        });
    }

    #[test]
    fn a_call_stuck_past_the_restart_threshold_cycles_the_radio_and_exits() {
        // CLCC, AT+CFUN=0, AT+CFUN=1, then CLCC lists nothing.
        let (mut w, seen, mut events, _) =
            worker_with_modem_and_channels("ec20-CLN6", 0, vec![CLCC_LISTED, OK, OK, OK]);
        stuck_past_restart(&mut w);

        assert!(
            w.tick_cleanup(),
            "the worker must exit so the slot is re-initialised"
        );

        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 4);
        assert_eq!(failure_alerts(&drain(&mut events)), 1);
        assert_eq!(cleanup_count("ec20-CLN6", "radio_restart"), 1.0);
        assert_eq!(cleanup_count("ec20-CLN6", "module_reset"), 0.0);
    }

    /// Greptile on PR #175: `AT+CFUN=0` can be refused with the call still
    /// up. Exiting then would let the pool hand an occupied line to the next
    /// call, so the worker resets the module instead.
    #[test]
    fn a_call_that_survives_the_radio_cycle_resets_the_module_and_exits() {
        // CLCC, AT+CFUN=0 (refused), AT+CFUN=1, CLCC still listed, then
        // AT+CFUN=1,1 goes unanswered as a resetting modem's does.
        let (mut w, seen, _, _) = worker_with_modem_and_channels(
            "ec20-CLN10",
            0,
            vec![CLCC_LISTED, ERROR, OK, CLCC_LISTED],
        );
        stuck_past_restart(&mut w);

        assert!(w.tick_cleanup(), "a reset under way drops any call");

        // The mock modem stops answering after four replies, so the reset
        // command times out like a real reset: counted, not refused.
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 4);
        assert_eq!(cleanup_count("ec20-CLN10", "module_reset"), 1.0);
    }

    /// A failed call-list query after the radio cycle is not an empty list.
    #[test]
    fn an_unanswerable_call_list_after_the_radio_cycle_resets_the_module() {
        let (mut w, _, _, _) =
            worker_with_modem_and_channels("ec20-CLN11", 0, vec![CLCC_LISTED, OK, OK, ERROR, OK]);
        stuck_past_restart(&mut w);

        assert!(w.tick_cleanup());
        assert_eq!(cleanup_count("ec20-CLN11", "module_reset"), 1.0);
    }

    #[test]
    fn a_refused_reset_keeps_the_line_busy_and_retries_later() {
        let (mut w, seen, mut events, _) = worker_with_modem_and_channels(
            "ec20-CLN12",
            0,
            vec![CLCC_LISTED, OK, OK, CLCC_LISTED, ERROR],
        );
        stuck_past_restart(&mut w);

        assert!(!w.tick_cleanup(), "the call may still be up: stay");

        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 5);
        assert_eq!(w.card.state, CardState::Cleanup);
        assert_eq!(w.cleanup.as_ref().unwrap().refused_restarts, 1);
        assert_eq!(cleanup_count("ec20-CLN12", "reset_refused"), 1.0);
        let events = drain(&mut events);
        assert_eq!(failure_alerts(&events), 2);
        assert!(!events
            .iter()
            .any(|e| matches!(e, BridgeEvent::LineIdle { .. })));
        // The next attempt waits another full interval, not the next tick.
        assert!(!w.check_cleanup_deadlines());
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 5);
    }

    #[test]
    fn a_line_in_cleanup_takes_no_new_call() {
        let (mut w, seen, _, _) = worker_with_modem_and_channels("ec20-CLN7", 0, vec![]);
        w.card.state = CardState::Cleanup;

        w.dispatch_urc("RING");

        assert_eq!(w.card.state, CardState::Cleanup);
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 0);

        let mut at = mock_at("OK\r\n");
        let mut card = mock_card(CardState::Cleanup);
        assert!(apply_dial_cmd(&mut at, &mut card, "+919000000001").is_err());
        assert_eq!(card.state, CardState::Cleanup);
    }

    /// `NO CARRIER` won the race: the line is already idle, but the pool is
    /// still holding it busy and must be told.
    #[test]
    fn a_sip_peer_hangup_on_an_idle_line_confirms_at_once() {
        let (mut w, seen, mut events, _) = worker_with_modem_and_channels("ec20-CLN8", 0, vec![]);

        w.apply_cmd(ModuleCmd::SipPeerHangup);

        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(line_idles(&drain(&mut events)), 1);
    }

    /// A second hang-up while already waiting must not restart the clock, or
    /// repeated commands could postpone the alert and restart for ever.
    #[test]
    fn a_repeated_hang_up_keeps_the_original_cleanup_clock() {
        let (mut w, _, _, _) =
            worker_with_modem_and_channels("ec20-CLN9", 0, vec![OK, CLCC_LISTED, OK, CLCC_LISTED]);
        bridged(&mut w);
        w.apply_cmd(ModuleCmd::Hangup);
        backdate_cleanup(&mut w, Duration::from_secs(5));
        let started = w.cleanup.as_ref().unwrap().started;

        w.apply_cmd(ModuleCmd::Hangup);

        assert_eq!(w.cleanup.as_ref().unwrap().started, started);
        assert_eq!(w.card.state, CardState::Cleanup);
    }
}
