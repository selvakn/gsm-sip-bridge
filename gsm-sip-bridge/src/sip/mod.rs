pub mod alsa_media_port;

use crate::config::{AppConfig, SipTransport, TlsVerify};
use pjsua_safe::{Account, AccountConfig, Call, Endpoint, EndpointConfig, TransportType};

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
    endpoint: Option<Endpoint>,
    account: Option<Account>,
    active_call: Option<Call>,
}

#[derive(Clone)]
#[allow(dead_code)]
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
    jb_init_ms: i32,
    jb_min_pre: i32,
    jb_max_ms: i32,
    vad_enabled: bool,
    tx_level: f32,
    snd_rec_latency_ms: u32,
    snd_play_latency_ms: u32,
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
            jb_init_ms: config.audio.settings.jb_init_ms,
            jb_min_pre: config.audio.settings.jb_min_pre,
            jb_max_ms: config.audio.settings.jb_max_ms,
            vad_enabled: config.audio.vad,
            tx_level: config.audio.tx_level,
            snd_rec_latency_ms: config.audio.snd_rec_latency_ms,
            snd_play_latency_ms: config.audio.snd_play_latency_ms,
        };

        Self {
            state: RegistrationState::Unregistered,
            config: sip_config,
            endpoint: None,
            account: None,
            active_call: None,
        }
    }

    pub fn register(&mut self) -> Result<(), String> {
        self.state = RegistrationState::Registering;

        let transport = match self.config.transport {
            SipTransport::Udp => TransportType::Udp,
            SipTransport::Tcp => TransportType::Tcp,
            SipTransport::Tls => TransportType::Tls,
        };

        let ep_config = EndpointConfig {
            transport,
            local_port: self.config.local_port,
            tls_verify: self.config.tls_verify == TlsVerify::Strict,
            jb_init_ms: self.config.jb_init_ms,
            jb_min_pre: self.config.jb_min_pre,
            jb_max_ms: self.config.jb_max_ms,
            vad_enabled: self.config.vad_enabled,
            tx_level: self.config.tx_level,
            snd_rec_latency_ms: self.config.snd_rec_latency_ms,
            snd_play_latency_ms: self.config.snd_play_latency_ms,
        };

        let endpoint = Endpoint::create(ep_config).map_err(|e| {
            self.state = RegistrationState::Failed;
            crate::metrics::SIP_REGISTRATIONS_TOTAL
                .with_label_values(&["failure"])
                .inc();
            format!("PJSIP endpoint creation failed: {e}")
        })?;

        let acc_config = AccountConfig {
            sip_server: self.config.server.clone(),
            sip_port: self.config.port,
            username: self.config.username.clone(),
            password: self.config.password.clone(),
            display_name: self.config.display_name.clone(),
        };

        let account = Account::register(&endpoint, acc_config, None).map_err(|e| {
            self.state = RegistrationState::Failed;
            crate::metrics::SIP_REGISTRATIONS_TOTAL
                .with_label_values(&["failure"])
                .inc();
            format!("SIP account registration failed: {e}")
        })?;

        tracing::info!(
            server = %self.config.server,
            port = self.config.port,
            username = %self.config.username,
            transport = ?self.config.transport,
            "SIP registered"
        );

        self.endpoint = Some(endpoint);
        self.account = Some(account);
        self.state = RegistrationState::Registered;
        crate::metrics::SIP_REGISTERED.set(1.0);
        crate::metrics::SIP_REGISTRATIONS_TOTAL
            .with_label_values(&["success"])
            .inc();
        Ok(())
    }

    pub fn compute_destination_uri(&self, caller_did: &str) -> String {
        let raw_dest = if self.config.sip_destination.is_empty() {
            caller_did
        } else {
            &self.config.sip_destination
        };
        let dest = raw_dest.trim_start_matches('+');
        format!("sip:{}@{}:{}", dest, self.config.server, self.config.port)
    }

    pub fn set_sound_device(&self, alsa_device: &str) -> Result<(), String> {
        let endpoint = self
            .endpoint
            .as_ref()
            .ok_or_else(|| "PJSIP endpoint not initialized".to_string())?;

        // Diagnostic: confirm the EC20 capture device can run natively at PJMEDIA's
        // 8 kHz clock. If not, pjmedia silently resamples, which introduces the
        // high-frequency imaging artefacts heard as "noise" on the GSM leg.
        verify_native_rate(alsa_device, 8000);

        let dev_index = endpoint
            .find_audio_device(alsa_device)
            .map_err(|e| format!("{e}"))?;

        endpoint
            .set_sound_device(dev_index, dev_index)
            .map_err(|e| format!("{e}"))?;

        tracing::info!(alsa = %alsa_device, pjsip_dev = dev_index, "sound device set");
        Ok(())
    }

    pub fn make_call(&mut self, dest_uri: &str, gsm_caller_id: &str) -> Result<(), String> {
        let account = self
            .account
            .as_ref()
            .ok_or_else(|| "no SIP account registered".to_string())?;

        let mut headers: Vec<(&str, &str)> = Vec::new();
        let pai_value;
        if !gsm_caller_id.is_empty() {
            pai_value = format!("\"{}\" <tel:{}>", gsm_caller_id, gsm_caller_id);
            headers.push(("P-Asserted-Identity", &pai_value));
            headers.push(("X-GSM-Caller-ID", gsm_caller_id));
        }

        let call = Call::make(account, dest_uri, None, &headers).map_err(|e| format!("{e}"))?;
        tracing::info!(
            dest = %dest_uri,
            call_id = call.call_id(),
            gsm_caller = %gsm_caller_id,
            "SIP outbound call initiated"
        );
        self.active_call = Some(call);
        Ok(())
    }

    pub fn hangup_active_call(&mut self) {
        if let Some(ref mut call) = self.active_call {
            if let Err(e) = call.hangup() {
                tracing::warn!(error = %e, "failed to hangup SIP call");
            }
        }
        self.active_call = None;
    }

    pub fn unregister(&mut self) {
        self.hangup_active_call();
        if let Some(ref mut account) = self.account {
            account.unregister();
        }
        self.account = None;
        self.endpoint = None;
        self.state = RegistrationState::Unregistered;
        crate::metrics::SIP_REGISTERED.set(0.0);
        tracing::info!("SIP unregistered");
    }
}

