pub mod at_commander;
pub mod audio_pipeline;
pub mod beep;
pub mod card;
pub mod discovery;

use crate::config::AppConfig;
use crate::control::protocol::{ControlCmd, ControlResp, SlotInfo};
use crate::metrics;
use crate::modules::at_commander::{AtCommander, AtResponse, NetworkMode, NetworkType};
use crate::modules::card::{CardInstance, CardState};
use crate::modules::discovery::{scan_modules, DiscoveredModule};
use crate::sip::SipBridge;
use crate::sms::discord::DiscordClient;
use crate::sms::SmsHandler;
use crate::store::calls::CallRecord;
use crate::store::sms::{SmsForwardingByTimeUpdate, SmsRecord};
use crate::store::{StoreCommand, StoreHandle};
use chrono::Utc;
use std::collections::HashMap;
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
}

pub type ControlCmdSender = mpsc::Sender<(ControlCmd, oneshot::Sender<ControlResp>)>;
pub type ControlCmdReceiver = mpsc::Receiver<(ControlCmd, oneshot::Sender<ControlResp>)>;

enum ModuleCmd {
    SetMode(NetworkMode, oneshot::Sender<Result<NetworkMode, String>>),
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

pub struct CardPool {
    config: AppConfig,
    store: StoreHandle,
    sip_bridge: SipBridge,
    sms_handler: SmsHandler,
    discord_client: Option<DiscordClient>,
}

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

