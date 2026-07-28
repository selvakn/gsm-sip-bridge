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

#[derive(Clone)]
pub struct DiscordClient {
    client: reqwest::Client,
    /// The client's own default webhook — used by `forward_sms` (SMS keeps
    /// its own dedicated webhook per FR-001's backward-compat rule).
    /// `send_alert` ignores this and always takes its target webhook
    /// explicitly, since a shared client now serves every category, each
    /// possibly resolving to a different webhook.
    webhook_url: Secret<String>,
}

impl DiscordClient {
    pub fn new(webhook_url: Secret<String>) -> BridgeResult<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| BridgeError::Sms(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            client,
            webhook_url,
        })
    }

    pub async fn forward_sms(
        &self,
        module_id: &str,
        sender: &str,
        body: &str,
        timestamp: &str,
    ) -> Result<u16, String> {
        let description = if body.len() > MAX_DESCRIPTION_LEN {
            format!("{}…", &body[..MAX_DESCRIPTION_LEN])
        } else {
            body.to_string()
        };

        let payload = serde_json::json!({
            "embeds": [{
                "title": format!("SMS from {sender}"),
                "description": description,
                "timestamp": timestamp,
                "color": 3447003,
                "fields": [
                    { "name": "Module", "value": module_id, "inline": true },
                    { "name": "Sender", "value": sender, "inline": true }
                ],
                "footer": { "text": "gsm-sip-bridge" }
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

        let payload = serde_json::json!({
            "embeds": [{
                "title": title,
                "description": description,
                "timestamp": event.at.to_rfc3339(),
                "color": color,
                "fields": fields,
                "footer": { "text": "gsm-sip-bridge" }
            }]
        });

        self.post_with_retry(webhook_url, payload).await
    }

    async fn post_with_retry(
        &self,
        webhook_url: &str,
        payload: serde_json::Value,
    ) -> Result<u16, String> {
        let start = std::time::Instant::now();
        let mut last_status = 0u16;

        for attempt in 0..=MAX_RETRIES {
            if start.elapsed() >= TOTAL_TIMEOUT {
                return Err("total timeout exceeded".into());
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
                            tokio::time::sleep(Duration::from_secs_f64(retry_after)).await;
                        }
                        400..=499 => {
                            let body = resp.text().await.unwrap_or_default();
                            tracing::warn!(
                                status = last_status,
                                body = %body.chars().take(256).collect::<String>(),
                                "Discord returned client error"
                            );
                            return Err(format!("client error {last_status}"));
                        }
                        _ => {
                            let backoff = Duration::from_secs(1 << attempt.min(3));
                            tokio::time::sleep(backoff).await;
                        }
                    }
                }
                Err(e) => {
                    if attempt == MAX_RETRIES {
                        return Err(format!("network error after retries: {e}"));
                    }
                    let backoff = Duration::from_secs(1 << attempt.min(3));
                    tokio::time::sleep(backoff).await;
                }
            }
        }

        Err(format!(
            "failed after {MAX_RETRIES} retries, last status: {last_status}"
        ))
    }
}

fn category_title(category: AlertCategory) -> &'static str {
    match category {
        AlertCategory::Sms => "SMS",
        AlertCategory::ModuleLifecycle => "Module/Modem Lifecycle Failure",
        AlertCategory::RegistrationLoss => "IMS/SIP Registration Loss",
        AlertCategory::TunnelFailure => "VoWiFi Tunnel Failure",
        AlertCategory::MissedCall => "Missed Call",
    }
}
