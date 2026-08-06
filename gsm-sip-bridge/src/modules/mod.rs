pub mod at_commander;
pub mod audio_pipeline;
pub mod beep;
pub mod card;
pub mod discovery;
pub mod pcsc_card;
pub mod pcsc_list;
pub mod scheduler;
pub mod usim;

use crate::alerts;
use crate::config::secret::Secret;
use crate::config::{AppConfig, ScheduledRestartConfig};
use crate::control::protocol::{ControlCmd, ControlResp, SlotInfo};
use crate::metrics;
use crate::modules::at_commander::{AtCommander, AtResponse, NetworkMode, NetworkType};
use crate::modules::card::{CardInstance, CardState};
use crate::modules::discovery::DiscoveredModule;
use crate::modules::scheduler::{
    AttemptType, CycleOutcome, CyclePhase, CycleState, Outcome, RestartProgress, SchedulerAction,
    SkipReason, SlotView,
};
use crate::sip::SipBridge;
use crate::sms;
use crate::sms::discord::DiscordClient;
use crate::sms::SmsHandler;
use crate::store::calls::CallRecord;
use crate::store::{StoreCommand, StoreHandle};
use chrono::Utc;
use pjsua_safe::Call;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinSet;

pub enum BridgeEvent {
    Ring {
        module_id: String,
        caller_id: String,
        audio_device: String,
    },
    Hangup {
        module_id: String,
    },
    SmsReceived {
        module_id: String,
        sender: String,
        body: String,
        received_at: String,
    },
    NetworkLost {
        module_id: String,
    },
    /// specs/022-discord-critical-alerts. Sent from the blocking per-module
    /// loop (which has no direct tokio `Handle`) so the async side can
    /// dispatch it via `Handle::current()`, mirroring `SmsReceived`.
    CriticalAlert(alerts::CriticalEvent),
}

pub type ControlCmdSender = mpsc::Sender<(ControlCmd, oneshot::Sender<ControlResp>)>;
pub type ControlCmdReceiver = mpsc::Receiver<(ControlCmd, oneshot::Sender<ControlResp>)>;

/// How hard to restart a card — shared by the scheduled-restart cycle
/// (`[scheduled_restart].restart_mode`) and manual `card restart` (always
/// `Full`, matching this crate's long-standing behavior).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartMode {
    /// `AT+CFUN=0` -> `AT+CFUN=1`: drops and re-acquires network
    /// registration without power-cycling the module or re-enumerating USB.
    Radio,
    /// `AT+CFUN=1,1`: a full module reset. Can move the card's ttyUSB path.
    Full,
}

/// `[scheduled_restart].restart_mode` -> `RestartMode`, pulled out of
/// `CardPool::apply_send_reboot` so the decision is testable without
/// constructing a whole `CardPool`. `build_scheduled_restart` (config/build.rs)
/// already rejects anything other than `"full"`/`"radio"` at load time (the
/// section is disabled instead), so this only ever sees one of the two.
fn scheduled_restart_mode(config: &ScheduledRestartConfig) -> RestartMode {
    if config.restart_mode == "radio" {
        RestartMode::Radio
    } else {
        RestartMode::Full
    }
}

enum ModuleCmd {
    SetMode(NetworkMode, oneshot::Sender<Result<NetworkMode, String>>),
    Reboot(RestartMode),
    /// Dial an outbound call on this line (specs/025-outbound-calling).
    /// `Ok(())` means the modem accepted `ATD`, not that the call was
    /// answered — see `AtCommander::dial`'s own doc comment.
    Dial(String, oneshot::Sender<Result<(), String>>),
    /// Hang up a call this line just dialed, fire-and-forget (matches
    /// `Reboot`'s no-response shape). Exists for the narrow case where
    /// `ATD` already succeeded but accepting the SIP-side leg failed
    /// afterward (specs/025-outbound-calling review) — without this, the
    /// modem stays on a real, live call while `SlotState` reports it idle
    /// and eligible for a second dial.
    Hangup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleState {
    Initializing,
    Ready,
    Recovering,
    GivenUp,
}

impl std::fmt::Display for LifecycleState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LifecycleState::Initializing => write!(f, "Initializing"),
            LifecycleState::Ready => write!(f, "Ready"),
            LifecycleState::Recovering => write!(f, "Recovering"),
            LifecycleState::GivenUp => write!(f, "GivenUp"),
        }
    }
}

struct SlotState {
    slot: u32,
    module: DiscoveredModule,
    imei: String,
    phone_number: String,
    network_type: NetworkType,
    network_mode: Option<NetworkMode>,
    lifecycle: LifecycleState,
    retry_count: u32,
    next_retry_at: Option<tokio::time::Instant>,
    cmd_tx: Option<crossbeam_channel::Sender<ModuleCmd>>,
    has_active_call: bool,
}

impl SlotState {
    fn info(&self) -> SlotInfo {
        SlotInfo {
            slot: self.slot,
            state: self.lifecycle.to_string(),
            phone: if self.phone_number.is_empty() {
                "Unknown".to_string()
            } else {
                self.phone_number.clone()
            },
            network: self.network_type.to_string(),
        }
    }
}

pub fn backoff_delay(attempt: u32, initial_sec: u64, max_sec: u64) -> Duration {
    let shift = attempt.min(30);
    let secs = initial_sec.saturating_mul(1u64 << shift);
    Duration::from_secs(secs.min(max_sec))
}

/// specs/022-discord-critical-alerts (Greptile P1 fix): finds a stale
/// `GivenUp` slot for `module_id`, if one exists under a *different* key —
/// rediscovery (a USB rescan or a fresh startup scan) creates a brand new
/// slot rather than reusing the old one, so without this the old slot would
/// otherwise sit there forever with no recovery notification ever sent and
/// its `CRITICAL_EVENT_ACTIVE` gauge stuck at 1.
fn find_given_up_slot(slots: &HashMap<u32, SlotState>, module_id: &str) -> Option<u32> {
    slots
        .iter()
        .find(|(_, s)| s.lifecycle == LifecycleState::GivenUp && s.module.id == module_id)
        .map(|(k, _)| *k)
}

pub struct CardPool {
    config: AppConfig,
    store: StoreHandle,
    sip_bridge: SipBridge,
    sms_handler: SmsHandler,
    discord_client: Option<DiscordClient>,
    /// specs/022-discord-critical-alerts. Separate from `discord_client`
    /// (SMS's own dedicated client): alert categories resolve their own
    /// webhook per event (`alerts::dispatch`), so this client is always
    /// constructed and never gated on any one category being enabled.
    alerts_client: Option<DiscordClient>,
    cron_schedule: Option<cron::Schedule>,
    cycle: Option<CycleState>,
    next_scheduled_at: Option<tokio::time::Instant>,
    last_fired_tick: Option<chrono::DateTime<chrono::Local>>,
}

/// `SlotView` implementation backed by the pool's slot map. Built fresh on
/// each scheduler tick because the borrow it holds is short-lived.
struct PoolSlotView<'a> {
    slots: &'a HashMap<u32, SlotState>,
}

impl<'a> SlotView for PoolSlotView<'a> {
    fn is_ready(&self, slot: u32) -> bool {
        self.slots
            .get(&slot)
            .is_some_and(|s| s.lifecycle == LifecycleState::Ready)
    }

    fn non_ready_skip_reason(&self, slot: u32) -> Option<String> {
        match self.slots.get(&slot) {
            None => Some("slot not present".to_string()),
            Some(s) if s.lifecycle == LifecycleState::Ready => None,
            Some(s) => Some(s.lifecycle.to_string()),
        }
    }

    fn has_active_call(&self, slot: u32) -> bool {
        self.slots.get(&slot).is_some_and(|s| s.has_active_call)
    }

    fn restart_progress(&self, slot: u32) -> RestartProgress {
        match self.slots.get(&slot) {
            None => RestartProgress::Gone,
            Some(s) => match s.lifecycle {
                LifecycleState::Ready => RestartProgress::Succeeded,
                LifecycleState::GivenUp => RestartProgress::Failed,
                LifecycleState::Initializing | LifecycleState::Recovering => {
                    RestartProgress::InFlight
                }
            },
        }
    }
}

/// How long a restarted slot stays `Recovering` before recovery is allowed
/// to retry it, when the restart command went to a live worker (`cmd_tx`).
/// Just a starting estimate — the worker's eventual exit (picked up via
/// `CardPool::run`'s `JoinSet`) recomputes `next_retry_at` for real once the
/// restart actually finishes, so this only matters for the brief window
/// before that.
const RECOVERY_RETRY_DELAY: Duration = Duration::from_secs(10);

/// Same idea, but for `apply_send_reboot`'s no-worker fallback: that restart
/// runs detached (`tokio::task::spawn_blocking`, fire-and-forget) with
/// nothing to recompute `next_retry_at` once it finishes, unlike the
/// `cmd_tx`/`JoinSet` path above. Recovery reopening the same serial port
/// before the detached restart releases it would interleave AT commands or
/// fail outright (Greptile review, PR #30), so this waits out the slowest
/// case regardless of which restart mode ran: `AtCommander::open` plus
/// `radio_restart`'s `AT+CFUN=0` (~5s worst-case AT timeout) -> `sleep(4s)`
/// -> `AT+CFUN=1` (~5s worst-case) is ~14s; this adds real margin on top.
const RADIO_RESTART_FALLBACK_RETRY_DELAY: Duration = Duration::from_secs(20);