        Self {
            config,
            store,
            sip_bridge,
            sms_handler,
            discord_client,
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
            None => match scan_modules() {
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
                    };
                    let store_tx = self.store.sender();
                    let sms_enabled = self.sms_handler.is_enabled();
                    let module_clone = module.clone();
                    let evt_tx = event_tx.clone();
                    tasks.spawn_blocking(move || {
                        let sid = slot;
                        if let Err(e) =
                            run_module_loop(module_clone.clone(), store_tx, sms_enabled, evt_tx, cmd_rx)
                        {
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

        // USB rescan for hotplug reconnect (every 5 s)
        let mut rescan_deadline = tokio::time::Instant::now() + Duration::from_secs(5);

        loop {
            // Compute next retry deadline across all recovering/initializing slots
            let next_slot_retry = slots
                .values()
                .filter_map(|s| s.next_retry_at)
                .min()
                .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));

            let earliest_wakeup = next_slot_retry.min(rescan_deadline);

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
                    self.handle_control_cmd(cmd, reply, &mut slots, &resilience);
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
                                tasks.spawn_blocking(move || {
                                    if let Err(e) = run_module_loop(module_clone.clone(), store_tx, sms_enabled, evt_tx, cmd_rx) {
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
                        self.rescan_new_modules(&mut slots, &mut tasks, &event_tx);
                        rescan_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                    }

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

    fn rescan_new_modules(
        &self,
        slots: &mut HashMap<u32, SlotState>,
        tasks: &mut JoinSet<(u32, String)>,
        event_tx: &mpsc::UnboundedSender<BridgeEvent>,
    ) {
        let known_serials: std::collections::HashSet<PathBuf> = slots
            .values()
            .map(|s| s.module.serial_port.clone())
            .collect();

        let new_modules = match scan_modules() {
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
                    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<ModuleCmd>();
                    let store_tx = self.store.sender();
                    let sms_enabled = self.sms_handler.is_enabled();
                    let module_clone = module.clone();
                    let evt_tx = event_tx.clone();
                    tasks.spawn_blocking(move || {
                        if let Err(e) =
                            run_module_loop(module_clone.clone(), store_tx, sms_enabled, evt_tx, cmd_rx)
                        {
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
                        },
                    );
                }
            }
        }
    }

    fn handle_control_cmd(
        &self,
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
                if let Some(state) = slots.get_mut(&slot) {
                    tracing::info!(slot = slot, module = %state.module.id, "card restart requested");
                    state.cmd_tx = None;
                    state.lifecycle = LifecycleState::Initializing;
                    state.retry_count = 0;
                    state.next_retry_at = Some(tokio::time::Instant::now());
                    let _ = reply.send(ControlResp::ok());
                } else {
                    let max = slots.keys().max().copied().unwrap_or(0);
                    let _ = reply.send(ControlResp::err(format!(
                        "slot {slot} not found; valid slots: 0..={max}"
                    )));
                }
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
                    }
                }
            }
            BridgeEvent::Ring {
                module_id,
                caller_id,
                audio_device,
            } => {
                if self.sip_bridge.state != crate::sip::RegistrationState::Registered {
                    tracing::warn!(
                        module = %module_id,
                        "SIP not registered, cannot bridge call"
                    );
                    return;
                }

                let dest_uri = self.sip_bridge.compute_destination_uri(&caller_id);
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
                    return;
                }

                if let Err(e) = self.sip_bridge.make_call(&dest_uri, &caller_id) {
                    tracing::error!(
                        module = %module_id,
                        error = %e,
                        "SIP outbound call failed"
                    );
                    metrics::SIP_CALLS_TOTAL
                        .with_label_values(&[&module_id, "error"])
                        .inc();
                } else {
                    metrics::SIP_CALLS_TOTAL
                        .with_label_values(&[&module_id, "initiated"])
                        .inc();
                }
            }
            BridgeEvent::Hangup { module_id } => {
                tracing::info!(module = %module_id, "GSM call ended, tearing down SIP call");
                self.sip_bridge.hangup_active_call();
            }
            BridgeEvent::SmsReceived {
                module_id,
                sender,
                body,
                received_at,
            } => {
                if let Some(ref client) = self.discord_client {
                    let client = client.clone();
                    let store_tx = self.store.sender();
                    tokio::spawn(async move {
                        let result = client
                            .forward_sms(&module_id, &sender, &body, &received_at)
                            .await;
                        let (status_str, discord_code) = match &result {
                            Ok(code) => ("sent", Some(*code as i32)),
                            Err(_) => ("failed", None),
                        };
                        let _ = store_tx.send(StoreCommand::UpdateSmsForwardingByTime(
                            SmsForwardingByTimeUpdate {
                                module_id: module_id.clone(),
                                received_at: received_at.clone(),
                                forwarding_status: status_str.to_string(),
                                forwarded_at: Some(Utc::now().to_rfc3339()),
                                discord_status_code: discord_code,
                            },
                        ));
                        match result {
                            Ok(status) => {
                                tracing::info!(
                                    module = %module_id,
                                    status = status,
                                    "SMS forwarded to Discord"
                                );
                                metrics::SMS_FORWARDED_TOTAL
                                    .with_label_values(&[&module_id, "sent"])
                                    .inc();
                            }
                            Err(e) => {
                                tracing::warn!(
                                    module = %module_id,
                                    error = %e,
                                    "SMS Discord forwarding failed"
                                );
                                metrics::SMS_FORWARDED_TOTAL
                                    .with_label_values(&[&module_id, "failed"])
                                    .inc();
                            }
                        }
                    });
                }
            }
        }
    }
}

struct CallContext {
    caller_id: String,
    sip_destination: String,
    started_at: chrono::DateTime<Utc>,
}

