use crate::config::secret::Secret;
use crate::error::{BridgeError, BridgeResult};
use std::time::Duration;

const MAX_DESCRIPTION_LEN: usize = 4090;
const MAX_RETRIES: u32 = 3;
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = "gsm-sip-bridge/5.0.0";

#[derive(Clone)]
pub struct DiscordClient {
    client: reqwest::Client,
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

        let start = std::time::Instant::now();
        let mut last_status = 0u16;

        for attempt in 0..=MAX_RETRIES {
            if start.elapsed() >= TOTAL_TIMEOUT {
                return Err("total timeout exceeded".into());
            }

            let response = self
                .client
                .post(self.webhook_url.expose_secret())
                .json(&payload)
                .send()
                .await;

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
