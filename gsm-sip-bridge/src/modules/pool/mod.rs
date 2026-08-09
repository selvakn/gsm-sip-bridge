//! The card pool: the async owner of every circuit-switched modem slot.
//!
//! `CardPool`'s inherent `impl` had grown to ~1500 lines in one block, mixing
//! three unrelated jobs. It is now split across three files by concern, all
//! `impl CardPool` blocks on the type declared here:
//!
//! | Concern | Module |
//! |---|---|
//! | Slot bootstrap, retry/backoff, hotplug rescan, the `select!` loop | this file |
//! | The `[scheduled_restart]` cycle that drives `modules::scheduler` | [`restart_cycle`] |
//! | Control-socket commands, worker events, outbound dial requests | [`dispatch`] |
//!
//! Splitting an inherent `impl` across modules is deliberate here: every
//! method needs the same private fields, so an extracted *type* would have
//! had to expose all of them anyway. The concerns really are one object's
//! behavior — they just aren't one file's worth of reading.

mod dispatch;
mod restart_cycle;

use crate::alerts;
use crate::config::secret::Secret;
use crate::config::AppConfig;
use crate::metrics;
use crate::modules::at_commander::{AtCommander, AtResponse, NetworkMode, NetworkType};
use crate::modules::discovery::{self, DiscoveredModule};
use crate::modules::protocol::{BridgeEvent, ControlCmdReceiver, ModuleCmd};
use crate::modules::scheduler::{self, CycleState};
use crate::modules::slot::{backoff_delay, find_given_up_slot, LifecycleState, SlotState};
use crate::modules::worker::{self, ModuleAudioInit, WorkerSetup};
use crate::sip::SipBridge;
use crate::sms::discord::DiscordClient;
use crate::sms::SmsHandler;
use crate::store::{StoreCommand, StoreHandle};
use chrono::Utc;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinSet;

/// How often the hotplug rescan runs. Hot-plug is rare, so this is
/// deliberately slow relative to everything else in the wakeup computation.
const RESCAN_INTERVAL: Duration = Duration::from_secs(60);

pub struct CardPool {
    config: AppConfig,
    store: StoreHandle,
    sip_bridge: SipBridge,
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
    /// specs/030-bad-port-isolation: persistent across rescans so a port
    /// quarantined after repeated probe timeouts stays skipped, and the
    /// operator `[discovery]` blocklist/timeout apply on every rescan. Behind a
    /// `Mutex` because the rescan methods take `&self`.
    discovery_policy: std::sync::Mutex<discovery::DiscoveryPolicy>,
}

/// Republishes the two module-count gauges from the current slot map. Called
/// at startup and after anything that can change a slot's lifecycle.
fn update_module_gauges(slots: &HashMap<u32, SlotState>) {
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
}

impl CardPool {
    /// `sms_handler` is consumed rather than stored: the only thing the pool
    /// ever needed from it was whether a webhook is configured, and the
    /// per-worker store handle comes from `store` instead. Keeping it as a
    /// field left it write-only.
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

        let discovery_policy =
            std::sync::Mutex::new(discovery::DiscoveryPolicy::new(config.discovery.clone()));

        Self {
            config,
            store,
            sip_bridge,
            discord_client,
            alerts_client,
            cron_schedule,
            cycle: None,
            next_scheduled_at: None,
            last_fired_tick: None,
            discovery_policy,
        }
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

        let modules = self.discover_modules(single_card);
        if modules.is_empty() {
            tracing::warn!("no EC20 modules found — waiting for retry or shutdown");
        }

        let mut slots: HashMap<u32, SlotState> = HashMap::new();
        let mut tasks: JoinSet<(u32, String)> = JoinSet::new();
        let resilience = self.config.resilience.clone();

        self.bootstrap_slots(modules, &mut slots, &mut tasks, &event_tx, &resilience);

        // Print startup diagnostics
        self.print_diagnostics(&slots);
        update_module_gauges(&slots);

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