/// Best-effort check that `device` supports `expected_rate` (Hz) natively for capture.
///
/// PJMEDIA runs the sound device at 8 kHz; if the EC20 USB-audio device only offers a
/// different native rate, pjmedia resamples on the fly and the GSM-leg audio picks up
/// high-frequency imaging artefacts. This logs a WARN so the mismatch is visible in the
/// monitoring stack instead of being silently masked. Never fails the call path.
fn verify_native_rate(device: &str, expected_rate: u32) {
    use alsa::pcm::{HwParams, PCM};
    use alsa::Direction;

    let pcm = match PCM::new(device, Direction::Capture, false) {
        Ok(p) => p,
        Err(e) => {
            // Device busy (already opened) or unusual name — non-fatal.
            tracing::debug!(device, error = %e, "native-rate check: could not open capture device");
            return;
        }
    };
    let hwp = match HwParams::any(&pcm) {
        Ok(h) => h,
        Err(e) => {
            tracing::debug!(device, error = %e, "native-rate check: HwParams::any failed");
            return;
        }
    };
    let min = hwp.get_rate_min().ok();
    let max = hwp.get_rate_max().ok();
    match (min, max) {
        (Some(lo), Some(hi)) => {
            let supported = expected_rate >= lo && expected_rate <= hi;
            if supported {
                tracing::info!(
                    device,
                    expected_rate,
                    rate_min = lo,
                    rate_max = hi,
                    "capture device supports the PJMEDIA clock rate natively"
                );
            } else {
                tracing::warn!(
                    device,
                    expected_rate,
                    rate_min = lo,
                    rate_max = hi,
                    "capture device does NOT support the PJMEDIA clock rate natively; \
                     pjmedia will resample and may introduce high-frequency artefacts on the GSM leg"
                );
            }
        }
        _ => {
            tracing::debug!(
                device,
                "native-rate check: device did not report a rate range"
            );
        }
    }
}