impl CardPool {
    pub fn new(
        config: AppConfig,
        store: StoreHandle,
        sip_bridge: SipBridge,
        sms_handler: SmsHandler,
    ) -> Self {
        let discord_client = if sms_handler.has_webhook() {
            let url = config.sms.discord_webhook_url.clone();
            match DiscordClient::new(url) {
                Ok(client) => Some(client),
                Err(e) => {
                    tracing::error!(error = %e, "failed to create Discord client");
                    None
                }
            }
        } else {
            None
        };

        let alerts_client = match DiscordClient::new(Secret::new(String::new())) {
            Ok(client) => Some(client),
            Err(e) => {
                tracing::error!(error = %e, "failed to create critical-alerts Discord client");
                None
            }
        };

        let cron_schedule = if config.scheduled_restart.enabled {
            match scheduler::parse_cron_5field(&config.scheduled_restart.cron) {
                Ok(s) => {
                    tracing::info!(
                        cron = %config.scheduled_restart.cron,
                        start_jitter_seconds = config.scheduled_restart.start_jitter_seconds,
                        inter_card_gap_seconds = config.scheduled_restart.inter_card_gap_seconds,
                        inter_card_gap_jitter_seconds =
                            config.scheduled_restart.inter_card_gap_jitter_seconds,
                        "scheduled_restart enabled"
                    );
                    Some(s)
                }
                Err(e) => {
                    tracing::warn!(
                        cron = %config.scheduled_restart.cron,
                        error = %e,
                        "scheduled_restart disabled: cron expression failed to parse"
                    );
                    None
                }
            }
        } else {
            tracing::info!("scheduled_restart disabled (enabled = false in config)");
            None
        };

        Self {
            config,
            store,
            sip_bridge,
            sms_handler,
            discord_client,
            alerts_client,
            cron_schedule,
            cycle: None,
            next_scheduled_at: None,
            last_fired_tick: None,
        }
    }

    /// Compute the next jittered cycle start instant from the cron schedule.
    /// Returns `None` if the schedule is disabled or has no future occurrence.
    fn recompute_next_scheduled_at(&mut self) {
        let Some(schedule) = self.cron_schedule.as_ref() else {
            self.next_scheduled_at = None;
            return;
        };
        let now_local = chrono::Local::now();
        // Use the last natural cron tick as the lower bound so we never re-fire
        // the same occurrence regardless of jitter direction.  On the very first
        // call `last_fired_tick` is None, so we fall back to `now_local`.
        let after = self.last_fired_tick.unwrap_or(now_local);
        let Some(next_tick) = schedule.after(&after).next() else {
            tracing::warn!("scheduled_restart has no future cron occurrence; disabling scheduler");
            self.cron_schedule = None;
            self.next_scheduled_at = None;
            return;
        };
        // Persist the natural tick immediately so the next recompute call always
        // advances past this occurrence, even if the jittered start lands earlier.
        self.last_fired_tick = Some(next_tick);
        let mut rng = rand::rng();
        let jitter =
            scheduler::jitter_offset(&mut rng, self.config.scheduled_restart.start_jitter_seconds);
        let delta_sec = (next_tick - now_local).num_seconds() + jitter;
        let now_instant = tokio::time::Instant::now();
        let target = if delta_sec <= 0 {
            now_instant
        } else {
            now_instant + Duration::from_secs(delta_sec as u64)
        };
        self.next_scheduled_at = Some(target);
        tracing::info!(
            next_cron_tick = %next_tick,
            jittered_delta_seconds = delta_sec,
            "scheduled_restart next cycle armed"
        );
    }

    pub async fn run(
        mut self,
        single_card: Option<(PathBuf, String)>,
        mut shutdown_rx: broadcast::Receiver<()>,
        mut control_rx: ControlCmdReceiver,
    ) {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<BridgeEvent>();

        if let Err(e) = self.sip_bridge.register() {
            tracing::error!(error = %e, "SIP registration failed — calls will not be bridged");
        }

        let modules = match single_card {
            Some((serial, audio)) => {
                let id = discovery::derive_module_id(
                    serial
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .as_ref(),
                );
                vec![DiscoveredModule {
                    id,
                    serial_port: serial,
                    audio_device: audio,
                    usb_serial: String::new(),
                }]
            }
            // A card assigned to the host-side cellular service belongs to it
            // alone (FR-034) — probing a port another subsystem is
            // mid-transaction on is the documented "claimed by both" hazard.
            None => match discovery::scan_modules_excluding_cards(
                &discovery::volte_claimed_ports(&self.config.volte),
                &discovery::volte_claimed_card_ids(&self.config.volte),
            ) {
                Ok(m) => m,
                Err(e) => {
                    tracing::error!(error = %e, "module discovery failed");
                    Vec::new()
                }
            },
        };

        if modules.is_empty() {
            tracing::warn!("no EC20 modules found — waiting for retry or shutdown");
        }

        let mut slots: HashMap<u32, SlotState> = HashMap::new();
        let mut tasks: JoinSet<(u32, String)> = JoinSet::new();
        let resilience = self.config.resilience.clone();
        let ring_capacity = self.config.audio.settings.ring_capacity;

        for module in modules {
            match self.try_init_module(&module) {
                Ok((slot, imei, phone, net_type, net_mode)) => {
                    tracing::info!(
                        module = %module.id,
                        slot = slot,
                        imei = %imei,
                        phone = %phone,
                        network = %net_type,
                        "module initialized"
                    );
                    metrics::MODULE_INIT_TOTAL
                        .with_label_values(&[&module.id, "success", ""])
                        .inc();

                    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<ModuleCmd>();
                    let state = SlotState {
                        slot,
                        module: module.clone(),
                        imei,
                        phone_number: phone,
                        network_type: net_type,
                        network_mode: net_mode,
                        lifecycle: LifecycleState::Ready,
                        retry_count: 0,
                        next_retry_at: None,
                        cmd_tx: Some(cmd_tx),
                        has_active_call: false,
                    };
                    let store_tx = self.store.sender();
                    let sms_enabled = self.sms_handler.is_enabled();
                    let module_clone = module.clone();
                    let evt_tx = event_tx.clone();
                    let audio_init = ModuleAudioInit {
                        rx_gain: self.config.modem_audio.rx_gain,
                        eec_mode: self.config.modem_audio.eec_mode,
                    };
                    let at_worker_threshold = Duration::from_secs(
                        self.config
                            .alerts
                            .module_lifecycle_thresholds
                            .at_worker_unresponsive_sec,
                    );
                    tasks.spawn_blocking(move || {
                        let sid = slot;
                        if let Err(e) = run_module_loop(
                            module_clone.clone(),
                            store_tx,
                            sms_enabled,
                            evt_tx,
                            cmd_rx,
                            ring_capacity,
                            audio_init,
                            at_worker_threshold,
                        ) {
                            tracing::error!(module = %module_clone.id, error = %e, "module loop exited with error");
                        }
                        (sid, module_clone.id)
                    });
                    slots.insert(slot, state);
                }
                Err(e) => {
                    tracing::warn!(module = %module.id, error = %e, "module init failed, will retry");
                    metrics::MODULE_INIT_TOTAL
                        .with_label_values(&[&module.id, "failure", &e])
                        .inc();
                    // Assign a temporary slot for tracking
                    let slot = slots.len() as u32;
                    slots.insert(
                        slot,
                        SlotState {
                            slot,
                            module,
                            imei: String::new(),
                            phone_number: String::new(),
                            network_type: NetworkType::Unknown,
                            network_mode: None,
                            lifecycle: LifecycleState::Initializing,
                            retry_count: 0,
                            next_retry_at: Some(
                                tokio::time::Instant::now()
                                    + backoff_delay(
                                        0,
                                        resilience.initial_backoff_sec,
                                        resilience.max_backoff_sec,
                                    ),
                            ),
                            cmd_tx: None,
                            has_active_call: false,
                        },
                    );
                }
            }
        }

        // Print startup diagnostics
        self.print_diagnostics(&slots);

        metrics::MODULES_ACTIVE.set(
            slots
                .values()
                .filter(|s| s.lifecycle == LifecycleState::Ready)
                .count() as f64,
        );
        metrics::MODULES_FAILED.set(
            slots
                .values()
                .filter(|s| s.lifecycle != LifecycleState::Ready)
                .count() as f64,
        );

        tracing::info!(
            active = slots
                .values()
                .filter(|s| s.lifecycle == LifecycleState::Ready)
                .count(),
            recovering = slots
                .values()
                .filter(|s| s.lifecycle != LifecycleState::Ready)
                .count(),
            "card pool running"
        );

        // USB rescan for hotplug reconnect (every 60 s — hot-plug is rare)
        let mut rescan_deadline = tokio::time::Instant::now() + Duration::from_secs(60);

        // Outbound calling (specs/025-outbound-calling): a short, dedicated
        // poll for `Endpoint::poll_incoming_call`, independent of the
        // retry/rescan wakeup computation above — those deadlines can be an
        // hour or more away, far too infrequent for a caller waiting on a
        // SIP response. The `if` guard on its `select!` arm means this does
        // nothing at all when the feature is disabled (FR-017).
        let mut outbound_poll = tokio::time::interval(Duration::from_millis(200));
        outbound_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        self.recompute_next_scheduled_at();

        loop {
            // Compute next retry deadline across all recovering/initializing slots
            let next_slot_retry = slots
                .values()
                .filter_map(|s| s.next_retry_at)
                .min()
                .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));

            let mut earliest_wakeup = next_slot_retry.min(rescan_deadline);
            if let Some(sched) = self.next_scheduled_at {
                earliest_wakeup = earliest_wakeup.min(sched);
            }
            if let Some(cycle) = self.cycle.as_ref() {
                earliest_wakeup = earliest_wakeup.min(cycle.next_action_at);
            }

