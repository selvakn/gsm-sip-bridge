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
use crate::store::calls::CallRecord;
use crate::store::StoreCommand;
use chrono::Utc;
use std::time::Duration;
use tokio::sync::mpsc;

pub(crate) struct CallContext {
    caller_id: String,
    sip_destination: String,
    started_at: chrono::DateTime<Utc>,
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
}

/// How often the idle-tick liveness probe (`AT`) is sent while otherwise
/// quiet — comfortably below any reasonable `at_worker_unresponsive_sec`
/// threshold (default 60s) so a real stall is caught within one threshold
/// window, not two.
const AT_WORKER_PROBE_INTERVAL: Duration = Duration::from_secs(15);

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
    /// specs/034-alert-identity: this card's phone number (read once at open
    /// via `AT+CNUM`), shown in the module's critical alerts. `None` when the
    /// SIM's EF_MSISDN is blank/unreadable ⇒ alerts render `unknown`.
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
        at.send_command("AT+CMGF=1").ok();
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
        })
    }

    fn run(mut self) -> Result<(), String> {
        loop {
            self.tick_liveness_probe();

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

            if pjsua_safe::is_sip_peer_disconnected()
                && (self.card.state == CardState::Bridged
                    || self.card.state == CardState::Answering)
            {
                tracing::info!(module = %self.module.id, "SIP peer disconnected, hanging up GSM");
                let _ = self.at.hangup();
                record_call_end(
                    &self.module.id,
                    &self.event_tx,
                    &self.store_tx,
                    &mut self.call_ctx,
                    "answered",
                    self.phone_number.as_deref(),
                );
                self.card.state = CardState::Idle;
                metrics::ACTIVE_CALLS
                    .with_label_values(&[&self.module.id, "cs"])
                    .set(0.0);
            }

            let trimmed = line.trim().to_string();
            if trimmed.is_empty() {
                continue;
            }

            tracing::trace!(module = %self.module.id, urc = %trimmed, "received");
            self.dispatch_urc(&trimmed);
        }
    }

    /// specs/022-discord-critical-alerts FR-003: send an `AT` no-op every
    /// `AT_WORKER_PROBE_INTERVAL` and alert once (not every tick) if the modem
    /// has been silent past the configured threshold.
    fn tick_liveness_probe(&mut self) {
        if self.last_at_probe.elapsed() < AT_WORKER_PROBE_INTERVAL {
            return;
        }
        self.last_at_probe = std::time::Instant::now();
        match self.at.send_command("AT") {
            Ok(AtResponse::Ok(_)) => {
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
            }
            _ => {
                if !self.at_worker_alerted
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
                tracing::info!(module = %self.module.id, "outbound: hanging up after SIP-side accept failed post-dial");
                let _ = self.at.hangup();
                self.card.state = CardState::Idle;
                metrics::ACTIVE_CALLS
                    .with_label_values(&[&self.module.id, "cs"])
                    .set(0.0);
                record_call_end(
                    &self.module.id,
                    &self.event_tx,
                    &self.store_tx,
                    &mut self.call_ctx,
                    "failed",
                    self.phone_number.as_deref(),
                );
                false
            }
        }
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

        let cmd = format!("AT+CMGR={idx}");
        match self.at.send_command(&cmd) {
            Ok(AtResponse::Ok(lines)) => {
                tracing::debug!(module = %self.module.id, index = idx, lines = ?lines, "SMS read");

                let (sender, body) = parse_sms_response(&lines);
                let received_at = Utc::now().to_rfc3339();

                let _ = self.event_tx.send(BridgeEvent::SmsReceived {
                    module_id: self.module.id.clone(),
                    sender,
                    body,
                    received_at,
                    // specs/034-alert-identity: attach this worker's own card
                    // number so the forward names the SIM that received the
                    // message, regardless of any concurrent slot churn.
                    phone_number: self.phone_number.clone(),
                });

                let del_cmd = format!("AT+CMGD={idx}");
                self.at.send_command(&del_cmd).ok();
            }
            Ok(AtResponse::Error(e)) => {
                tracing::warn!(module = %self.module.id, error = %e, "failed to read SMS");
            }
            Ok(AtResponse::CmeError(code, msg)) => {
                tracing::warn!(module = %self.module.id, code = code, error = %msg, "failed to read SMS");
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
        };
        if let Err(e) = store_tx.send(StoreCommand::InsertCall(record)) {
            tracing::error!(error = %e, "failed to send call record to store");
        }
    }
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

fn parse_sms_response(lines: &[String]) -> (String, String) {
    let mut sender = "unknown".to_string();
    let mut body = String::new();

    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("+CMGR:") {
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() >= 2 {
                sender = parts[1].trim().trim_matches('"').to_string();
            }
            if i + 1 < lines.len() {
                body = lines[i + 1..].join("\n");
            }
            break;
        }
    }

    (sender, body)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
