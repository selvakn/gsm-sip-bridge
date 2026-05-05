pub mod at_commander;
pub mod audio_pipeline;
pub mod beep;
pub mod card;
pub mod discovery;

use crate::config::AppConfig;
use crate::metrics;
use crate::modules::at_commander::{AtCommander, AtResponse};
use crate::modules::card::{CardInstance, CardState};
use crate::modules::discovery::{scan_modules, DiscoveredModule};
use crate::sip::SipBridge;
use crate::sms::discord::DiscordClient;
use crate::sms::SmsHandler;
use crate::store::calls::CallRecord;
use crate::store::sms::{SmsForwardingByTimeUpdate, SmsRecord};
use crate::store::StoreCommand;
use chrono::Utc;
use crossbeam_channel::Sender;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
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
}

pub struct CardPool {
    config: AppConfig,
    store_tx: Sender<StoreCommand>,
    sip_bridge: SipBridge,
    sms_handler: SmsHandler,
    discord_client: Option<DiscordClient>,
}

impl CardPool {
    pub fn new(
        config: AppConfig,
        store_tx: Sender<StoreCommand>,
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
            store_tx,
            sip_bridge,
            sms_handler,
            discord_client,
        }
    }

    pub async fn run(
        mut self,
        single_card: Option<(PathBuf, String)>,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<BridgeEvent>();

        if let Err(e) = self.sip_bridge.register() {
            tracing::error!(error = %e, "SIP registration failed — calls will not be bridged");
        }

        let modules = match single_card {
            Some((serial, audio)) => {
                let id = discovery::derive_module_id(
                    serial.file_name().unwrap_or_default().to_string_lossy().as_ref(),
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

        let mut active: Vec<DiscoveredModule> = Vec::new();
        let mut failed: Vec<DiscoveredModule> = Vec::new();
        let mut tasks = JoinSet::new();

        for module in modules {
            match self.try_init_module(&module) {
                Ok(()) => {
                    tracing::info!(module = %module.id, "module initialized");
                    metrics::MODULE_INIT_TOTAL
                        .with_label_values(&[&module.id, "success", ""])
                        .inc();
                    active.push(module.clone());
                    self.spawn_module_worker(&mut tasks, module, event_tx.clone());
                }
                Err(e) => {
                    tracing::warn!(module = %module.id, error = %e, "module init failed, will retry");
                    metrics::MODULE_INIT_TOTAL
                        .with_label_values(&[&module.id, "failure", &e.to_string()])
                        .inc();
                    failed.push(module);
                }
            }
        }

        metrics::MODULES_ACTIVE.set(active.len() as f64);
        metrics::MODULES_FAILED.set(failed.len() as f64);

        tracing::info!(
            active = active.len(),
            failed = failed.len(),
            "card pool running"
        );

        let retry_interval = Duration::from_secs(self.config.modules.retry_interval_sec);
        let mut retry_deadline = Instant::now() + retry_interval;

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    tracing::info!("card pool shutting down");
                    break;
                }
                Some(event) = event_rx.recv() => {
                    self.handle_bridge_event(event);
                }
                Some(result) = tasks.join_next() => {
                    match result {
                        Ok(module_id) => {
                            tracing::warn!(module = %module_id, "module worker exited, scheduling retry");
                            if let Some(pos) = active.iter().position(|m| m.id == module_id) {
                                let m = active.remove(pos);
                                failed.push(m);
                                metrics::MODULES_ACTIVE.set(active.len() as f64);
                                metrics::MODULES_FAILED.set(failed.len() as f64);
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "module worker panicked");
                        }
                    }
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(retry_deadline)) => {
                    if !failed.is_empty() {
                        let mut still_failed = Vec::new();
                        for module in failed.drain(..) {
                            metrics::MODULE_RETRIES_TOTAL.with_label_values(&[&module.id]).inc();
                            match self.try_init_module(&module) {
                                Ok(()) => {
                                    tracing::info!(module = %module.id, "module recovered on retry");
                                    metrics::MODULE_INIT_TOTAL
                                        .with_label_values(&[&module.id, "success", ""])
                                        .inc();
                                    active.push(module.clone());
                                    self.spawn_module_worker(&mut tasks, module, event_tx.clone());
                                }
                                Err(e) => {
                                    tracing::debug!(module = %module.id, error = %e, "retry failed");
                                    still_failed.push(module);
                                }
                            }
                        }
                        failed = still_failed;
                        metrics::MODULES_ACTIVE.set(active.len() as f64);
                        metrics::MODULES_FAILED.set(failed.len() as f64);
                    }
                    retry_deadline = Instant::now() + retry_interval;
                }
            }
        }

        self.sip_bridge.unregister();
        tasks.shutdown().await;
    }

    fn try_init_module(&self, module: &DiscoveredModule) -> Result<(), String> {
        if module.serial_port.as_os_str().is_empty() {
            return Err("serial port path not resolved".into());
        }
        let mut at = AtCommander::open(&module.serial_port).map_err(|e| e.to_string())?;
        match at.send_command("AT") {
            Ok(AtResponse::Ok(_)) => Ok(()),
            Ok(AtResponse::Error(e)) => Err(format!("AT probe returned ERROR: {e}")),
            Ok(AtResponse::CmeError(code, msg)) => {
                Err(format!("AT probe returned +CME ERROR {code}: {msg}"))
            }
            Err(e) => Err(format!("AT probe failed: {e}")),
        }
    }

    fn spawn_module_worker(
        &self,
        tasks: &mut JoinSet<String>,
        module: DiscoveredModule,
        event_tx: mpsc::UnboundedSender<BridgeEvent>,
    ) {
        let store_tx = self.store_tx.clone();
        let sms_enabled = self.sms_handler.is_enabled();
        let module_id = module.id.clone();

        tasks.spawn_blocking(move || {
            if let Err(e) = run_module_loop(module.clone(), store_tx, sms_enabled, event_tx) {
                tracing::error!(module = %module.id, error = %e, "module loop exited with error");
            }
            module_id
        });
    }

    fn handle_bridge_event(&mut self, event: BridgeEvent) {
        match event {
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
                    return;
                }

                if let Err(e) = self.sip_bridge.make_call(&dest_uri, &caller_id) {
                    tracing::error!(
                        module = %module_id,
                        error = %e,
                        "SIP outbound call failed"
                    );
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
                    let store_tx = self.store_tx.clone();
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
    store_tx: Sender<StoreCommand>,
    _sms_enabled: bool,
    event_tx: mpsc::UnboundedSender<BridgeEvent>,
) -> Result<(), String> {
    let mut at = AtCommander::open(&module.serial_port).map_err(|e| e.to_string())?;

    at.send_command("ATE0").ok();
    at.send_command("AT+CLIP=1").ok();
    at.send_command("AT+CMGF=1").ok();
    at.send_command("AT+CNMI=2,1,0,0,0").ok();

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
    metrics::ACTIVE_CALLS.with_label_values(&[&module.id]).set(0.0);

    loop {
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

        if (card.state == CardState::Bridged || card.state == CardState::Answering)
            && pjsua_safe::is_sip_peer_disconnected()
        {
            tracing::info!(module = %module.id, "SIP peer disconnected, hanging up GSM");
            let _ = at.hangup();
            record_call_end(&module.id, &store_tx, &mut call_ctx, "answered");
            card.state = CardState::Idle;
            metrics::ACTIVE_CALLS.with_label_values(&[&module.id]).set(0.0);
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
        }
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
            metrics::ACTIVE_CALLS.with_label_values(&[&module.id]).set(1.0);
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
    store_tx: &Sender<StoreCommand>,
    call_ctx: &mut Option<CallContext>,
) {
    if card.state == CardState::Bridged || card.state == CardState::Answering {
        tracing::info!(module = %module.id, "call ended (NO CARRIER)");
        metrics::ACTIVE_CALLS.with_label_values(&[&module.id]).set(0.0);
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
    store_tx: &Sender<StoreCommand>,
    call_ctx: &mut Option<CallContext>,
    status: &str,
) {
    if let Some(ctx) = call_ctx.take() {
        let duration = Utc::now()
            .signed_duration_since(ctx.started_at)
            .num_seconds() as f64;
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
    store_tx: &Sender<StoreCommand>,
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