            tokio::select! {
                _ = shutdown_rx.recv() => {
                    tracing::info!("card pool shutting down");
                    break;
                }
                Some(event) = event_rx.recv() => {
                    self.handle_bridge_event(event, &mut slots);
                }
                Some(result) = tasks.join_next() => {
                    match result {
                        Ok((slot, module_id)) => {
                            tracing::warn!(module = %module_id, slot = slot, "module worker exited, scheduling retry");
                            if let Some(state) = slots.get_mut(&slot) {
                                state.lifecycle = LifecycleState::Recovering;
                                state.cmd_tx = None;
                                let delay = backoff_delay(
                                    state.retry_count,
                                    resilience.initial_backoff_sec,
                                    resilience.max_backoff_sec,
                                );
                                state.next_retry_at = Some(tokio::time::Instant::now() + delay);
                                metrics::MODULES_ACTIVE.set(slots.values().filter(|s| s.lifecycle == LifecycleState::Ready).count() as f64);
                                metrics::MODULES_FAILED.set(slots.values().filter(|s| s.lifecycle != LifecycleState::Ready).count() as f64);
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "module worker panicked");
                        }
                    }
                }
                Some((cmd, reply)) = control_rx.recv() => {
                    self.handle_control_cmd(cmd, reply, &mut slots, &resilience).await;
                }
                _ = outbound_poll.tick(), if self.config.outbound.enabled => {
                    if let Some((call, destination)) = self.sip_bridge.poll_outbound_request() {
                        self.handle_outbound_request(call, destination, &mut slots).await;
                    }
                }
                _ = tokio::time::sleep_until(earliest_wakeup) => {
                    let now = tokio::time::Instant::now();

                    // Retry recovering/initializing slots whose backoff has expired
                    let slot_ids: Vec<u32> = slots.keys().copied().collect();
                    for slot in slot_ids {
                        let should_retry = {
                            let s = &slots[&slot];
                            s.lifecycle != LifecycleState::Ready
                                && s.lifecycle != LifecycleState::GivenUp
                                && s.next_retry_at.is_some_and(|t| t <= now)
                        };
                        if !should_retry {
                            continue;
                        }

                        let module = slots[&slot].module.clone();
                        metrics::MODULE_RETRIES_TOTAL.with_label_values(&[&module.id]).inc();

                        match self.try_init_module(&module) {
                            Ok((new_slot, imei, phone, net_type, net_mode)) => {
                                tracing::info!(module = %module.id, slot = new_slot, "module recovered on retry");
                                metrics::MODULE_INIT_TOTAL
                                    .with_label_values(&[&module.id, "success", ""])
                                    .inc();

                                let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<ModuleCmd>();
                                let store_tx = self.store.sender();
                                let sms_enabled = self.sms_handler.is_enabled();
                                let module_clone = module.clone();
                                let evt_tx = event_tx.clone();
                                let audio_init = ModuleAudioInit {
                                    rx_gain: self.config.modem_audio.rx_gain,
                                    eec_mode: self.config.modem_audio.eec_mode,
                                };
                                let at_worker_threshold = Duration::from_secs(
                                    self.config
                                        .alerts
                                        .module_lifecycle_thresholds
                                        .at_worker_unresponsive_sec,
                                );
                                tasks.spawn_blocking(move || {
                                    if let Err(e) = run_module_loop(
                                        module_clone.clone(),
                                        store_tx,
                                        sms_enabled,
                                        evt_tx,
                                        cmd_rx,
                                        ring_capacity,
                                        audio_init,
                                        at_worker_threshold,
                                    ) {
                                        tracing::error!(module = %module_clone.id, error = %e, "module loop exited");
                                    }
                                    (new_slot, module_clone.id)
                                });

                                if let Some(state) = slots.get_mut(&slot) {
                                    state.imei = imei;
                                    state.phone_number = phone;
                                    state.network_type = net_type;
                                    state.network_mode = net_mode;
                                    state.lifecycle = LifecycleState::Ready;
                                    state.retry_count = 0;
                                    state.next_retry_at = None;
                                    state.cmd_tx = Some(cmd_tx);
                                }

                                // specs/022-discord-critical-alerts (Greptile
                                // P1 follow-up: "GivenUp cleanup runs only on
                                // immediate rescan success and is skipped
                                // when initialization succeeds through the
                                // retry loop"). This slot's own retry never
                                // touches a `GivenUp` slot directly (excluded
                                // by `should_retry` above), but a *different*
                                // slot for the same module_id can be
                                // `GivenUp` — e.g. a rescan's failed init
                                // created a fresh `Initializing` slot for a
                                // module that already had a `GivenUp` slot
                                // from an earlier incident, and it's that
                                // fresh slot recovering here.
                                self.clear_stale_given_up(&mut slots, &module.id);
                            }
                            Err(e) => {
                                tracing::debug!(module = %module.id, error = %e, "retry failed");
                                if let Some(state) = slots.get_mut(&slot) {
                                    state.retry_count += 1;
                                    if state.retry_count >= resilience.max_retries {
                                        tracing::error!(
                                            module = %module.id,
                                            slot = slot,
                                            retries = state.retry_count,
                                            "module gave up after max retries"
                                        );
                                        state.lifecycle = LifecycleState::GivenUp;
                                        state.next_retry_at = None;
                                        if let Some(client) = self.alerts_client.clone() {
                                            let config = self.config.alerts.clone();
                                            let event = alerts::CriticalEvent {
                                                category: alerts::AlertCategory::ModuleLifecycle,
                                                unit_id: Some(module.id.clone()),
                                                description: format!(
                                                    "module failed to initialize after {} retries: {e}",
                                                    state.retry_count
                                                ),
                                                at: Utc::now(),
                                                kind: alerts::CriticalEventKind::Failure,
                                            };
                                            tokio::spawn(async move {
                                                alerts::dispatch(&client, &config, event).await
                                            });
                                        }
                                    } else {
                                        let delay = backoff_delay(
                                            state.retry_count,
                                            resilience.initial_backoff_sec,
                                            resilience.max_backoff_sec,
                                        );
                                        state.next_retry_at = Some(tokio::time::Instant::now() + delay);
                                    }
                                }
                            }
                        }
                    }

                    // USB rescan for new modules
                    if now >= rescan_deadline {
                        self.rescan_new_modules(&mut slots, &mut tasks, &event_tx, ring_capacity);
                        rescan_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
                    }

                    // Scheduled restart: start cycle if armed, or advance running cycle.
                    self.advance_scheduler(&mut slots, now);

                    metrics::MODULES_ACTIVE.set(slots.values().filter(|s| s.lifecycle == LifecycleState::Ready).count() as f64);
                    metrics::MODULES_FAILED.set(slots.values().filter(|s| s.lifecycle != LifecycleState::Ready).count() as f64);
                }
            }
        }

        self.sip_bridge.unregister();
        tasks.shutdown().await;
        self.store.shutdown();
    }

    fn try_init_module(
        &self,
        module: &DiscoveredModule,
    ) -> Result<(u32, String, String, NetworkType, Option<NetworkMode>), String> {
        if module.serial_port.as_os_str().is_empty() {
            return Err("serial port path not resolved".into());
        }
        let mut at = AtCommander::open(&module.serial_port).map_err(|e| e.to_string())?;
        match at.send_command("AT") {
            Ok(AtResponse::Ok(_)) => {}
            Ok(AtResponse::Error(e)) => return Err(format!("AT probe returned ERROR: {e}")),
            Ok(AtResponse::CmeError(code, msg)) => {
                return Err(format!("AT probe returned +CME ERROR {code}: {msg}"))
            }
            Err(e) => return Err(format!("AT probe failed: {e}")),
        }

        let imei = at.query_imei().unwrap_or_else(|_| "Unknown".into());

        // Look up or assign slot in DB
        let slot = match self.store.lookup_slot(&imei) {
            Ok(Some(s)) => s,
            Ok(None) => self
                .store
                .assign_slot_sync(&imei, &module.usb_serial)
                .map_err(|e| e.to_string())?,
            Err(e) => return Err(format!("DB slot lookup failed: {e}")),
        };

        // Persist the slot mapping (idempotent)
        let _ = self.store.sender().send(StoreCommand::UpsertSlot {
            imei: imei.clone(),
            usb_serial: module.usb_serial.clone(),
        });

        let phone = at.query_phone_number().unwrap_or_else(|_| "Unknown".into());
        let net_type = at.query_network_type().unwrap_or(NetworkType::Unknown);

        // Apply stored network mode preference
        let stored_mode = self.store.get_mode_pref(slot).ok().flatten();
        if let Some(mode) = stored_mode {
            let _ = at.set_network_mode(mode);
        }

        // Enable network registration URC for loss detection
        at.send_command("AT+CREG=1").ok();
        at.send_command("AT+CEREG=1").ok();

        Ok((slot, imei, phone, net_type, stored_mode))
    }

    fn print_diagnostics(&self, slots: &HashMap<u32, SlotState>) {
        if slots.is_empty() {
            return;
        }
        let mut sorted: Vec<&SlotState> = slots.values().collect();
        sorted.sort_by_key(|s| s.slot);
        for state in sorted {
            let phone = if state.phone_number.is_empty() {
                "Unknown"
            } else {
                &state.phone_number
            };
            tracing::info!(
                slot = state.slot,
                phone_number = phone,
                network_type = %state.network_type,
                imei = %state.imei,
                "[Slot {}] {}  {}",
                state.slot,
                phone,
                state.network_type,
            );
        }
    }

    /// specs/022-discord-critical-alerts (Greptile P1 fix, and its
    /// follow-up: the same cleanup is needed wherever a module reaches
    /// `Ready` again — both `rescan_new_modules`'s "new module" success arm
    /// and the retry loop's success arm call this). If `module_id` has a
    /// stale `GivenUp` slot under a different key, remove it and fire a
    /// `ModuleLifecycle` `Recovered` event — otherwise that slot (and its
    /// `CRITICAL_EVENT_ACTIVE` gauge) would sit there forever, alongside the
    /// new healthy one, with no recovery notification ever sent.
    fn clear_stale_given_up(&self, slots: &mut HashMap<u32, SlotState>, module_id: &str) {
        let Some(stale_slot) = find_given_up_slot(slots, module_id) else {
            return;
        };
        slots.remove(&stale_slot);
        let Some(client) = self.alerts_client.clone() else {
            return;
        };
        let config = self.config.alerts.clone();
        let event = alerts::CriticalEvent {
            category: alerts::AlertCategory::ModuleLifecycle,
            unit_id: Some(module_id.to_string()),
            description: "module recovered after previously giving up".to_string(),
            at: Utc::now(),
            kind: alerts::CriticalEventKind::Recovered,
        };
        tokio::spawn(async move { alerts::dispatch(&client, &config, event).await });
    }

    fn rescan_new_modules(
        &self,
        slots: &mut HashMap<u32, SlotState>,
        tasks: &mut JoinSet<(u32, String)>,
        event_tx: &mpsc::UnboundedSender<BridgeEvent>,
        ring_capacity: usize,
    ) {
        // specs/022-discord-critical-alerts (Greptile P1 fix): a `GivenUp`
        // slot's serial port must NOT count as "known" — otherwise this scan
        // treats the module as already-seen forever and never attempts it
        // again, even if the underlying hardware issue has since cleared.
        let known_serials: std::collections::HashSet<PathBuf> = slots
            .values()
            .filter(|s| s.lifecycle != LifecycleState::GivenUp)
            .map(|s| s.module.serial_port.clone())
            .collect();

        let volte_ports = discovery::volte_claimed_ports(&self.config.volte);
        let volte_cards = discovery::volte_claimed_card_ids(&self.config.volte);
        let new_modules = match discovery::scan_modules_excluding_cards(&volte_ports, &volte_cards)
        {
            Ok(m) => m
                .into_iter()
                .filter(|m| !known_serials.contains(&m.serial_port))
                .collect::<Vec<_>>(),
            Err(e) => {
                tracing::debug!(error = %e, "USB rescan failed");
                return;
            }
        };

        let resilience = &self.config.resilience;
        for module in new_modules {
            tracing::info!(module = %module.id, "new module detected, initializing");
            match self.try_init_module(&module) {
                Ok((slot, imei, phone, net_type, net_mode)) => {
                    // specs/022-discord-critical-alerts (Greptile P1 fix):
                    // this module_id may already have a stale `GivenUp` slot
                    // from an earlier incident (a different slot key — the
                    // scan above now revisits it once it stops counting as
                    // "known"). Rediscovering it here is exactly a recovery:
                    // clear the stale slot and its active-incident gauge
                    // rather than leaving a dead entry sitting alongside the
                    // new, healthy one forever.
                    self.clear_stale_given_up(slots, &module.id);

                    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<ModuleCmd>();
                    let store_tx = self.store.sender();
                    let sms_enabled = self.sms_handler.is_enabled();
                    let module_clone = module.clone();
                    let evt_tx = event_tx.clone();
                    let audio_init = ModuleAudioInit {
                        rx_gain: self.config.modem_audio.rx_gain,
                        eec_mode: self.config.modem_audio.eec_mode,
                    };
                    let at_worker_threshold = Duration::from_secs(
                        self.config
                            .alerts
                            .module_lifecycle_thresholds
                            .at_worker_unresponsive_sec,
                    );
                    tasks.spawn_blocking(move || {
                        if let Err(e) = run_module_loop(
                            module_clone.clone(),
                            store_tx,
                            sms_enabled,
                            evt_tx,
                            cmd_rx,
                            ring_capacity,
                            audio_init,
                            at_worker_threshold,
                        ) {
                            tracing::error!(module = %module_clone.id, error = %e, "module loop exited");
                        }
                        (slot, module_clone.id)
                    });
                    slots.insert(
                        slot,
                        SlotState {
                            slot,
                            module,
                            imei,
                            phone_number: phone,
                            network_type: net_type,
                            network_mode: net_mode,
                            lifecycle: LifecycleState::Ready,
                            retry_count: 0,
                            next_retry_at: None,
                            cmd_tx: Some(cmd_tx),
                            has_active_call: false,
                        },
                    );
                }
                Err(e) => {
                    tracing::warn!(module = %module.id, error = %e, "new module init failed");
                    let slot = slots.len() as u32;
                    slots.insert(
                        slot,
                        SlotState {
                            slot,
                            module,
                            imei: String::new(),
                            phone_number: String::new(),
                            network_type: NetworkType::Unknown,
                            network_mode: None,
                            lifecycle: LifecycleState::Initializing,
                            retry_count: 0,
                            next_retry_at: Some(
                                tokio::time::Instant::now()
                                    + backoff_delay(
                                        0,
                                        resilience.initial_backoff_sec,
                                        resilience.max_backoff_sec,
                                    ),
                            ),
                            cmd_tx: None,
                            has_active_call: false,
                        },
                    );
                }
            }
        }
    }

    fn advance_scheduler(
        &mut self,
        slots: &mut HashMap<u32, SlotState>,
        now: tokio::time::Instant,
    ) {
        // 1) If no cycle is active and the scheduled instant has arrived, start one.
        if self.cycle.is_none() {
            let Some(scheduled) = self.next_scheduled_at else {
                return;
            };
            if now < scheduled {
                return;
            }
            self.start_cycle(slots, now);
            return;
        }

        // 2) If a cycle is active and its next-action deadline has arrived, tick it.
        let Some(cycle) = self.cycle.as_mut() else {
            return;
        };
        if now < cycle.next_action_at {
            return;
        }

        let view = PoolSlotView { slots };
        let mut rng = rand::rng();
        let actions = scheduler::tick_scheduler(
            cycle,
            &view,
            now,
            &mut rng,
            self.config.scheduled_restart.inter_card_gap_seconds,
            self.config.scheduled_restart.inter_card_gap_jitter_seconds,
        );

        let mut complete = false;
        for action in actions {
            match action {
                SchedulerAction::SendReboot { slot } => {
                    self.apply_send_reboot(slots, slot, now);
                }
                SchedulerAction::RecordOutcome { slot, outcome } => {
                    self.record_outcome(slot, &outcome);
                }
                SchedulerAction::Complete => {
                    complete = true;
                }
            }
        }

        if complete {
            self.complete_cycle();
        }
    }

    fn start_cycle(&mut self, slots: &HashMap<u32, SlotState>, now: tokio::time::Instant) {
        // FR-014 guard belongs here too: if a previous cycle is somehow still
        // active (shouldn't be — we cleared next_scheduled_at on cycle start)
        // bail out.
        if self.cycle.is_some() {
            tracing::warn!(
                "scheduled_restart cycle-trigger-dropped: a previous cycle is still active"
            );
            return;
        }

        let cron_tick = chrono::Local::now();
        let id = cron_tick.timestamp().max(0) as u64;

        let mut as_vec: Vec<u32> = slots.keys().copied().collect();
        as_vec.sort_unstable();
        let pending: VecDeque<u32> = as_vec.iter().copied().collect();

        tracing::info!(
            cycle_id = id,
            cron_tick = %cron_tick,
            actual_start = %chrono::Local::now(),
            n_slots = pending.len(),
            pending_slots = ?as_vec,
            "scheduled_restart cycle-start"
        );

        // `last_fired_tick` is already set by `recompute_next_scheduled_at` to
        // the natural cron tick, so the next recompute advances past it.
        self.next_scheduled_at = None;

        self.cycle = Some(CycleState {
            id,
            cron_tick,
            started_at: now,
            phase: CyclePhase::Initial,
            pending,
            deferred: VecDeque::new(),
            current: None,
            next_action_at: now,
            outcomes: Vec::new(),
        });
    }

    fn apply_send_reboot(
        &self,
        slots: &mut HashMap<u32, SlotState>,
        slot: u32,
        now: tokio::time::Instant,
    ) {
        let Some(state) = slots.get_mut(&slot) else {
            tracing::warn!(
                slot = slot,
                "scheduled_restart attempted to reboot a slot that vanished mid-cycle"
            );
            return;
        };

        tracing::info!(
            cycle_id = self.cycle.as_ref().map(|c| c.id).unwrap_or(0),
            slot = slot,
            module = %state.module.id,
            attempt = %self.cycle
                .as_ref()
                .and_then(|c| c.current.as_ref().map(|cc| cc.attempt))
                .unwrap_or(AttemptType::Initial),
            "scheduled_restart per-card-start"
        );

        // `[scheduled_restart].restart_mode` picks the restart's severity for
        // every card in this cycle; manual `card restart` (below) always
        // does a full reset regardless of this setting.
        let mode = scheduled_restart_mode(&self.config.scheduled_restart);

        // Mirror the manual `card restart` code path: send Reboot via the worker
        // if present, else open the serial port directly.
        let retry_delay = if let Some(cmd_tx) = state.cmd_tx.take() {
            let _ = cmd_tx.send(ModuleCmd::Reboot(mode));
            RECOVERY_RETRY_DELAY
        } else {
            // No worker owns this slot right now, so there's no dedicated OS
            // thread to hand the command to — unlike the `cmd_tx` path above.
            // Open the port and run the restart on tokio's blocking pool
            // rather than inline: `RestartMode::Radio` is `AT+CFUN=0` ->
            // `sleep(4s)` -> `AT+CFUN=1`, up to ~14s of synchronous AT I/O
            // that would otherwise stall this CardPool's shared event loop —
            // every other card's control commands and bridge events — for
            // the duration (Greptile review, PR #30).
            let serial_port = state.module.serial_port.clone();
            let module_id = state.module.id.clone();
            tokio::task::spawn_blocking(move || match AtCommander::open(&serial_port) {
                Ok(mut at) => match mode {
                    RestartMode::Radio => at.radio_restart(),
                    RestartMode::Full => at.reboot(),
                },
                Err(e) => {
                    tracing::warn!(
                        module = %module_id,
                        error = %e,
                        "scheduled_restart: could not open modem port for fallback restart"
                    );
                }
            });
            // Nothing tracks this detached task's completion the way the
            // cmd_tx/worker path is tracked by CardPool::run's JoinSet (whose
            // eventual worker-exit event recomputes next_retry_at on its
            // own). Without a longer delay here, recovery could reopen this
            // same serial port before the detached radio_restart releases
            // it, interleaving AT commands or failing recovery outright
            // (Greptile review, PR #30) — so this fallback always waits out
            // the worst case regardless of which mode ran.
            RADIO_RESTART_FALLBACK_RETRY_DELAY
        };
        state.lifecycle = LifecycleState::Recovering;
        state.retry_count = 0;
        state.next_retry_at = Some(now + retry_delay);
    }

    fn record_outcome(&self, slot: u32, outcome: &CycleOutcome) {
        let cycle_id = self.cycle.as_ref().map(|c| c.id).unwrap_or(0);
        let label = outcome.outcome.metric_label();
        metrics::SCHEDULED_RESTART_TOTAL
            .with_label_values(&[&slot.to_string(), label])
            .inc();

        match &outcome.outcome {
            Outcome::Success => {
                tracing::info!(
                    cycle_id = cycle_id,
                    slot = slot,
                    attempt = %outcome.attempt,
                    outcome = "success",
                    duration_ms = outcome.duration.as_millis() as u64,
                    "scheduled_restart per-card-outcome"
                );
            }
            Outcome::Failed { reason } => {
                tracing::warn!(
                    cycle_id = cycle_id,
                    slot = slot,
                    attempt = %outcome.attempt,
                    outcome = "failed",
                    reason = %reason,
                    duration_ms = outcome.duration.as_millis() as u64,
                    "scheduled_restart per-card-outcome"
                );
            }
            Outcome::TimedOut => {
                tracing::warn!(
                    cycle_id = cycle_id,
                    slot = slot,
                    attempt = %outcome.attempt,
                    outcome = "timed-out",
                    duration_ms = outcome.duration.as_millis() as u64,
                    "scheduled_restart per-card-outcome"
                );
            }
            Outcome::Deferred { reason } => {
                tracing::debug!(
                    cycle_id = cycle_id,
                    slot = slot,
                    attempt = %outcome.attempt,
                    outcome = "deferred",
                    reason = %reason,
                    "scheduled_restart per-card-outcome"
                );
            }
            Outcome::Skipped { reason } => {
                let reason_str = match reason {
                    SkipReason::NonReady(s) => format!("non-ready: {s}"),
                    SkipReason::ActiveCall => "active-call (after deferred retry)".to_string(),
                    SkipReason::SlotDisappeared => "slot disappeared".to_string(),
                };
                tracing::debug!(
                    cycle_id = cycle_id,
                    slot = slot,
                    attempt = %outcome.attempt,
                    outcome = "skipped",
                    reason = %reason_str,
                    "scheduled_restart per-card-outcome"
                );
            }
            Outcome::AlreadyRestartedByManual => {
                tracing::debug!(
                    cycle_id = cycle_id,
                    slot = slot,
                    attempt = %outcome.attempt,
                    outcome = "skipped-already-restarted-by-manual",
                    "scheduled_restart per-card-outcome"
                );
            }
        }
    }

    fn complete_cycle(&mut self) {
        let Some(cycle) = self.cycle.take() else {
            return;
        };

        let total = cycle.outcomes.len();
        let succeeded = cycle
            .outcomes
            .iter()
            .filter(|o| matches!(o.outcome, Outcome::Success))
            .count();
        let failed = cycle
            .outcomes
            .iter()
            .filter(|o| matches!(o.outcome, Outcome::Failed { .. } | Outcome::TimedOut))
            .count();
        let deferred_recovered = cycle
            .outcomes
            .iter()
            .filter(|o| {
                matches!(o.outcome, Outcome::Success) && o.attempt == AttemptType::DeferredRetry
            })
            .count();
        let skipped = cycle
            .outcomes
            .iter()
            .filter(|o| {
                matches!(
                    o.outcome,
                    Outcome::Skipped { .. } | Outcome::AlreadyRestartedByManual
                )
            })
            .count();
        let duration_ms = tokio::time::Instant::now()
            .duration_since(cycle.started_at)
            .as_millis() as u64;

        tracing::info!(
            cycle_id = cycle.id,
            total = total,
            succeeded = succeeded,
            failed = failed,
            deferred_recovered = deferred_recovered,
            skipped = skipped,
            duration_ms = duration_ms,
            "scheduled_restart cycle-complete"
        );

        self.recompute_next_scheduled_at();
    }

    /// Handles one `SipBridge::poll_outbound_request` result: validate,
    /// select an idle CS line, dial it, and accept or refuse the SIP call
    /// accordingly (specs/025-outbound-calling, US1).
    ///
    /// Teardown needs no new code here: `ModuleCmd::Dial`
    /// (`apply_dial_cmd`) already sets `card.state = Answering` on success,
    /// which is exactly the state the existing SIP-peer-disconnect check in
    /// `run_module_loop` and the existing `BridgeEvent::Hangup` handling
    /// (both written for the inbound-call direction) already watch —
    /// reused here unmodified in the other direction.
    async fn handle_outbound_request(
        &mut self,
        call: Call,
        destination: String,
        slots: &mut HashMap<u32, SlotState>,
    ) {
        if crate::sip::outbound::validate_destination(&destination).is_err() {
            tracing::warn!(destination = %destination, "outbound: invalid destination, refusing");
            self.sip_bridge.refuse_outbound(call, 484);
            metrics::OUTBOUND_ATTEMPTS_TOTAL
                .with_label_values(&["refused_invalid_destination"])
                .inc();
            return;
        }

        let Some((slot, audio_device, cmd_tx)) = claim_idle_cs_slot(slots) else {
            tracing::warn!(destination = %destination, "outbound: no idle line, refusing");
            self.sip_bridge.refuse_outbound(call, 503);
            metrics::OUTBOUND_ATTEMPTS_TOTAL
                .with_label_values(&["refused_no_idle_line"])
                .inc();
            return;
        };

        let (resp_tx, resp_rx) = oneshot::channel();
        if cmd_tx
            .send(ModuleCmd::Dial(destination.clone(), resp_tx))
            .is_err()
        {
            tracing::warn!(
                slot,
                destination = %destination,
                "outbound: module command channel closed, refusing"
            );
            if let Some(state) = slots.get_mut(&slot) {
                state.has_active_call = false;
            }
            self.sip_bridge.refuse_outbound(call, 503);
            metrics::OUTBOUND_ATTEMPTS_TOTAL
                .with_label_values(&["refused_no_idle_line"])
                .inc();
            return;
        }

        match tokio::time::timeout(Duration::from_secs(5), resp_rx).await {
            Ok(Ok(Ok(()))) => match self.sip_bridge.accept_outbound(call, &audio_device) {
                Ok(()) => {
                    tracing::info!(slot, destination = %destination, "outbound call placed");
                    metrics::OUTBOUND_ATTEMPTS_TOTAL
                        .with_label_values(&["placed"])
                        .inc();
                }
                Err(e) => {
                    tracing::error!(slot, error = %e, "outbound: failed to accept SIP leg after dial succeeded");
                    // ATD already succeeded — the modem is on a real,
                    // live call. Hang it up before freeing the slot, or
                    // it stays connected indefinitely while `SlotState`
                    // reports it idle and eligible for a second dial
                    // (specs/025-outbound-calling review).
                    let _ = cmd_tx.send(ModuleCmd::Hangup);
                    if let Some(state) = slots.get_mut(&slot) {
                        state.has_active_call = false;
                    }
                    metrics::OUTBOUND_ATTEMPTS_TOTAL
                        .with_label_values(&["refused_network_failure"])
                        .inc();
                }
            },
            Ok(Ok(Err(e))) => {
                tracing::warn!(slot, error = %e, "outbound: dial failed");
                if let Some(state) = slots.get_mut(&slot) {
                    state.has_active_call = false;
                }
                self.sip_bridge.refuse_outbound(call, 503);
                metrics::OUTBOUND_ATTEMPTS_TOTAL
                    .with_label_values(&["refused_network_failure"])
                    .inc();
            }
            Ok(Err(_)) | Err(_) => {
                tracing::warn!(slot, "outbound: module did not respond in time");
                // The 5s timeout only bounds how long *we* wait — it says
                // nothing about the modem. If `apply_dial_cmd` is still
                // in flight and later succeeds, the call would otherwise
                // be a real, connected phone call nothing in this process
                // ever tracks or hangs up (same class of leak the
                // accept_outbound failure just above already guards
                // against). Best-effort: `run_module_loop` processes one
                // `ModuleCmd` at a time, so this queues behind whatever
                // dial is still running and hangs it up the moment it
                // finishes either way; harmless if the worker already
                // died (`cmd_tx.send` just fails silently) or if the dial
                // genuinely never reached the modem.
                let _ = cmd_tx.send(ModuleCmd::Hangup);
                if let Some(state) = slots.get_mut(&slot) {
                    state.has_active_call = false;
                }
                self.sip_bridge.refuse_outbound(call, 503);
                metrics::OUTBOUND_ATTEMPTS_TOTAL
                    .with_label_values(&["refused_network_failure"])
                    .inc();
            }
        }
    }

    async fn handle_control_cmd(
        &mut self,
        cmd: ControlCmd,
        reply: oneshot::Sender<ControlResp>,
        slots: &mut HashMap<u32, SlotState>,
        _resilience: &crate::config::ResilienceConfig,
    ) {
        match cmd {
            ControlCmd::ListSlots => {
                let mut infos: Vec<SlotInfo> = slots.values().map(|s| s.info()).collect();
                infos.sort_by_key(|i| i.slot);
                let _ = reply.send(ControlResp::ok_slots(infos));
            }

            ControlCmd::GetMode { slot } => {
                if !slots.contains_key(&slot) {
                    let max = slots.keys().max().copied().unwrap_or(0);
                    let _ = reply.send(ControlResp::err(format!(
                        "slot {slot} not found; valid slots: 0..={max}"
                    )));
                    return;
                }
                let mode = match self.store.get_mode_pref(slot) {
                    Ok(Some(m)) => m,
                    Ok(None) => NetworkMode::Auto,
                    Err(e) => {
                        let _ = reply.send(ControlResp::err(format!("DB error: {e}")));
                        return;
                    }
                };
                let _ = reply.send(ControlResp::ok_mode(mode));
            }

            ControlCmd::SetMode {
                slot,
                mode: mode_str,
            } => {
                let mode = match mode_str.parse::<NetworkMode>() {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = reply.send(ControlResp::err(e));
                        return;
                    }
                };

                let state = match slots.get(&slot) {
                    Some(s) => s,
                    None => {
                        let max = slots.keys().max().copied().unwrap_or(0);
                        let _ = reply.send(ControlResp::err(format!(
                            "slot {slot} not found; valid slots: 0..={max}"
                        )));
                        return;
                    }
                };

                if state.lifecycle != LifecycleState::Ready {
                    let _ = reply.send(ControlResp::err(format!(
                        "slot {slot} is not in Ready state (current: {})",
                        state.lifecycle
                    )));
                    return;
                }

                if let Some(cmd_tx) = state.cmd_tx.clone() {
                    let (resp_tx, resp_rx) = oneshot::channel();
                    if cmd_tx.send(ModuleCmd::SetMode(mode, resp_tx)).is_err() {
                        let _ = reply.send(ControlResp::err("module command channel closed"));
                        return;
                    }
                    let store_tx = self.store.sender();
                    // Await the response in a separate task to avoid holding &self across .await
                    tokio::spawn(async move {
                        match tokio::time::timeout(Duration::from_secs(30), resp_rx).await {
                            Ok(Ok(Ok(confirmed))) => {
                                let _ = store_tx.send(StoreCommand::SetModePref {
                                    slot,
                                    mode: confirmed,
                                });
                                let _ = reply.send(ControlResp::ok_mode(confirmed));
                            }
                            Ok(Ok(Err(e))) => {
                                let _ =
                                    reply.send(ControlResp::err(format!("AT command failed: {e}")));
                            }
                            Ok(Err(_)) => {
                                let _ = reply.send(ControlResp::err("module did not respond"));
                            }
                            Err(_) => {
                                let _ = reply.send(ControlResp::err(
                                    "AT command timeout while applying mode",
                                ));
                            }
                        }
                    });
                } else {
                    let _ = reply.send(ControlResp::err("module command channel not available"));
                }
            }

            ControlCmd::CardRestart { slot } => {
                // FR-014a: cycle concurrency rules.
                use scheduler::{handle_manual_restart_during_cycle, ManualRestartCycleAdvice};
                let cycle_advice = self
                    .cycle
                    .as_mut()
                    .map(|c| handle_manual_restart_during_cycle(c, slot))
                    .unwrap_or(ManualRestartCycleAdvice::Proceed);
                match cycle_advice {
                    ManualRestartCycleAdvice::Reject { error } => {
                        let _ = reply.send(ControlResp::err(error));
                        return;
                    }
                    ManualRestartCycleAdvice::PreemptAndProceed => {
                        // The pure helper already pushed the outcome into the
                        // cycle's outcome log; mirror it to tracing+metrics.
                        let outcome = CycleOutcome {
                            slot,
                            attempt: AttemptType::Initial,
                            outcome: Outcome::AlreadyRestartedByManual,
                            duration: Duration::ZERO,
                        };
                        self.record_outcome(slot, &outcome);
                    }
                    ManualRestartCycleAdvice::Proceed => {}
                }

                if let Some(state) = slots.get_mut(&slot) {
                    tracing::info!(slot = slot, module = %state.module.id, "card restart requested");
                    if let Some(cmd_tx) = state.cmd_tx.take() {
                        // Worker is running — ask it to send AT+CFUN=1,1 and exit.
                        // Manual restart is always a full reset, regardless of
                        // [scheduled_restart].restart_mode — that setting only
                        // governs the scheduled cycle.
                        let _ = cmd_tx.send(ModuleCmd::Reboot(RestartMode::Full));
                    } else {
                        // Worker not running — send AT+CFUN=1,1 directly
                        tracing::info!(module = %state.module.id, "no worker running, rebooting modem directly");
                        if let Ok(mut at) = AtCommander::open(&state.module.serial_port) {
                            at.reboot();
                        }
                    }
                    state.lifecycle = LifecycleState::Recovering;
                    state.retry_count = 0;
                    // Allow 10 s for the modem to reboot before re-initializing
                    state.next_retry_at =
                        Some(tokio::time::Instant::now() + Duration::from_secs(10));
                    let _ = reply.send(ControlResp::ok());
                } else {
                    let max = slots.keys().max().copied().unwrap_or(0);
                    let _ = reply.send(ControlResp::err(format!(
                        "slot {slot} not found; valid slots: 0..={max}"
                    )));
                }
            }

            ControlCmd::Observe { .. } => {
                // `control::server::handle_connection` applies this to the
                // metrics registry directly and never forwards it here — an
                // Observe reaching CardPool would mean that short-circuit
                // regressed (specs/014-vowifi-metrics-restore).
                let _ = reply.send(ControlResp::err(
                    "observe commands are handled by the control server, not CardPool",
                ));
            }
        }
    }

    fn handle_bridge_event(&mut self, event: BridgeEvent, slots: &mut HashMap<u32, SlotState>) {
        match event {
            BridgeEvent::NetworkLost { module_id } => {
                if let Some(state) = slots.values_mut().find(|s| s.module.id == module_id) {
                    if state.lifecycle == LifecycleState::Ready {
                        tracing::warn!(module = %module_id, slot = state.slot, "network lost, transitioning to Recovering");
                        state.lifecycle = LifecycleState::Recovering;
                        state.network_type = NetworkType::NoSignal;
                        state.cmd_tx = None;
                        state.retry_count = 0;
                        state.next_retry_at = Some(
                            tokio::time::Instant::now()
                                + backoff_delay(
                                    0,
                                    self.config.resilience.initial_backoff_sec,
                                    self.config.resilience.max_backoff_sec,
                                ),
                        );
                    }
                    // Network loss tears down any in-progress call; clear the flag so
                    // the scheduler does not permanently defer this slot.
                    if state.has_active_call {
                        tracing::warn!(module = %module_id, slot = state.slot, "active call terminated by network loss");
                        state.has_active_call = false;
                    }
                }
            }
            BridgeEvent::Ring {
                module_id,
                caller_id,
                audio_device,
            } => {
                if let Some(state) = slots.values_mut().find(|s| s.module.id == module_id) {
                    state.has_active_call = true;
                }
                // `handle_ring` (the per-module worker) already sent `ATA`
                // and answered the call on real hardware *before* this event
                // ever arrives — every early return below is therefore a
                // real, live, connected GSM call with no SIP bridge for it,
                // not just an aborted attempt. Found in review (Greptile,
                // PR #22): a plain "clear the flag and give up" here left
                // that call connected to dead air indefinitely, while
                // `has_active_call = false` made the slot look idle and
                // eligible for a *second* dial while the modem was still
                // genuinely busy with the first. `hang_up_unbridged_call!`
                // sends the same `ModuleCmd::Hangup` the outbound-dial path
                // already uses for the equivalent "answered but couldn't
                // bridge" case — it hangs up the real call, sets
                // `card.state = Idle`, and closes out `call_ctx` via
                // `record_call_end("failed")` — genuinely freeing the slot,
                // not just pretending to.
                macro_rules! hang_up_unbridged_call {
                    () => {
                        if let Some(state) = slots.values_mut().find(|s| s.module.id == module_id) {
                            state.has_active_call = false;
                            if let Some(cmd_tx) = &state.cmd_tx {
                                let _ = cmd_tx.send(ModuleCmd::Hangup);
                            }
                        }
                    };
                }
                if self.sip_bridge.state != crate::sip::RegistrationState::Registered {
                    tracing::warn!(
                        module = %module_id,
                        "SIP not registered, cannot bridge call"
                    );
                    hang_up_unbridged_call!();
                    return;
                }

                // In SIP server mode this fails when the phone is not
                // registered.
                let dest_uri = match self.sip_bridge.compute_destination_uri(&caller_id) {
                    Ok(uri) => uri,
                    Err(e) => {
                        tracing::warn!(
                            module = %module_id,
                            caller = %caller_id,
                            error = %e,
                            "cannot bridge call: no phone to ring"
                        );
                        metrics::SIP_SERVER_RING_TARGET_MISSING_TOTAL.inc();
                        metrics::SIP_CALLS_TOTAL
                            .with_label_values(&[&module_id, "error", "cs"])
                            .inc();
                        hang_up_unbridged_call!();
                        return;
                    }
                };
                tracing::info!(
                    module = %module_id,
                    caller = %caller_id,
                    dest = %dest_uri,
                    audio = %audio_device,
                    "bridging GSM call to SIP"
                );

                if let Err(e) = self.sip_bridge.set_sound_device(&audio_device) {
                    tracing::error!(error = %e, "failed to set sound device");
                    metrics::AUDIO_ERRORS_TOTAL
                        .with_label_values(&[&module_id, "sound_device"])
                        .inc();
                    hang_up_unbridged_call!();
                    return;
                }

                if let Err(e) = self.sip_bridge.make_call(&dest_uri, &caller_id) {
                    tracing::error!(
                        module = %module_id,
                        error = %e,
                        "SIP outbound call failed"
                    );
                    metrics::SIP_CALLS_TOTAL
                        .with_label_values(&[&module_id, "error", "cs"])
                        .inc();
                    hang_up_unbridged_call!();
                } else {
                    metrics::SIP_CALLS_TOTAL
                        .with_label_values(&[&module_id, "initiated", "cs"])
                        .inc();
                }
            }
            BridgeEvent::Hangup { module_id } => {
                if let Some(state) = slots.values_mut().find(|s| s.module.id == module_id) {
                    state.has_active_call = false;
                }
                tracing::info!(module = %module_id, "GSM call ended, tearing down SIP call");
                self.sip_bridge.hangup_active_call();
            }
            BridgeEvent::SmsReceived {
                module_id,
                sender,
                body,
                received_at,
            } => {
                sms::record_and_forward(
                    &tokio::runtime::Handle::current(),
                    self.store.sender(),
                    self.discord_client.clone(),
                    module_id,
                    sender,
                    body,
                    received_at,
                    crate::store::Transport::Cs,
                    None,
                );
            }
            BridgeEvent::CriticalAlert(event) => {
                if let Some(client) = self.alerts_client.clone() {
                    let config = self.config.alerts.clone();
                    tokio::runtime::Handle::current()
                        .spawn(async move { alerts::dispatch(&client, &config, event).await });
                }
            }
        }
    }
}

