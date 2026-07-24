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
    /// Whether this circuit-switched bridge should register the SIP trunk.
    /// `false` when the VoLTE inbound bridge is active: it registers the *same*
    /// trunk account for its own outbound leg, and a trunk keeps a single
    /// binding, so two registrants of one account fight over it (and the churn
    /// can get the account auth-denied). The VoLTE bridge owns the trunk then.
    register_trunk: bool,
    dial_timeout_sec: u64,
    sip_destination: String,
    jb_init_ms: i32,
    jb_min_pre: i32,
    jb_max_ms: i32,
    vad_enabled: bool,
    tx_level: f32,
    snd_rec_latency_ms: u32,
    snd_play_latency_ms: u32,
    rt_audio_prio: u32,
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
            // Defer the trunk to the VoLTE inbound bridge when it is active, so
            // the same account is not registered from two places at once.
            register_trunk: !config.volte.bridge_inbound,
            dial_timeout_sec: config.bridge.sip_dial_timeout_sec,
            sip_destination: config.bridge.sip_destination.clone(),
            jb_init_ms: config.audio.settings.jb_init_ms,
            jb_min_pre: config.audio.settings.jb_min_pre,
            jb_max_ms: config.audio.settings.jb_max_ms,
            vad_enabled: config.audio.vad,
            tx_level: config.modem_audio.tx_level,
            snd_rec_latency_ms: config.audio.snd_rec_latency_ms,
            snd_play_latency_ms: config.audio.snd_play_latency_ms,
            rt_audio_prio: config.modem_audio.rt_audio_prio,
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
        // When the VoLTE inbound bridge owns the trunk, this circuit-switched
        // bridge must not also register the same account — see `register_trunk`.
        // Skip cleanly (no endpoint/account): the caller already tolerates an
        // unregistered bridge ("calls will not be bridged"), and this container
        // is doing VoLTE, not circuit-switched, work.
        if !self.config.register_trunk {
            self.state = RegistrationState::Unregistered;
            tracing::info!(
                server = %self.config.server,
                username = %self.config.username,
                "circuit-switched SIP trunk registration skipped: the VoLTE inbound \
                 bridge owns this trunk (avoids double-registering the same account)"
            );
            return Ok(());
        }
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
            // The circuit-switched bridge's audio comes off the modem's 8 kHz
            // USB sound device, so there is no wideband to preserve and no
            // reason to run the conference bridge any faster. (The VoWiFi
            // bridge's Agent B does run at 16 kHz — see `crate::vowifi`.)
            clock_rate: 8000,
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

        // Promote PJMEDIA's sound-device thread to real-time so the ALSA capture buffer is
        // serviced ahead of best-effort work (prevents XRUNs / choppy GSM audio). Opt-in
        // via [modem_audio] rt_audio_prio; best-effort, never fails the call path.
        if self.config.rt_audio_prio > 0 {
            // Prefix-match the PJMEDIA audio threads: the ALSA capture/playback I/O threads
            // (`alsasound_captu`/`alsasound_playb`, 15-char-truncated comm) plus the
            // `media`/`clock` timing threads. The capture thread is the one that matters
            // most for preventing GSM-leg overruns.
            let promoted = pjsua_safe::thread_prio::promote_threads_fifo(
                self.config.rt_audio_prio as i32,
                &["alsasound", "media", "clock"],
            );
            tracing::info!(
                prio = self.config.rt_audio_prio,
                promoted,
                "applied real-time scheduling to audio thread(s)"
            );
        }
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
