pub mod alsa_media_port;

use crate::config::{AppConfig, SipTransport, TlsVerify};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationState {
    Unregistered,
    Registering,
    Registered,
    Failed,
}

pub struct SipBridge {
    pub state: RegistrationState,
    config: SipBridgeConfig,
}

#[derive(Clone)]
struct SipBridgeConfig {
    server: String,
    port: u16,
    username: String,
    password: String,
    transport: SipTransport,
    local_port: u16,
    display_name: String,
    tls_verify: TlsVerify,
    dial_timeout_sec: u64,
    sip_destination: String,
}

impl SipBridge {
    pub fn new(config: &AppConfig) -> Self {
        let sip_config = SipBridgeConfig {
            server: config.sip.server.clone(),
            port: config.sip.port,
            username: config.sip.username.clone(),
            password: config.sip.password.expose_secret().clone(),
            transport: config.sip.transport.clone(),
            local_port: config.sip.local_port,
            display_name: config.sip.display_name.clone(),
            tls_verify: config.sip.tls_verify.clone(),
            dial_timeout_sec: config.bridge.sip_dial_timeout_sec,
            sip_destination: config.bridge.sip_destination.clone(),
        };

        Self {
            state: RegistrationState::Unregistered,
            config: sip_config,
        }
    }

    pub fn register(&mut self) -> Result<(), String> {
        self.state = RegistrationState::Registering;
        tracing::info!(
            server = %self.config.server,
            port = self.config.port,
            username = %self.config.username,
            transport = ?self.config.transport,
            "SIP registration initiated (PJSIP not yet wired)"
        );
        self.state = RegistrationState::Registered;
        crate::metrics::SIP_REGISTERED.set(1.0);
        crate::metrics::SIP_REGISTRATIONS_TOTAL
            .with_label_values(&["success"])
            .inc();
        Ok(())
    }

    pub fn compute_destination_uri(&self, caller_did: &str) -> String {
        let dest = if self.config.sip_destination.is_empty() {
            caller_did
        } else {
            &self.config.sip_destination
        };
        format!("sip:{}@{}:{}", dest, self.config.server, self.config.port)
    }

    pub fn unregister(&mut self) {
        self.state = RegistrationState::Unregistered;
        crate::metrics::SIP_REGISTERED.set(0.0);
        tracing::info!("SIP unregistered");
    }
}