/// Selects the first `Ready`, non-busy CS slot and claims it
/// (`has_active_call = true`) in the same pass, before the caller ever
/// dispatches a `ModuleCmd::Dial` — pulled out of `handle_outbound_request`
/// so the claim discipline itself (not the SIP-side accept/refuse plumbing
/// around it, which needs a real `SipBridge`) is directly unit-testable
/// (specs/025-outbound-calling, T025). No path preference beyond "first
/// idle wins" (FR-007) — iterates every slot in `slots`, not just one line,
/// so this covers every CS modem attached to the host.
///
/// Calling this twice in a row with the same `slots` map and only one
/// idle slot returns `Some` then `None` — the whole point: a second
/// outbound request (or a concurrent inbound GSM call reusing the same
/// slot-claim convention) sees the slot as busy the instant this returns,
/// not only once the `ModuleCmd::Dial` round trip finishes.
fn claim_idle_cs_slot(
    slots: &mut HashMap<u32, SlotState>,
) -> Option<(u32, String, crossbeam_channel::Sender<ModuleCmd>)> {
    let (slot, audio_device, cmd_tx) = slots
        .iter()
        .find(|(_, s)| s.lifecycle == LifecycleState::Ready && !s.has_active_call)
        .map(|(slot, s)| (*slot, s.module.audio_device.clone(), s.cmd_tx.clone()))?;
    let cmd_tx = cmd_tx?;

    if let Some(state) = slots.get_mut(&slot) {
        state.has_active_call = true;
    }
    Some((slot, audio_device, cmd_tx))
}