        // USB rescan for hotplug reconnect
        let mut rescan_deadline = tokio::time::Instant::now() + RESCAN_INTERVAL;

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
            let earliest_wakeup = self.earliest_wakeup(&slots, rescan_deadline);

            tokio::select! {
                _ = shutdown_rx.recv() => {
                    tracing::info!("card pool shutting down");
                    break;
                }
                Some(event) = event_rx.recv() => {
                    self.handle_bridge_event(event, &mut slots);
                }
                Some(result) = tasks.join_next() => {
                    Self::on_worker_exit(result, &mut slots, &resilience);
                }
                Some((cmd, reply)) = control_rx.recv() => {
                    self.handle_control_cmd(cmd, reply, &mut slots, &mut tasks).await;
                }
                _ = outbound_poll.tick(), if self.config.outbound.enabled => {
                    if let Some((call, destination)) = self.sip_bridge.poll_outbound_request() {
                        self.handle_outbound_request(call, destination, &mut slots).await;
                    }
                }
                _ = tokio::time::sleep_until(earliest_wakeup) => {
                    self.on_timer_tick(
                        &mut slots,
                        &mut tasks,
                        &event_tx,
                        &resilience,
                        &mut rescan_deadline,
                    );
                }
            }
        }

        self.sip_bridge.unregister();
        tasks.shutdown().await;
        self.store.shutdown();
    }

    /// Either the operator-pinned single card, or a full discovery scan.
    ///
    /// A card assigned to the host-side cellular service belongs to it alone
    /// (FR-034) — probing a port another subsystem is mid-transaction on is
    /// the documented "claimed by both" hazard.
    fn discover_modules(&self, single_card: Option<(PathBuf, String)>) -> Vec<DiscoveredModule> {
        match single_card {
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
            None => match discovery::scan_modules_excluding_cards(
                &discovery::volte_claimed_ports(&self.config.volte),
                &discovery::volte_claimed_card_ids(&self.config.volte),
                &mut self.discovery_policy.lock().unwrap(),
            ) {
                Ok(m) => m,
                Err(e) => {
                    tracing::error!(error = %e, "module discovery failed");
                    Vec::new()
                }
            },
        }
    }

    /// Initializes every discovered module into a slot, spawning a worker for
    /// each one that comes up and parking the rest on the retry backoff.
    fn bootstrap_slots(
        &self,
        modules: Vec<DiscoveredModule>,
        slots: &mut HashMap<u32, SlotState>,
        tasks: &mut JoinSet<(u32, String)>,
        event_tx: &mpsc::UnboundedSender<BridgeEvent>,
        resilience: &crate::config::ResilienceConfig,
    ) {
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

                    let cmd_tx = self.spawn_worker(tasks, &module, slot, event_tx);
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
                    tracing::warn!(module = %module.id, error = %e, "module init failed, will retry");
                    metrics::MODULE_INIT_TOTAL
                        .with_label_values(&[&module.id, "failure", &e])
                        .inc();
                    // Assign a temporary slot for tracking
                    let slot = slots.len() as u32;
                    slots.insert(slot, pending_slot(slot, module, resilience));
                }
            }
        }
    }

    /// The `select!` arm that fires on the earliest of: a slot's retry
    /// backoff, the hotplug rescan deadline, or a scheduled-restart deadline.
    fn on_timer_tick(
        &mut self,
        slots: &mut HashMap<u32, SlotState>,
        tasks: &mut JoinSet<(u32, String)>,
        event_tx: &mpsc::UnboundedSender<BridgeEvent>,
        resilience: &crate::config::ResilienceConfig,
        rescan_deadline: &mut tokio::time::Instant,
    ) {
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
            if should_retry {
                self.retry_slot(slot, slots, tasks, event_tx, resilience);
            }
        }

        // USB rescan for new modules
        if now >= *rescan_deadline {
            self.rescan_new_modules(slots, tasks, event_tx);
            *rescan_deadline = tokio::time::Instant::now() + RESCAN_INTERVAL;
        }

        // Scheduled restart: start cycle if armed, or advance running cycle.
        self.advance_scheduler(slots, tasks, now);

        update_module_gauges(slots);
    }

    /// One expired-backoff retry for a single slot.
    fn retry_slot(
        &self,
        slot: u32,
        slots: &mut HashMap<u32, SlotState>,
        tasks: &mut JoinSet<(u32, String)>,
        event_tx: &mpsc::UnboundedSender<BridgeEvent>,
        resilience: &crate::config::ResilienceConfig,
    ) {
        let module = slots[&slot].module.clone();
        metrics::MODULE_RETRIES_TOTAL
            .with_label_values(&[&module.id])
            .inc();

        match self.try_init_module(&module) {
            Ok((new_slot, imei, phone, net_type, net_mode)) => {
                tracing::info!(module = %module.id, slot = new_slot, "module recovered on retry");
                metrics::MODULE_INIT_TOTAL
                    .with_label_values(&[&module.id, "success", ""])
                    .inc();

                let cmd_tx = self.spawn_worker(tasks, &module, new_slot, event_tx);

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

                // specs/022-discord-critical-alerts (Greptile P1 follow-up:
                // "GivenUp cleanup runs only on immediate rescan success and
                // is skipped when initialization succeeds through the retry
                // loop"). This slot's own retry never touches a `GivenUp`
                // slot directly (excluded by `should_retry` in
                // `on_timer_tick`), but a *different* slot for the same
                // module_id can be `GivenUp` — e.g. a rescan's failed init
                // created a fresh `Initializing` slot for a module that
                // already had a `GivenUp` slot from an earlier incident, and
                // it's that fresh slot recovering here.
                self.clear_stale_given_up(slots, &module.id);
            }
            Err(e) => {
                tracing::debug!(module = %module.id, error = %e, "retry failed");
                let Some(state) = slots.get_mut(&slot) else {
                    return;
                };
                state.retry_count += 1;
                if state.retry_count < resilience.max_retries {
                    let delay = backoff_delay(
                        state.retry_count,
                        resilience.initial_backoff_sec,
                        resilience.max_backoff_sec,
                    );
                    state.next_retry_at = Some(tokio::time::Instant::now() + delay);
                    return;
                }

                tracing::error!(
                    module = %module.id,
                    slot = slot,
                    retries = state.retry_count,
                    "module gave up after max retries"
                );
                state.lifecycle = LifecycleState::GivenUp;
                state.next_retry_at = None;
                let retry_count = state.retry_count;
                self.dispatch_alert(
                    &module.id,
                    format!("module failed to initialize after {retry_count} retries: {e}"),
                    alerts::CriticalEventKind::Failure,
                );
            }
        }
    }

    /// A module worker thread ended: park its slot on the retry backoff so
    /// the timer tick picks it up.
    fn on_worker_exit(
        result: Result<(u32, String), tokio::task::JoinError>,
        slots: &mut HashMap<u32, SlotState>,
        resilience: &crate::config::ResilienceConfig,
    ) {
        let (slot, module_id) = match result {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "module worker panicked");
                return;
            }
        };
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
            update_module_gauges(slots);
        }
    }

    /// The earliest instant anything needs attention: a slot retry, the
    /// hotplug rescan, the next armed cycle, or the running cycle's next step.
    fn earliest_wakeup(
        &self,
        slots: &HashMap<u32, SlotState>,
        rescan_deadline: tokio::time::Instant,
    ) -> tokio::time::Instant {
        let next_slot_retry = slots
            .values()
            .filter_map(|s| s.next_retry_at)
            .min()
            .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));

        let mut earliest = next_slot_retry.min(rescan_deadline);
        if let Some(sched) = self.next_scheduled_at {
            earliest = earliest.min(sched);
        }
        if let Some(cycle) = self.cycle.as_ref() {
            earliest = earliest.min(cycle.next_action_at);
        }
        earliest
    }

    /// The per-worker slice of config, rebuilt per spawn. Cheap (two channel
    /// clones and four scalars) and keeps the three spawn sites from each
    /// re-deriving the same argument list.
    fn worker_setup(&self) -> WorkerSetup {
        WorkerSetup {
            store_tx: self.store.sender(),
            ring_capacity: self.config.audio.settings.ring_capacity,
            audio: ModuleAudioInit {
                rx_gain: self.config.modem_audio.rx_gain,
                eec_mode: self.config.modem_audio.eec_mode,
            },
            at_worker_unresponsive_threshold: Duration::from_secs(
                self.config
                    .alerts
                    .module_lifecycle_thresholds
                    .at_worker_unresponsive_sec,
            ),
        }
    }

    /// Spawns the blocking worker thread for `module` and returns the command
    /// channel the pool talks to it through. Tracked in `tasks` (not detached)
    /// so `run`'s `join_next` arm sees the exit and reschedules the slot.
    fn spawn_worker(
        &self,
        tasks: &mut JoinSet<(u32, String)>,
        module: &DiscoveredModule,
        slot: u32,
        event_tx: &mpsc::UnboundedSender<BridgeEvent>,
    ) -> crossbeam_channel::Sender<ModuleCmd> {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<ModuleCmd>();
        let setup = self.worker_setup();
        let module = module.clone();
        let event_tx = event_tx.clone();
        tasks.spawn_blocking(move || {
            let module_id = module.id.clone();
            if let Err(e) = worker::run_module_loop(module, setup, event_tx, cmd_rx) {
                tracing::error!(module = %module_id, error = %e, "module loop exited with error");
            }
            (slot, module_id)
        });
        cmd_tx
    }

    /// Fire-and-forget a `ModuleLifecycle` critical alert, if a client exists.
    fn dispatch_alert(
        &self,
        module_id: &str,
        description: String,
        kind: alerts::CriticalEventKind,
    ) {
        let Some(client) = self.alerts_client.clone() else {
            return;
        };
        let config = self.config.alerts.clone();
        let event = alerts::CriticalEvent {
            category: alerts::AlertCategory::ModuleLifecycle,
            unit_id: Some(module_id.to_string()),
            description,
            at: Utc::now(),
            kind,
        };
        tokio::spawn(async move { alerts::dispatch(&client, &config, event).await });
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
        self.dispatch_alert(
            module_id,
            "module recovered after previously giving up".to_string(),
            alerts::CriticalEventKind::Recovered,
        );
    }

    fn rescan_new_modules(
        &self,
        slots: &mut HashMap<u32, SlotState>,
        tasks: &mut JoinSet<(u32, String)>,
        event_tx: &mpsc::UnboundedSender<BridgeEvent>,
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
        let mut policy = self.discovery_policy.lock().unwrap();
        let new_modules = match discovery::scan_modules_excluding_cards(
            &volte_ports,
            &volte_cards,
            &mut policy,
        ) {
            Ok(m) => m
                .into_iter()
                .filter(|m| !known_serials.contains(&m.serial_port))
                .collect::<Vec<_>>(),
            Err(e) => {
                tracing::debug!(error = %e, "USB rescan failed");
                return;
            }
        };
        drop(policy);

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

                    let cmd_tx = self.spawn_worker(tasks, &module, slot, event_tx);
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
                    slots.insert(slot, pending_slot(slot, module, resilience));
                }
            }
        }
    }
}

/// A slot for a module that failed to initialize: tracked, but parked on the
/// first backoff step until the timer tick retries it.
fn pending_slot(
    slot: u32,
    module: DiscoveredModule,
    resilience: &crate::config::ResilienceConfig,
) -> SlotState {
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
    }
}
