//! The shared Discord webhook client for every alert category
//! (specs/022-discord-critical-alerts). `forward_sms` is the original
//! SMS-forwarding method (specs/006-sms-discord-forward), kept byte-for-byte
//! behaviorally unchanged (FR-001) — its embed shape is specific to SMS and
//! is not reused by `send_alert`. What *is* shared is the retry/backoff/
//! timeout POST loop (`post_with_retry`), which both methods call with
//! their own embed payload.

use crate::alerts::{AlertCategory, CriticalEvent, CriticalEventKind};
use crate::config::secret::Secret;
use crate::error::{BridgeError, BridgeResult};
use std::time::Duration;

const MAX_DESCRIPTION_LEN: usize = 4090;
const MAX_RETRIES: u32 = 3;
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = "gsm-sip-bridge/7.2.0";

/// Why a webhook POST ultimately failed. `Display` reproduces the exact
/// strings the string-typed error used to carry, so critical alerts (which
/// still surface `Result<u16, String>`) log identically; [`PostError::category`]
/// is the stable, secret-free value persisted for SMS retry bookkeeping
/// (specs/121-sms-alert-retry, R6) — the `Unreachable` detail is a raw
/// `reqwest` message that can contain the webhook URL and is never stored.
#[derive(Debug, Clone, PartialEq)]
pub enum PostError {
    /// Network-level failure on every attempt.
    Unreachable(String),
    /// The 30 s total budget ran out.
    Timeout,
    /// A non-retryable 4xx from Discord.
    Rejected(u16),
    /// Still 429 after the inline retries.
    RateLimited { retry_after: Duration },
    /// Still 5xx (or otherwise not 2xx) after the inline retries.
    ServerError(u16),
}

impl PostError {
    pub fn category(&self) -> String {
        match self {
            PostError::Unreachable(_) => "unreachable".into(),
            PostError::Timeout => "timeout".into(),
            PostError::Rejected(code) => format!("rejected:{code}"),
            PostError::RateLimited { .. } => "rate_limited".into(),
            PostError::ServerError(code) => format!("server_error:{code}"),
        }
    }

    /// The HTTP status behind the failure, when there was one.
    pub fn status_code(&self) -> Option<u16> {
        match self {
            PostError::Rejected(c) | PostError::ServerError(c) => Some(*c),
            PostError::RateLimited { .. } => Some(429),
            PostError::Unreachable(_) | PostError::Timeout => None,
        }
    }
}

impl std::fmt::Display for PostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PostError::Unreachable(e) => write!(f, "network error after retries: {e}"),
            PostError::Timeout => f.write_str("total timeout exceeded"),
            PostError::Rejected(code) => write!(f, "client error {code}"),
            PostError::RateLimited { .. } => {
                write!(f, "failed after {MAX_RETRIES} retries, last status: 429")
            }
            PostError::ServerError(code) => {
                write!(f, "failed after {MAX_RETRIES} retries, last status: {code}")
            }
        }
    }
}

impl std::error::Error for PostError {}

#[derive(Clone)]
pub struct DiscordClient {
    client: reqwest::Client,
    /// The client's own default webhook — used by `forward_sms` (SMS keeps
    /// its own dedicated webhook per FR-001's backward-compat rule).
    /// `send_alert` ignores this and always takes its target webhook
    /// explicitly, since a shared client now serves every category, each
    /// possibly resolving to a different webhook.
    webhook_url: Secret<String>,
    /// specs/034-alert-identity: the deployment instance label rendered in
    /// every embed footer — resolved once (`alerts::instance_label`) when the
    /// client is built. All split processes run on the same host, so each
    /// process's client carries the same value.
    instance: String,
}