struct CallContext {
    caller_id: String,
    sip_destination: String,
    started_at: chrono::DateTime<Utc>,
}

struct ModuleAudioInit {
    rx_gain: Option<u32>,
    eec_mode: Option<u32>,
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

/// How often the idle-tick liveness probe (`AT`) is sent while otherwise
/// quiet — comfortably below any reasonable `at_worker_unresponsive_sec`
/// threshold (default 60s) so a real stall is caught within one threshold
/// window, not two.
const AT_WORKER_PROBE_INTERVAL: Duration = Duration::from_secs(15);

#[allow(clippy::too_many_arguments)]
fn run_module_loop(
    module: DiscoveredModule,
    store_tx: crossbeam_channel::Sender<StoreCommand>,
    _sms_enabled: bool,
    event_tx: mpsc::UnboundedSender<BridgeEvent>,
    cmd_rx: crossbeam_channel::Receiver<ModuleCmd>,
    ring_capacity: usize,
    audio_init: ModuleAudioInit,
    at_worker_unresponsive_threshold: Duration,
) -> Result<(), String> {
    let mut at = AtCommander::open(&module.serial_port).map_err(|e| e.to_string())?;

    at.send_command("ATE0").ok();
    at.send_command("AT+CLIP=1").ok();
    at.send_command("AT+CMGF=1").ok();
    at.send_command("AT+CNMI=2,1,0,0,0").ok();
    at.send_command("AT+CREG=1").ok();
    at.send_command("AT+CEREG=1").ok();
    route_audio_to_usb(&mut at, &module.id);
    if let Some(gain) = audio_init.rx_gain {
        set_rx_gain(&mut at, &module.id, gain);
    }
    if let Some(mode) = audio_init.eec_mode {
        set_eec_mode(&mut at, &module.id, mode);
    }

    if let Ok((rssi, _ber)) = at.check_signal() {
        tracing::info!(module = %module.id, rssi = rssi, "signal quality");
    }

    let mut card = CardInstance::new(
        module.id.clone(),
        module.serial_port.clone(),
        module.audio_device.clone(),
        ring_capacity,
    );

    let mut call_ctx: Option<CallContext> = None;

    tracing::info!(module = %module.id, "module worker started, monitoring for events");
    metrics::ACTIVE_CALLS
        .with_label_values(&[&module.id, "cs"])
        .set(0.0);

    // specs/022-discord-critical-alerts FR-003: last time an AT command on
    // this module succeeded, and whether we've already alerted for it being
    // unresponsive (so we don't re-alert every tick — FR-013).
    let mut last_at_success = std::time::Instant::now();
    let mut last_at_probe = std::time::Instant::now();
    let mut at_worker_alerted = false;

    loop {
        if last_at_probe.elapsed() >= AT_WORKER_PROBE_INTERVAL {
            last_at_probe = std::time::Instant::now();
            match at.send_command("AT") {
                Ok(AtResponse::Ok(_)) => {
                    last_at_success = std::time::Instant::now();
                    if at_worker_alerted {
                        at_worker_alerted = false;
                        let _ = event_tx.send(BridgeEvent::CriticalAlert(alerts::CriticalEvent {
                            category: alerts::AlertCategory::ModuleLifecycle,
                            unit_id: Some(module.id.clone()),
                            description: "AT command worker responsive again".to_string(),
                            at: Utc::now(),
                            kind: alerts::CriticalEventKind::Recovered,
                        }));
                    }
                }
                _ => {
                    if !at_worker_alerted
                        && at_worker_unresponsive(
                            last_at_success,
                            std::time::Instant::now(),
                            at_worker_unresponsive_threshold,
                        )
                    {
                        at_worker_alerted = true;
                        tracing::error!(module = %module.id, "AT command worker unresponsive");
                        let _ = event_tx.send(BridgeEvent::CriticalAlert(alerts::CriticalEvent {
                            category: alerts::AlertCategory::ModuleLifecycle,
                            unit_id: Some(module.id.clone()),
                            description: format!(
                                "AT command worker unresponsive for over {}s",
                                at_worker_unresponsive_threshold.as_secs()
                            ),
                            at: Utc::now(),
                            kind: alerts::CriticalEventKind::Failure,
                        }));
                    }
                }
            }
        }

        // If cmd_tx side was dropped (slot restarted), try_recv will see a disconnect.
        // We check via a separate try_recv for the disconnect error.
        match cmd_rx.try_recv() {
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                tracing::info!(module = %module.id, "control channel closed, worker exiting");
                return Ok(());
            }
            Ok(cmd) => {
                // Process the command we just received
                match cmd {
                    ModuleCmd::SetMode(mode, resp_tx) => {
                        let result = at.set_network_mode(mode).map_err(|e| e.to_string());
                        let _ = resp_tx.send(result);
                    }
                    ModuleCmd::Reboot(RestartMode::Full) => {
                        tracing::info!(module = %module.id, "rebooting modem (AT+CFUN=1,1)");
                        at.reboot();
                        return Ok(());
                    }
                    ModuleCmd::Reboot(RestartMode::Radio) => {
                        tracing::info!(module = %module.id, "radio-cycling modem (AT+CFUN=0 -> 1)");
                        at.radio_restart();
                        return Ok(());
                    }
                    ModuleCmd::Dial(number, resp_tx) => {
                        let result = apply_dial_cmd(&mut at, &mut card, &number);
                        if result.is_ok() {
                            record_call_start_outbound(&module.id, &number, &mut call_ctx);
                        }
                        let _ = resp_tx.send(result);
                    }
                    ModuleCmd::Hangup => {
                        tracing::info!(module = %module.id, "outbound: hanging up after SIP-side accept failed post-dial");
                        let _ = at.hangup();
                        card.state = CardState::Idle;
                        metrics::ACTIVE_CALLS
                            .with_label_values(&[&module.id, "cs"])
                            .set(0.0);
                        record_call_end(&module.id, &event_tx, &store_tx, &mut call_ctx, "failed");
                    }
                }
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
        }

        let line = match read_line_from_at(&mut at) {
            Ok(l) => l,
            Err(e) => {
                if e.contains("timeout") || e.contains("TimedOut") {
                    continue;
                }
                tracing::error!(module = %module.id, error = %e, "serial read error");
                return Err(e);
            }
        };

        if pjsua_safe::is_sip_peer_disconnected()
            && (card.state == CardState::Bridged || card.state == CardState::Answering)
        {
            tracing::info!(module = %module.id, "SIP peer disconnected, hanging up GSM");
            let _ = at.hangup();
            record_call_end(&module.id, &event_tx, &store_tx, &mut call_ctx, "answered");
            card.state = CardState::Idle;
            metrics::ACTIVE_CALLS
                .with_label_values(&[&module.id, "cs"])
                .set(0.0);
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        tracing::trace!(module = %module.id, urc = trimmed, "received");

        if trimmed == "RING" {
            handle_ring(&module, &mut at, &mut card, &event_tx, &mut call_ctx);
        } else if trimmed == "NO CARRIER" {
            handle_hangup(&module, &mut card, &event_tx, &store_tx, &mut call_ctx);
        } else if trimmed.starts_with("+CMTI:") {
            handle_cmti(&module, &mut at, trimmed, &event_tx);
        } else if trimmed.starts_with("+CREG:") || trimmed.starts_with("+CEREG:") {
            handle_creg_urc(&module, trimmed, &event_tx);
        }
    }
}

/// The body of `ModuleCmd::Dial` handling, pulled out of `run_module_loop`'s
/// match arm so it's unit-testable against a mocked `AtCommander` the same
/// way `at_commander`'s own tests mock the serial stream
/// (specs/025-outbound-calling, contracts/control-cmd-dial.md).
///
/// Same-thread serialization is the race guard: `run_module_loop` only ever
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

fn handle_creg_urc(
    module: &DiscoveredModule,
    line: &str,
    event_tx: &mpsc::UnboundedSender<BridgeEvent>,
) {
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
        tracing::warn!(module = %module.id, stat = stat, "network registration lost");
        let _ = event_tx.send(BridgeEvent::NetworkLost {
            module_id: module.id.clone(),
        });
    }
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

fn handle_ring(
    module: &DiscoveredModule,
    at: &mut AtCommander,
    card: &mut CardInstance,
    event_tx: &mpsc::UnboundedSender<BridgeEvent>,
    call_ctx: &mut Option<CallContext>,
) {
    if card.state != CardState::Idle {
        return;
    }

    tracing::info!(module = %module.id, "incoming call (RING)");
    card.state = CardState::Ringing;
    metrics::CALLS_TOTAL
        .with_label_values(&[&module.id, "incoming", "cs"])
        .inc();

    let caller_id = extract_caller_id(at);

    match at.answer_call() {
        Ok(()) => {
            card.state = CardState::Answering;
            tracing::info!(
                module = %module.id,
                caller = %caller_id,
                "call answered, requesting SIP bridge"
            );

            *call_ctx = Some(CallContext {
                caller_id: caller_id.clone(),
                sip_destination: String::new(),
                started_at: Utc::now(),
            });

            let _ = event_tx.send(BridgeEvent::Ring {
                module_id: module.id.clone(),
                caller_id,
                audio_device: module.audio_device.clone(),
            });

            card.state = CardState::Bridged;
            metrics::ACTIVE_CALLS
                .with_label_values(&[&module.id, "cs"])
                .set(1.0);
            metrics::CALLS_TOTAL
                .with_label_values(&[&module.id, "answered", "cs"])
                .inc();
        }
        Err(e) => {
            tracing::error!(module = %module.id, error = %e, "failed to answer call");
            card.state = CardState::Idle;
            metrics::CALLS_TOTAL
                .with_label_values(&[&module.id, "missed", "cs"])
                .inc();
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

fn handle_hangup(
    module: &DiscoveredModule,
    card: &mut CardInstance,
    event_tx: &mpsc::UnboundedSender<BridgeEvent>,
    store_tx: &crossbeam_channel::Sender<StoreCommand>,
    call_ctx: &mut Option<CallContext>,
) {
    if card.state == CardState::Bridged || card.state == CardState::Answering {
        tracing::info!(module = %module.id, "call ended (NO CARRIER)");
        metrics::ACTIVE_CALLS
            .with_label_values(&[&module.id, "cs"])
            .set(0.0);
        let _ = event_tx.send(BridgeEvent::Hangup {
            module_id: module.id.clone(),
        });
        record_call_end(&module.id, event_tx, store_tx, call_ctx, "answered");
    } else if card.state == CardState::Ringing {
        record_call_end(&module.id, event_tx, store_tx, call_ctx, "missed");
    }
    card.state = CardState::Idle;
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

fn handle_cmti(
    module: &DiscoveredModule,
    at: &mut AtCommander,
    line: &str,
    event_tx: &mpsc::UnboundedSender<BridgeEvent>,
) {
    tracing::info!(module = %module.id, notification = line, "SMS notification received");
    metrics::SMS_RECEIVED_TOTAL
        .with_label_values(&[&module.id, "cs"])
        .inc();

    if let Some(idx_str) = line.split(',').next_back() {
        if let Ok(idx) = idx_str.trim().parse::<u32>() {
            let cmd = format!("AT+CMGR={idx}");
            match at.send_command(&cmd) {
                Ok(AtResponse::Ok(lines)) => {
                    tracing::debug!(module = %module.id, index = idx, lines = ?lines, "SMS read");

                    let (sender, body) = parse_sms_response(&lines);
                    let received_at = Utc::now().to_rfc3339();

                    let _ = event_tx.send(BridgeEvent::SmsReceived {
                        module_id: module.id.clone(),
                        sender,
                        body,
                        received_at,
                    });

                    let del_cmd = format!("AT+CMGD={idx}");
                    at.send_command(&del_cmd).ok();
                }
                Ok(AtResponse::Error(e)) => {
                    tracing::warn!(module = %module.id, error = %e, "failed to read SMS");
                }
                Ok(AtResponse::CmeError(code, msg)) => {
                    tracing::warn!(module = %module.id, code = code, error = %msg, "failed to read SMS");
                }
                Err(e) => {
                    tracing::warn!(module = %module.id, error = %e, "failed to read SMS");
                }
            }
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
    fn scheduled_restart_mode_selects_radio_when_configured() {
        let cfg = ScheduledRestartConfig {
            restart_mode: "radio".to_string(),
            ..ScheduledRestartConfig::default()
        };
        assert_eq!(scheduled_restart_mode(&cfg), RestartMode::Radio);
    }

    #[test]
    fn scheduled_restart_mode_defaults_to_full() {
        assert_eq!(
            scheduled_restart_mode(&ScheduledRestartConfig::default()),
            RestartMode::Full,
            "the default config's restart_mode is \"full\", preserving old behavior"
        );
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

    #[test]
    fn test_backoff_delay_initial() {
        let d = backoff_delay(0, 5, 120);
        assert_eq!(d, Duration::from_secs(5));
    }

    #[test]
    fn test_backoff_delay_doubles() {
        assert_eq!(backoff_delay(1, 5, 120), Duration::from_secs(10));
        assert_eq!(backoff_delay(2, 5, 120), Duration::from_secs(20));
        assert_eq!(backoff_delay(3, 5, 120), Duration::from_secs(40));
        assert_eq!(backoff_delay(4, 5, 120), Duration::from_secs(80));
    }

    #[test]
    fn test_backoff_delay_caps_at_max() {
        assert_eq!(backoff_delay(5, 5, 120), Duration::from_secs(120));
        assert_eq!(backoff_delay(10, 5, 120), Duration::from_secs(120));
        assert_eq!(backoff_delay(30, 5, 120), Duration::from_secs(120));
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

        record_call_end("card0", &event_tx, &store_tx, &mut call_ctx, "missed");

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

        record_call_end("card0", &event_tx, &store_tx, &mut call_ctx, "answered");

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

        record_call_end("card0", &event_tx, &store_tx, &mut call_ctx, "answered");
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
        record_call_end("card0", &event_tx, &store_tx, &mut call_ctx, "failed");

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

    // specs/022-discord-critical-alerts (Greptile P1: "GivenUp state never
    // recovers").

    fn slot_state(module_id: &str, lifecycle: LifecycleState) -> SlotState {
        SlotState {
            slot: 0,
            module: DiscoveredModule {
                id: module_id.to_string(),
                serial_port: PathBuf::from(format!("/dev/tty{module_id}")),
                audio_device: String::new(),
                usb_serial: String::new(),
            },
            imei: String::new(),
            phone_number: String::new(),
            network_type: NetworkType::Unknown,
            network_mode: None,
            lifecycle,
            retry_count: 0,
            next_retry_at: None,
            cmd_tx: None,
            has_active_call: false,
        }
    }

    #[test]
    fn find_given_up_slot_locates_stale_slot_by_module_id() {
        let mut slots = HashMap::new();
        slots.insert(3, slot_state("card0", LifecycleState::GivenUp));

        assert_eq!(find_given_up_slot(&slots, "card0"), Some(3));
    }

    #[test]
    fn find_given_up_slot_ignores_non_given_up_slots_for_the_same_module() {
        let mut slots = HashMap::new();
        slots.insert(3, slot_state("card0", LifecycleState::Ready));

        assert_eq!(
            find_given_up_slot(&slots, "card0"),
            None,
            "a Ready slot is not a stale incident to clear"
        );
    }

    fn slot_state_with_cmd_tx(module_id: &str) -> SlotState {
        let (cmd_tx, _cmd_rx) = crossbeam_channel::unbounded();
        SlotState {
            cmd_tx: Some(cmd_tx),
            ..slot_state(module_id, LifecycleState::Ready)
        }
    }

    /// specs/025-outbound-calling T025 (fifth code review's rewrite of
    /// T024-T026 — the original `ControlCmd::Dial` contention test's premise
    /// no longer exists, that variant was deleted): the whole point of
    /// claiming a slot in the same pass as selecting it is that a *second*
    /// selection against the same map sees the claim immediately, not only
    /// once the dial round trip completes. With a single idle slot, the
    /// first call must succeed and the second must find nothing idle.
    #[test]
    fn claim_idle_cs_slot_does_not_double_book_the_last_idle_slot() {
        let mut slots = HashMap::new();
        slots.insert(0, slot_state_with_cmd_tx("card0"));

        let first = claim_idle_cs_slot(&mut slots);
        assert!(first.is_some(), "the only idle slot must be claimable");
        assert_eq!(first.unwrap().0, 0);
        assert!(
            slots[&0].has_active_call,
            "claiming must mark the slot busy before any dial round trip runs"
        );

        let second = claim_idle_cs_slot(&mut slots);
        assert!(
            second.is_none(),
            "a second request must not double-book the slot the first one just claimed"
        );
    }

    #[test]
    fn claim_idle_cs_slot_picks_among_every_attached_modem() {
        let mut slots = HashMap::new();
        slots.insert(0, slot_state_with_cmd_tx("card0"));
        // Busy: must be skipped even though it comes first in iteration for
        // some hash orderings.
        let mut busy = slot_state_with_cmd_tx("card1");
        busy.has_active_call = true;
        slots.insert(1, busy);
        slots.insert(2, slot_state_with_cmd_tx("card2"));

        let mut claimed = Vec::new();
        while let Some((slot, _, _)) = claim_idle_cs_slot(&mut slots) {
            claimed.push(slot);
        }

        claimed.sort_unstable();
        assert_eq!(
            claimed,
            vec![0, 2],
            "every Ready, non-busy modem must eventually be claimable, and the busy one never"
        );
    }

    #[test]
    fn find_given_up_slot_ignores_a_different_modules_given_up_slot() {
        let mut slots = HashMap::new();
        slots.insert(3, slot_state("card1", LifecycleState::GivenUp));

        assert_eq!(find_given_up_slot(&slots, "card0"), None);
    }
}
