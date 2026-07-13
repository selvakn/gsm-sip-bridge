pub mod discord;
pub mod reader;

use crate::config::SmsConfig;
use crate::metrics;
use crate::store::sms::{SmsForwardingByTimeUpdate, SmsRecord};
use crate::store::StoreCommand;
use crossbeam_channel::Sender;
use discord::DiscordClient;
use tokio::runtime::Handle;

pub struct SmsHandler {
    enabled: bool,
    webhook_url: String,
    store_tx: Sender<StoreCommand>,
}

impl SmsHandler {
    pub fn new(config: &SmsConfig, store_tx: Sender<StoreCommand>) -> Self {
        let webhook_url = config.discord_webhook_url.expose_secret().clone();
        if !config.enabled {
            tracing::info!("SMS monitoring disabled via configuration");
        } else if webhook_url.is_empty() {
            tracing::info!("SMS forwarding disabled (no webhook URL configured); messages will be persisted only");
        }

        Self {
            enabled: config.enabled,
            webhook_url,
            store_tx,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn has_webhook(&self) -> bool {
        !self.webhook_url.is_empty()
    }

    pub fn store_sender(&self) -> Sender<StoreCommand> {
        self.store_tx.clone()
    }
}

/// Persists an inbound SMS as `forwarding_status = "pending"` and, if a
/// `DiscordClient` is configured, forwards it and updates that same row's
/// forwarding status once the attempt completes. Shared by the
/// circuit-switched AT-command flow (`modules::mod`'s
/// `BridgeEvent::SmsReceived` handler) and the VoWiFi SIP `MESSAGE` flow
/// (`vowifi::mod`), so both transports land in one `sms` table with
/// identical semantics — the two flows differ only in what puts a `(sender,
/// body, received_at)` triple in front of this function, and in the
/// `module_id` they pass (a real GSM card id vs. VoWiFi's fixed label).
///
/// Takes `handle` explicitly rather than assuming an ambient Tokio context
/// via `tokio::spawn`: `modules::mod` calls this from its own async event
/// loop (`Handle::current()`), while `vowifi::mod`'s accept loop is plain
/// synchronous code with its own dedicated runtime built just for this.
pub fn record_and_forward(
    handle: &Handle,
    store_tx: Sender<StoreCommand>,
    discord_client: Option<DiscordClient>,
    module_id: String,
    sender: String,
    body: String,
    received_at: String,
) {
    let record = SmsRecord {
        module_id: module_id.clone(),
        sender: sender.clone(),
        body: body.clone(),
        received_at: received_at.clone(),
        forwarding_status: "pending".to_string(),
    };
    if let Err(e) = store_tx.send(StoreCommand::InsertSms(record)) {
        tracing::error!(error = %e, "failed to send SMS record to store");
    }

    let Some(client) = discord_client else {
        return;
    };

    handle.spawn(async move {
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
                forwarded_at: Some(chrono::Utc::now().to_rfc3339()),
                discord_status_code: discord_code,
            },
        ));
        match result {
            Ok(status) => {
                tracing::info!(module = %module_id, status = status, "SMS forwarded to Discord");
                metrics::SMS_FORWARDED_TOTAL
                    .with_label_values(&[&module_id, "sent"])
                    .inc();
            }
            Err(e) => {
                tracing::warn!(module = %module_id, error = %e, "SMS Discord forwarding failed");
                metrics::SMS_FORWARDED_TOTAL
                    .with_label_values(&[&module_id, "failed"])
                    .inc();
            }
        }
    });
}