fn run_module_loop(
    module: DiscoveredModule,
    store_tx: crossbeam_channel::Sender<StoreCommand>,
    _sms_enabled: bool,
    event_tx: mpsc::UnboundedSender<BridgeEvent>,
    cmd_rx: crossbeam_channel::Receiver<ModuleCmd>,
) -> Result<(), String> {
    let mut at = AtCommander::open(&module.serial_port).map_err(|e| e.to_string())?;

    at.send_command("ATE0").ok();
    at.send_command("AT+CLIP=1").ok();
    at.send_command("AT+CMGF=1").ok();
    at.send_command("AT+CNMI=2,1,0,0,0").ok();
    at.send_command("AT+CREG=1").ok();
    at.send_command("AT+CEREG=1").ok();
    route_audio_to_usb(&mut at, &module.id);

    if let Ok((rssi, _ber)) = at.check_signal() {
        tracing::info!(module = %module.id, rssi = rssi, "signal quality");
    }

    let mut card = CardInstance::new(
        module.id.clone(),
        module.serial_port.clone(),
        module.audio_device.clone(),
    );

    let mut call_ctx: Option<CallContext> = None;

    tracing::info!(module = %module.id, "module worker started, monitoring for events");
    metrics::ACTIVE_CALLS
        .with_label_values(&[&module.id])
        .set(0.0);

    loop {
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
            record_call_end(&module.id, &store_tx, &mut call_ctx, "answered");
            card.state = CardState::Idle;
            metrics::ACTIVE_CALLS
                .with_label_values(&[&module.id])
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
            handle_cmti(&module, &mut at, trimmed, &store_tx, &event_tx);
        } else if trimmed.starts_with("+CREG:") || trimmed.starts_with("+CEREG:") {
            handle_creg_urc(&module, trimmed, &event_tx);
        }
    }
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
        .with_label_values(&[&module.id, "incoming"])
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
                .with_label_values(&[&module.id])
                .set(1.0);
            metrics::CALLS_TOTAL
                .with_label_values(&[&module.id, "answered"])
                .inc();
        }
        Err(e) => {
            tracing::error!(module = %module.id, error = %e, "failed to answer call");
            card.state = CardState::Idle;
            metrics::CALLS_TOTAL
                .with_label_values(&[&module.id, "missed"])
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
            .with_label_values(&[&module.id])
            .set(0.0);
        let _ = event_tx.send(BridgeEvent::Hangup {
            module_id: module.id.clone(),
        });
        record_call_end(&module.id, store_tx, call_ctx, "answered");
    } else if card.state == CardState::Ringing {
        record_call_end(&module.id, store_tx, call_ctx, "missed");
    }
    card.state = CardState::Idle;
}

fn record_call_end(
    module_id: &str,
    store_tx: &crossbeam_channel::Sender<StoreCommand>,
    call_ctx: &mut Option<CallContext>,
    status: &str,
) {
    if let Some(ctx) = call_ctx.take() {
        let duration = Utc::now()
            .signed_duration_since(ctx.started_at)
            .num_seconds() as f64;

        metrics::CALL_DURATION_SECONDS
            .with_label_values(&[module_id])
            .observe(duration);

        let record = CallRecord {
            module_id: module_id.to_string(),
            caller_id: ctx.caller_id,
            started_at: ctx.started_at.to_rfc3339(),
            duration_seconds: duration,
            status: status.to_string(),
            sip_destination: ctx.sip_destination,
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
    store_tx: &crossbeam_channel::Sender<StoreCommand>,
    event_tx: &mpsc::UnboundedSender<BridgeEvent>,
) {
    tracing::info!(module = %module.id, notification = line, "SMS notification received");
    metrics::SMS_RECEIVED_TOTAL
        .with_label_values(&[&module.id])
        .inc();

    if let Some(idx_str) = line.split(',').next_back() {
        if let Ok(idx) = idx_str.trim().parse::<u32>() {
            let cmd = format!("AT+CMGR={idx}");
            match at.send_command(&cmd) {
                Ok(AtResponse::Ok(lines)) => {
                    tracing::debug!(module = %module.id, index = idx, lines = ?lines, "SMS read");

                    let (sender, body) = parse_sms_response(&lines);
                    let received_at = Utc::now().to_rfc3339();
                    let record = SmsRecord {
                        module_id: module.id.clone(),
                        sender: sender.clone(),
                        body: body.clone(),
                        received_at: received_at.clone(),
                        forwarding_status: "pending".to_string(),
                    };
                    if let Err(e) = store_tx.send(StoreCommand::InsertSms(record)) {
                        tracing::error!(error = %e, "failed to send SMS record to store");
                    }

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
}
