pub mod discord;
pub mod reader;
pub mod retry;

use crate::config::SmsConfig;
use crate::control::protocol::{AgentState, ObservedEvent, SmsOutcome};
use crate::metrics;
use crate::observability::reporter::Reporter;
use crate::store::sms::{SmsForwardingByTimeUpdate, SmsRecord};
use crate::store::{StoreCommand, Transport};
use crossbeam_channel::Sender;
pub use retry::SmsForwarder;
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
/// `transport` they pass. `module_id` is the same card identity on both
/// paths when the modem serving VoWiFi also does circuit-switched voice
/// (specs/014-vowifi-metrics-restore, FR-011a) — the circuit-switched
/// caller resolves it via `modules::discovery::derive_module_id`, the
/// VoWiFi caller uses the `card_id` `main.rs`/`discover` already resolved
/// at line-discovery time (specs/013-multi-card-vowifi), so this function
/// never has to know which transport it is.
///
/// `SMS_RECEIVED_TOTAL` is deliberately *not* incremented here: the
/// circuit-switched caller counts a receipt at the `+CMTI` notification
/// (`modules::mod::handle_cmti`), before this function is ever reached, and
/// double-counting it here would be wrong for that path. The VoWiFi caller
/// counts it at the point Agent A's `MESSAGE` is first relayed.
///
/// `vowifi_reporter` is `Some` only on the VoWiFi path. The forwarding
/// outcome below must not be recorded via a direct `metrics::` call in that
/// case: this function runs inside Agent B's own process, which owns no
/// scrape endpoint of its own (specs/014-vowifi-metrics-restore) — a direct
/// `metrics::SMS_FORWARDED_TOTAL.inc()` here would land in a Prometheus
/// registry nothing ever reads, exactly the bug this feature exists to fix.
/// The circuit-switched caller passes `None` and keeps using the direct
/// call, since for that path the local registry *is* the scraped one.
///
/// Takes `handle` explicitly rather than assuming an ambient Tokio context
/// via `tokio::spawn`: `modules::mod` calls this from its own async event
/// loop (`Handle::current()`), while `vowifi::mod`'s accept loop is plain
/// synchronous code with its own dedicated runtime built just for this.
#[allow(clippy::too_many_arguments)]
pub fn record_and_forward(
    handle: &Handle,
    store_tx: Sender<StoreCommand>,
    forwarder: Option<SmsForwarder>,
    module_id: String,
    sender: String,
    body: String,
    received_at: String,
    transport: Transport,
    vowifi_reporter: Option<Reporter>,
    phone_number: Option<String>,
) {
    let record = SmsRecord {
        module_id: module_id.clone(),
        sender: sender.clone(),
        body: body.clone(),
        received_at: received_at.clone(),
        // Rows nobody will attempt are `skipped` from the start, so a webhook
        // configured later never resurrects them (FR-015).
        forwarding_status: if forwarder.is_some() {
            "pending"
        } else {
            "skipped"
        }
        .to_string(),
        transport,
        retry_eligible: forwarder.as_ref().is_some_and(|f| f.retry_enabled),
    };
    if let Err(e) = store_tx.send(StoreCommand::InsertSms(record)) {
        tracing::error!(error = %e, "failed to send SMS record to store");
    }

    let Some(forwarder) = forwarder else {
        return;
    };
    if let Some(phone) = phone_number.as_deref().filter(|p| !p.trim().is_empty()) {
        forwarder.remember_phone(&module_id, phone);
    }

    handle.spawn(async move {
        let result = forwarder
            .client
            .forward_sms(
                &module_id,
                &sender,
                &body,
                &received_at,
                phone_number.as_deref(),
                false,
            )
            .await;
        let (status_str, discord_code, last_error) = match &result {
            Ok(code) => ("sent", Some(*code as i32), None),
            Err(e) => ("failed", e.status_code().map(i32::from), Some(e.category())),
        };
        let _ = store_tx.send(StoreCommand::UpdateSmsForwardingByTime(
            SmsForwardingByTimeUpdate {
                module_id: module_id.clone(),
                received_at: received_at.clone(),
                forwarding_status: status_str.to_string(),
                forwarded_at: Some(chrono::Utc::now().to_rfc3339()),
                discord_status_code: discord_code,
                last_error,
            },
        ));
        let outcome = match result {
            Ok(status) => {
                tracing::info!(module = %module_id, status = status, "SMS forwarded to Discord");
                SmsOutcome::Sent
            }
            Err(e) => {
                tracing::warn!(module = %module_id, error = %e, "SMS Discord forwarding failed");
                SmsOutcome::Failed
            }
        };
        report_outcome(&module_id, transport, outcome, &vowifi_reporter);

        // The "next alert" retry trigger (FR-003). Only after this text's own
        // first attempt has finished, so a fresh text never waits behind a
        // backlog (FR-009). The writer thread may not have cleared this row's
        // lease yet, so the row that just failed is excluded explicitly: it
        // waits for the next alert or timer tick.
        retry::sweep_excluding(
            &forwarder,
            &vowifi_reporter,
            Some((&module_id, &received_at)),
        )
        .await;
    });
}

/// Counts one forwarding attempt. The VoWiFi path goes through its reporter
/// because Agent B owns no scrape endpoint (see [`record_and_forward`]); the
/// circuit-switched path increments the local, scraped registry directly.
pub(crate) fn report_outcome(
    module_id: &str,
    transport: Transport,
    outcome: SmsOutcome,
    vowifi_reporter: &Option<Reporter>,
) {
    match vowifi_reporter {
        Some(reporter) => {
            reporter.report(
                AgentState::default(),
                vec![ObservedEvent::SmsForwarded { outcome }],
            );
        }
        None => {
            let outcome_str = match outcome {
                SmsOutcome::Sent => "sent",
                SmsOutcome::Failed => "failed",
            };
            metrics::SMS_FORWARDED_TOTAL
                .with_label_values(&[module_id, outcome_str, transport.as_str()])
                .inc();
        }
    }
}
