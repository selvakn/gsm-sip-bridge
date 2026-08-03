pub mod alsa_media_port;
pub mod outbound;
pub mod server;
pub mod target;

use std::sync::Arc;

use crate::config::{AppConfig, SipServerConfig, SipTransport, TlsVerify};
use pjsua_safe::{
    Account, AccountConfig, Call, CallState, Endpoint, EndpointConfig, TransportType,
};

use server::{BindingStore, Registrar};

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
    /// Present only in SIP server mode: the registrar this process hosts, and
    /// the table of phones it has accepted.
    registrar: Option<Registrar>,
    bindings: Option<Arc<BindingStore>>,
    /// Present only in PBX-trunk mode (`bindings.is_none()`): the configured
    /// trunk server's own resolved IP address(es), checked against a
    /// dial-out request's real source in `poll_outbound_request` — the
    /// trunk-mode analog of `bindings`' `find_by_source` (spec 025 review:
    /// nothing verified the sender of a request to this account's port at
    /// all before this field existed). Resolved once at registration time,
    /// not per-request — `ToSocketAddrs` can do a real (blocking) DNS
    /// lookup, and this is checked from the same async loop that must keep
    /// serving everything else.
    trunk_source_ips: Vec<std::net::IpAddr>,
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
    /// Whether this circuit-switched bridge owns the telephony-facing SIP
    /// identity at all.
    ///
    /// `false` when the VoLTE inbound bridge or the VoWiFi bridge is active:
    /// either one drives the *same* PBX-facing leg for its own calls, from a
    /// different process. With a PBX that matters because a trunk keeps a
    /// single binding, so two registrants of one account fight over it (and the
    /// churn can get the account auth-denied). In SIP server mode it matters
    /// for the same reason in a different shape: two registrars cannot bind one
    /// UDP port, so exactly one process may host it — and reusing this flag to
    /// decide which is what lets one registrar serve every call path with no
    /// IPC (spec 024, research.md R-003).
    owns_sip_side: bool,
    /// Whether to actually send a REGISTER. `false` in SIP server mode, where
    /// there is no trunk to register to at all — but that case still needs the
    /// endpoint for the leg it dials out on, so it cannot simply skip.
    register_trunk: bool,
    /// The SIP server mode's settings.
    sip_server: SipServerConfig,
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
    /// `[outbound].enabled` (spec 025) — passed to the registrar so a
    /// registered phone's INVITE is redirected instead of refused.
    outbound_enabled: bool,
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
            // Defer the trunk to the VoLTE inbound bridge or the VoWiFi bridge
            // when either is active, so the same account is not registered
            // from two places at once.
            owns_sip_side: !config.volte.bridge_inbound && !config.vowifi.enabled,
            register_trunk: !config.volte.bridge_inbound
                && !config.vowifi.enabled
                && !config.sip_server.enabled,
            sip_server: config.sip_server.clone(),
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
            outbound_enabled: config.outbound.enabled,
        };

        Self {
            state: RegistrationState::Unregistered,
            config: sip_config,
            endpoint: None,
            account: None,
            active_call: None,
            registrar: None,
            bindings: None,
            trunk_source_ips: Vec::new(),
        }
    }

    /// Whether this bridge is serving IP phones rather than calling a PBX.
    pub fn is_server_mode(&self) -> bool {
        self.config.sip_server.enabled
    }

    pub fn register(&mut self) -> Result<(), String> {
        // When the VoLTE or VoWiFi bridge owns the telephony-facing side, this
        // circuit-switched bridge must stand down entirely — see
        // `owns_sip_side`. Skip cleanly (no endpoint/account): the caller
        // already tolerates an unregistered bridge ("calls will not be
        // bridged"), and this container is doing VoLTE/VoWiFi, not
        // circuit-switched, work.
        //
        // Checked before the server-mode branch, not after. The other way round,
        // a VoWiFi deployment with server mode on would start a registrar here
        // *and* one in the telephony agent, and the second to bind would fail.
        if !self.config.owns_sip_side {
            self.state = RegistrationState::Unregistered;
            tracing::info!(
                server = %self.config.server,
                username = %self.config.username,
                server_mode = self.config.sip_server.enabled,
                "circuit-switched SIP side skipped: the VoLTE/VoWiFi bridge owns it \
                 (avoids double-registering one trunk, or two registrars on one port)"
            );
            return Ok(());
        }

        // SIP server mode: no trunk to register, but we still need the endpoint
        // for the leg we dial out on, so this cannot take the skip above.
        if self.config.sip_server.enabled {
            return self.start_server_mode();
        }

        self.state = RegistrationState::Registering;

        let endpoint = Endpoint::create(self.endpoint_config()).map_err(|e| {
            self.state = RegistrationState::Failed;
            crate::metrics::SIP_REGISTRATIONS_TOTAL
                .with_label_values(&["failure"])
                .inc();
            format!("PJSIP endpoint creation failed: {e}")
        })?;
        // Without this, an unsolicited INVITE to the trunk account queues
        // forever with no response when outbound dialing is off — nothing
        // in this process ever calls `poll_outbound_request` to drain it
        // (specs/025-outbound-calling review).
        endpoint.set_accept_incoming_calls(self.config.outbound_enabled);

        // Resolved once, here, not per-request — see `trunk_source_ips`'s
        // own doc comment. A literal IP resolves locally with no network
        // call; a hostname does a real (one-time) DNS lookup. Failure
        // leaves this empty, which fails closed: `poll_outbound_request`
        // trusts nothing until a restart re-resolves successfully, rather
        // than silently trusting every sender.
        if self.config.outbound_enabled {
            use std::net::ToSocketAddrs;
            match (self.config.server.as_str(), self.config.port).to_socket_addrs() {
                Ok(addrs) => self.trunk_source_ips = addrs.map(|a| a.ip()).collect(),
                Err(e) => {
                    tracing::warn!(
                        server = %self.config.server,
                        error = %e,
                        "outbound: could not resolve the trunk server's address; \
                         dial-out requests will be refused until this succeeds"
                    );
                }
            }
        }

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

    /// The pjsua endpoint settings, shared by both modes — server mode differs
    /// only in what it registers, not in how the media stack is configured.
    fn endpoint_config(&self) -> EndpointConfig {
        let transport = match self.config.transport {
            SipTransport::Udp => TransportType::Udp,
            SipTransport::Tcp => TransportType::Tcp,
            SipTransport::Tls => TransportType::Tls,
        };
        EndpointConfig {
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
        }
    }

    /// Builds the endpoint, starts the registrar, and creates the
    /// non-registering account calls are placed from.
    fn start_server_mode(&mut self) -> Result<(), String> {
        self.state = RegistrationState::Registering;
        let server = self.config.sip_server.clone();

        let endpoint = Endpoint::create(self.endpoint_config()).map_err(|e| {
            self.state = RegistrationState::Failed;
            format!("PJSIP endpoint creation failed: {e}")
        })?;
        // See the same call in `start` above — SIP-server mode reaches
        // `poll_outbound_request` through this same account too.
        endpoint.set_accept_incoming_calls(self.config.outbound_enabled);

        // Bind before anything else can fail: a port clash here is the most
        // likely startup problem, and the operator needs it named plainly.
        let outbound_local_port = self
            .config
            .outbound_enabled
            .then_some(self.config.local_port);
        let registrar =
            Registrar::start_observed(&server, outbound_local_port, None).map_err(|e| {
                self.state = RegistrationState::Failed;
                format!(
                    "SIP registrar could not listen on {}:{}: {e}",
                    server.listen_addr, server.listen_port
                )
            })?;

        // The identity the handset sees calls arrive from, so it must name the
        // bridge as the phone knows it — the registrar's own address.
        let id_uri = server.identity_uri();
        let account = Account::local(&endpoint, &id_uri, &self.config.display_name)
            .map_err(|e| format!("local SIP account creation failed: {e}"))?;

        tracing::info!(
            listen = %format!("{}:{}", server.listen_addr, server.listen_port),
            uac_port = self.config.local_port,
            ring_aor = %server.ring_aor,
            accounts = server.accounts.len(),
            "SIP server mode active — IP phones register here; no PBX is used"
        );

        self.bindings = Some(registrar.bindings());
        self.registrar = Some(registrar);
        self.endpoint = Some(endpoint);
        self.account = Some(account);
        self.state = RegistrationState::Registered;
        Ok(())
    }

    /// Where a call from `caller_did` should go.
    ///
    /// Fallible because in server mode the phone may not be registered — the
    /// PBX case still cannot fail.
    pub fn compute_destination_uri(&self, caller_did: &str) -> Result<String, String> {
        let target = match &self.bindings {
            Some(bindings) => target::CallTarget::RegisteredPhone {
                bindings,
                aor: &self.config.sip_server.ring_aor,
            },
            // Server mode with no binding table means this process is not the
            // one hosting the registrar, so it has nowhere to send a call. The
            // `[sip]` fields would be empty here (they are forbidden in this
            // mode), and falling through to the PBX form would build a URI
            // pointing at nothing rather than saying so.
            None if self.config.sip_server.enabled => {
                return Err(
                    "SIP server mode is enabled but this process does not host the registrar \
                     — the VoLTE/VoWiFi bridge does"
                        .to_string(),
                )
            }
            None => target::CallTarget::Pbx {
                server: &self.config.server,
                port: self.config.port,
                sip_destination: &self.config.sip_destination,
            },
        };
        target.uri_for(caller_did, std::time::Instant::now())
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
        // `active_call` is one field for the whole bridge, not one per
        // modem — a pre-existing simplification (predates spec 025) that a
        // true multi-modem-concurrent-call design would need to revisit on
        // its own. Spec 025 adds a *new* way to hit it though: without this
        // check, a modem answering a call while an outbound call is already
        // active here would silently overwrite `active_call`, losing that
        // outbound call's own SIP-side handle while its real carrier leg
        // stays up with nothing left tracking it (found in review — the
        // same root cause `poll_outbound_request`'s own busy check already
        // guards against in the other direction).
        if self.active_call.is_some() {
            return Err("a call is already active on this SIP bridge".to_string());
        }
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

    /// Pops one not-yet-claimed inbound INVITE, if any, and extracts its
    /// destination (spec 025, `sip::outbound`). Only meaningful when this
    /// process owns the SIP side and `[outbound]` is enabled — callers are
    /// expected to check `[outbound].enabled` themselves before polling, the
    /// same way `[sip_server].enabled` gates the registrar.
    ///
    /// A call whose destination cannot be extracted (see
    /// `Call::request_destination`'s caveats) is refused here with `400`
    /// and never handed to the caller — there is nothing useful the rest of
    /// the outbound pipeline could do with it.
    pub fn poll_outbound_request(&self) -> Option<(Call, String)> {
        let endpoint = self.endpoint.as_ref()?;
        let (_, call_id, source_addr) = endpoint.poll_incoming_call()?;
        let mut call = Call::from_id(call_id, CallState::Incoming);
        // This account's port listens on every interface (real phones and
        // PBXes reach it directly, not just loopback), and nothing upstream
        // has checked who actually sent this INVITE — a bare SIP header is
        // caller-chosen text, not proof of anything. Verify the real
        // transport-level source before this request can reach a real
        // carrier call at all (found in review): in SIP server mode, it
        // must be a currently-registered phone (the same check the
        // registrar's own redirect decision makes); in trunk mode, it must
        // be the configured PBX itself.
        let trusted = match &self.bindings {
            Some(bindings) => bindings
                .find_by_source(source_addr, std::time::Instant::now())
                .is_some(),
            None => self.trunk_source_ips.contains(&source_addr.ip()),
        };
        if !trusted {
            tracing::warn!(call_id, %source_addr, "outbound: refusing a dial-out request from an untrusted source");
            let _ = call.answer(403);
            // No dedicated outcome exists for "untrusted source" — this
            // path should never be hit by a legitimate caller, so reusing
            // the closest existing label (a refused, never-placed attempt)
            // rather than widening the wire protocol for it.
            crate::metrics::OUTBOUND_ATTEMPTS_TOTAL
                .with_label_values(&["refused_invalid_destination"])
                .inc();
            return None;
        }
        if self.active_call.is_some() {
            // Refuse rather than queue: this is polled every tick
            // regardless of whether a previous call is still being
            // handled. Leaving it queued would mean dialing it — possibly
            // minutes later, for a caller who's since given up — once the
            // current call ends; accepting it right away would silently
            // clobber `active_call` (a single field, not one per line),
            // losing the current call's own SIP-side handle
            // (specs/025-outbound-calling review). `active_call` is shared
            // with the *inbound* direction too (`make_call`, an ongoing
            // GSM-to-PBX call on some other, otherwise-idle modem slot) —
            // this same check is what stops `handle_outbound_request`'s
            // purely-per-slot `has_active_call` selection from ever
            // reaching `accept_outbound` and clobbering it (a second,
            // independently-found review issue with the same root cause
            // and the same fix).
            tracing::warn!(call_id, "outbound: busy with another call, refusing");
            let _ = call.answer(503);
            // This terminal point never reaches `handle_outbound_request`
            // (there is no `(Call, String)` pair to hand it), so it's the
            // only place this outcome can be counted (specs/025-outbound-calling
            // review, T048).
            crate::metrics::OUTBOUND_ATTEMPTS_TOTAL
                .with_label_values(&["refused_no_idle_line"])
                .inc();
            return None;
        }
        match call.request_destination() {
            Some(destination) => Some((call, destination)),
            None => {
                tracing::warn!(
                    call_id,
                    "outbound: could not determine a destination for this call, refusing"
                );
                let _ = call.answer(400);
                // `handle_outbound_request` never runs for this case (there
                // is no `(Call, String)` pair to hand it) — the CS path's
                // own terminal-point increment site — so this is the only
                // place this outcome can be counted at all (specs/025-outbound-calling
                // review, T048).
                crate::metrics::OUTBOUND_ATTEMPTS_TOTAL
                    .with_label_values(&["refused_invalid_destination"])
                    .inc();
                None
            }
        }
    }

    /// Accepts a call `poll_outbound_request` returned, bridging its audio
    /// to `alsa_device` the same way `set_sound_device` already does for
    /// the inbound-mobile-call direction, and stores it as the (sole)
    /// active call so the existing SIP-peer-disconnected/hangup plumbing
    /// (`pjsua_safe::is_sip_peer_disconnected`, `hangup_active_call`) covers
    /// it with no new teardown path.
    pub fn accept_outbound(&mut self, mut call: Call, alsa_device: &str) -> Result<(), String> {
        self.set_sound_device(alsa_device)?;
        call.answer(200).map_err(|e| format!("{e}"))?;
        tracing::info!(call_id = call.call_id(), "outbound call accepted");
        self.active_call = Some(call);
        Ok(())
    }

    /// Refuses a call `poll_outbound_request` returned — no idle line, or an
    /// invalid destination (FR-009/FR-014). `code` names the SIP status;
    /// `contracts/sip-dialout.md` uses `503` for "no line was idle".
    pub fn refuse_outbound(&self, mut call: Call, code: u32) {
        if let Err(e) = call.answer(code) {
            tracing::warn!(error = %e, code, "failed to send outbound refusal");
        }
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
        // Stop serving phones before dropping the endpoint: the registrar owns
        // its own socket and thread, and joining it here keeps shutdown
        // deterministic rather than leaving it to drop order.
        if let Some(ref mut registrar) = self.registrar {
            registrar.stop();
        }
        self.registrar = None;
        self.bindings = None;
        if let Some(ref mut account) = self.account {
            account.unregister();
        }
        self.account = None;
        self.endpoint = None;
        self.state = RegistrationState::Unregistered;
        crate::metrics::SIP_REGISTERED.set(0.0);
        crate::metrics::SIP_SERVER_BINDINGS.set(0.0);
        crate::metrics::SIP_SERVER_RING_AOR_REGISTERED.set(0.0);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> crate::config::AppConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[sip]\nserver = \"sip.example.com\"\nusername = \"user\"\npassword = \"pass\"\n",
        )
        .unwrap();
        crate::config::load_config(&path).unwrap()
    }

    /// specs/025-outbound-calling review: `make_call` (inbound GSM→SIP
    /// bridging) used to overwrite `active_call` unconditionally — a call
    /// already occupying it (from another modem, or an outbound call this
    /// feature adds) would be silently destroyed, orphaning its real
    /// carrier/modem leg with nothing left tracking it. `make_call` must
    /// refuse instead of clobbering.
    #[test]
    fn make_call_refuses_when_a_call_is_already_active() {
        let config = test_config();
        let mut bridge = SipBridge::new(&config);
        bridge.active_call = Some(Call::from_id(1, CallState::Confirmed));

        let result = bridge.make_call("sip:1234@example.com", "+15551234567");

        assert_eq!(
            result,
            Err("a call is already active on this SIP bridge".to_string())
        );
    }
}
