pub mod discord;
pub mod reader;

use crate::config::SmsConfig;
use crate::store::StoreCommand;
use crossbeam_channel::Sender;

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