impl DiscordClient {
    pub fn new(webhook_url: Secret<String>, instance: String) -> BridgeResult<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| BridgeError::Sms(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            client,
            webhook_url,
            instance,
        })
    }

    /// The embed footer shown on every alert: the shared product name plus this
    /// deployment's instance label (specs/034-alert-identity, FR-006).
    fn footer(&self) -> serde_json::Value {
        serde_json::json!({ "text": format!("gsm-sip-bridge · {}", self.instance) })
    }

    pub async fn forward_sms(
        &self,
        module_id: &str,
        sender: &str,
        body: &str,
        timestamp: &str,
        phone_number: Option<&str>,
        late: bool,
    ) -> Result<u16, PostError> {
        let description = if body.len() > MAX_DESCRIPTION_LEN {
            format!("{}…", &body[..MAX_DESCRIPTION_LEN])
        } else {
            body.to_string()
        };

        let mut fields = vec![
            serde_json::json!({ "name": "Module", "value": module_id, "inline": true }),
            serde_json::json!({ "name": "Sender", "value": sender, "inline": true }),
            serde_json::json!({ "name": "Phone", "value": phone_field(phone_number), "inline": true }),
        ];
        // A retried text differs from a fresh one only by the title prefix and
        // one extra field (contracts/discord-payload.md).
        let title = if late {
            fields.push(serde_json::json!({
                "name": "Originally received",
                "value": timestamp,
                "inline": false
            }));
            format!("[Late] SMS from {sender}")
        } else {
            format!("SMS from {sender}")
        };

        let payload = serde_json::json!({
            "embeds": [{
                "title": title,
                "description": description,
                "timestamp": timestamp,
                "color": 3447003,
                "fields": fields,
                "footer": self.footer()
            }]
        });

        self.post_with_retry(self.webhook_url.expose_secret(), payload)
            .await
    }

    /// Generic critical-event alert (specs/022-discord-critical-alerts),
    /// used by every category except `Sms`. `webhook_url` is the already
    /// -resolved target (`alerts::resolve_webhook`'s result) — this method
    /// never falls back to `self.webhook_url`, since the caller may resolve
    /// a different webhook per category/event.
    pub async fn send_alert(
        &self,
        webhook_url: &str,
        event: &CriticalEvent,
    ) -> Result<u16, String> {
        let (title_prefix, color) = match event.kind {
            CriticalEventKind::Failure => ("Critical", 15158332), // red
            CriticalEventKind::Recovered => ("Recovered", 3066993), // green
        };
        let title = format!("{title_prefix}: {}", category_title(event.category));

        let description = if event.description.len() > MAX_DESCRIPTION_LEN {
            format!("{}…", &event.description[..MAX_DESCRIPTION_LEN])
        } else {
            event.description.clone()
        };

        let mut fields = vec![serde_json::json!({
            "name": "Category",
            "value": event.category.as_str(),
            "inline": true
        })];
        if let Some(unit_id) = &event.unit_id {
            fields.push(serde_json::json!({
                "name": "Module/Line",
                "value": unit_id,
                "inline": true
            }));
        }
        fields.push(serde_json::json!({
            "name": "Phone",
            "value": phone_field(event.phone_number.as_deref()),
            "inline": true
        }));

        let payload = serde_json::json!({
            "embeds": [{
                "title": title,
                "description": description,
                "timestamp": event.at.to_rfc3339(),
                "color": color,
                "fields": fields,
                "footer": self.footer()
            }]
        });

        self.post_with_retry(webhook_url, payload)
            .await
            .map_err(|e| e.to_string())
    }

    async fn post_with_retry(
        &self,
        webhook_url: &str,
        payload: serde_json::Value,
    ) -> Result<u16, PostError> {
        let start = std::time::Instant::now();
        let mut last_status = 0u16;
        let mut last_retry_after = Duration::ZERO;

        for attempt in 0..=MAX_RETRIES {
            if start.elapsed() >= TOTAL_TIMEOUT {
                return Err(PostError::Timeout);
            }

            let response = self.client.post(webhook_url).json(&payload).send().await;

            match response {
                Ok(resp) => {
                    last_status = resp.status().as_u16();
                    match last_status {
                        200 | 204 => return Ok(last_status),
                        429 => {
                            let retry_after = resp
                                .headers()
                                .get("retry-after")
                                .and_then(|v| v.to_str().ok())
                                .and_then(|s| s.parse::<f64>().ok())
                                .unwrap_or(1.0);
                            last_retry_after = Duration::from_secs_f64(retry_after);
                            tokio::time::sleep(last_retry_after).await;
                        }
                        400..=499 => {
                            let body = resp.text().await.unwrap_or_default();
                            tracing::warn!(
                                status = last_status,
                                body = %body.chars().take(256).collect::<String>(),
                                "Discord returned client error"
                            );
                            return Err(PostError::Rejected(last_status));
                        }
                        _ => {
                            let backoff = Duration::from_secs(1 << attempt.min(3));
                            tokio::time::sleep(backoff).await;
                        }
                    }
                }
                Err(e) => {
                    if attempt == MAX_RETRIES {
                        return Err(PostError::Unreachable(e.to_string()));
                    }
                    let backoff = Duration::from_secs(1 << attempt.min(3));
                    tokio::time::sleep(backoff).await;
                }
            }
        }

        Err(if last_status == 429 {
            PostError::RateLimited {
                retry_after: last_retry_after,
            }
        } else {
            PostError::ServerError(last_status)
        })
    }
}

/// The value for an embed's `Phone` field (specs/034-alert-identity, FR-005):
/// the resolved number when present and non-empty, else the literal
/// `unknown` — never a fabricated or empty-looking value.
fn phone_field(phone_number: Option<&str>) -> String {
    match phone_number.map(str::trim) {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => crate::alerts::UNKNOWN_IDENTITY.to_string(),
    }
}

fn category_title(category: AlertCategory) -> &'static str {
    match category {
        AlertCategory::Sms => "SMS",
        AlertCategory::ModuleLifecycle => "Module/Modem Lifecycle Failure",
        AlertCategory::RegistrationLoss => "IMS/SIP Registration Loss",
        AlertCategory::TunnelFailure => "VoWiFi Tunnel Failure",
        AlertCategory::MissedCall => "Missed Call",
        AlertCategory::LineDiscoveryFailed => "VoWiFi Line Discovery Failed",
        AlertCategory::GmConnectionLost => "Carrier Signaling Connection Lost",
        AlertCategory::PcscfPoolExhausted => "VoWiFi P-CSCF Pool Exhausted",
    }
}
