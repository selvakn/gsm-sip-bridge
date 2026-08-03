//! Agent A: the IMS/VoWiFi-facing half of the inbound VoWiFi bridge (see
//! `specs/011-vowifi-sip-bridge/`). Runs inside the ePDG tunnel's `ims`
//! network namespace, keeps a persistent IMS-AKA registration alive
//! (`super::register_session`, kept alive rather than torn down), answers
//! inbound `INVITE`s from the carrier, and relays RTP between the carrier
//! side and a veth link to `crate::vowifi` (Agent B).
//!
//! Agent A is a SIP UAS on *two* fronts for a single call: the carrier's
//! Gm-protected IMS transport (`session.transport`, established by IMS-AKA
//! registration) and a second, unauthenticated plain-SIP link on the veth
//! (`VETH_SIP_PORT`) that Agent B's PJSIP `Call::make` dials into once it
//! decides to bridge — see `crate::vowifi::bridge_call`. Both fronts reuse
//! the same `SipRequest`/`build_*` primitives from `super::sip_client`;
//! only the carrier-facing one needs IMS-AKA/Gm-IPsec, since the veth link
//! is a private, trusted point-to-point connection between the two agents.

use crate::config::VowifiConfig;
use crate::control::protocol::{AgentKind, BridgeFailureReason, CallStatus, RegistrationStatus};
use crate::error::{BridgeError, BridgeResult};
use crate::ims::lifecycle::{
    Admission, BridgedCall, CallStage, EndedBy, Maintenance, MaintenanceDecision, MaintenancePolicy,
};
use crate::ims::observability;
// Extracted to `ims::session` so the host-side cellular service uses the same
// implementation rather than a copy (FR-019, SC-008). Imported by name so the
// call sites below read exactly as they did before the move.
use crate::ims::sdp::{self, NegotiatedCodec};
use crate::ims::session::{
    attempt_renewal, extract_caller, handle_notify, map_registration_error,
    map_registration_status_code, next_backoff, respond, restart_client_reader, start_inbound,
    subscribe_reg_event, to_unix, Inbound,
};
use crate::ims::sip_client::{
    build_100_trying, build_180_ringing, build_200_ok_bye, build_200_ok_invite,
    build_200_ok_message, build_486_busy_here, build_bye, build_uas_response, format_sip_addr,
    random_hex, ByeRequest, SipMessage, SipRequest, SipSink,
};
use crate::ims::transport::{EpdgTransport, ImsTransport};
use crate::ims::ImsRegisterConfig;
use crate::observability::reporter::Reporter;
use crate::store::StoreHandle;
use crate::vowifi::control::{read_msg, reason, write_msg, ControlMessage};
use crate::vowifi::VETH_SIP_PORT;
use chrono::Utc;
use std::io::BufReader;
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// How long Agent A waits for Agent B to *place* its two legs (`BridgeReady`)
/// before giving up and declining the carrier's INVITE. Only covers getting the
/// PBX ringing — the wait for a human to actually pick up is `RING_TIMEOUT`,
/// and the caller hears ringback throughout it.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(4);
/// How long Agent A waits for Agent B's veth-side `INVITE` to arrive after
/// signaling `IncomingCall` — Agent B places its veth call as part of
/// reaching `BridgeReady`, so this should resolve well within
/// `CONTROL_TIMEOUT` in the success case; this is the ceiling for the
/// separate thread that's listening for it.
const VETH_INVITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the PBX extension may ring — with the caller hearing real ringback
/// throughout — before we give up and return `480`. Must stay under the
/// carrier's own no-answer timer so *we* decide the outcome, not the network.
/// `crate::vowifi`'s `PBX_RING_TIMEOUT` is deliberately a little shorter, so
/// Agent B normally reports `BridgeFailed` before this fires.
const RING_TIMEOUT: Duration = Duration::from_secs(50);
/// How often, while ringing, to check the control channel and the carrier's
/// signaling. Bounds how fast a caller's `CANCEL` gets answered.
const RING_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How often the dispatch loop comes up for air while a call is up, so a
/// hangup that starts on the PBX side is turned into a `BYE` toward the carrier
/// promptly rather than leaving the caller on a dead line.
const ACTIVE_CALL_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How often the RTP relay's blocking `recv` wakes up to check whether it
/// should stop — bounds how quickly a hangup actually silences the relay.
const RELAY_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// How often the main dispatch loop wakes up when idle. Originally sized
/// only for registration-renewal checks (30s, matching the existing
/// project's `[resilience].network_poll_interval_sec` default from feature
/// 009) — but `place_call_rx` (specs/025-outbound-calling) is also only
/// re-checked once per loop iteration, and the loop's *only* blocking wait
/// is `inbound.rx.recv_timeout(poll)`, so this interval is really "how long
/// a `PlaceCall` can sit unnoticed when nothing else wakes the loop first."
/// Found live (T072): at 30s, a `PlaceCall` could sit long enough for
/// `vowifi::mod`'s `PLACE_CALL_TIMEOUT` (a much shorter 3s) to give up on
/// this line before Agent A even sent `CallAttempting`. 1s keeps renewal
/// checks plenty timely (`RENEWAL_HEADROOM` is 300s) while bounding that
/// worst case to something `PLACE_CALL_TIMEOUT` safely covers. `pub(crate)`
/// so `vowifi::mod`'s tests can assert `PLACE_CALL_TIMEOUT` stays larger
/// than this, the same cross-check `CALL_ATTEMPT_TIMEOUT` does against
/// `OUTBOUND_INVITE_TIMEOUT`.
pub(crate) const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How far ahead of the registration's actual expiry Agent A starts trying
/// to renew it — SC-003's 90s recovery budget plus margin for the
/// renewal's own AKA-challenge round trip.
const RENEWAL_HEADROOM: Duration = Duration::from_secs(300);
const RETRY_INITIAL_BACKOFF: Duration = Duration::from_secs(5);
const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(120);

/// Work that must succeed before a renewal is worth attempting.
///
/// Exists for the LTE path, where the carrier tears the network attachment
/// down roughly every two hours (specs/015 research R15) and renewing over a
/// dead attachment only produces a connect timeout. The Wi-Fi path passes
/// `None` — its tunnel is maintained by charon, not by us.
///
/// **This is also what defers re-attachment during a call**, and deliberately
/// so: the hook runs inside the block the dispatch loop already skips while
/// `active_call.is_some()`, so re-attachment inherits renewal's deferral
/// rather than carrying a second policy that could drift from it. An
/// unguarded re-attach would drop a live call every two hours
/// (specs/017 T039).
pub(crate) type PreRenewalHook = dyn Fn() -> Result<(), String> + Send + Sync;

/// Answers "is the network attachment still up?" during a call, so a call whose
/// attachment genuinely dies mid-call can be ended with the cause stated,
/// distinct from the caller hanging up (FR-011).
///
/// Returns `true` while attached. LTE-only — the cellular path reads `CEREG`;
/// the Wi-Fi path passes `None`, because its ePDG tunnel is charon's to watch
/// and a lost tunnel already surfaces as the control connection dropping.
///
/// It is consulted only *during* a call and only after the media has stalled,
/// so it costs no modem traffic on a healthy call, and confirming genuine loss
/// before ending a call is what keeps a transient silence from being mistaken
/// for a dropped attachment.
pub(crate) type AttachmentHook = dyn Fn() -> bool + Send + Sync;

/// How long the carrier leg may carry no audio before the attachment is
/// checked. A real conversation with DTX still sends comfort-noise frames, so a
/// full stall this long is already abnormal; the check then decides whether it
/// is silence or a genuinely dead attachment.
const MEDIA_STALL_BEFORE_ATTACHMENT_CHECK: Duration = Duration::from_secs(6);

/// Consecutive attachment checks that must report "down" before a call is ended
/// for attachment loss. More than one so a single glitched `CEREG` read cannot
/// tear down a live call.
const ATTACHMENT_LOSS_CONFIRMATIONS: u32 = 2;

/// Minimum gap between attachment probes once the media has stalled — so a
/// stalled call is confirmed dead over a few seconds, not hammered at the
/// dispatch loop's fast poll rate.
const ATTACHMENT_PROBE_INTERVAL: Duration = Duration::from_secs(2);

/// Entry point for the `vowifi-ims-agent` subcommand. `card_id` labels this
/// line's metrics/history (specs/013-multi-card-vowifi FR-017) — always the
/// real card id of a resolved `discover` line (`--line N` is required; see
/// `main.rs::handle_vowifi_ims_agent_command`). `vowifi_config` is this
/// line's fully-derived settings, read from the `discover` resolution file;
/// `app_config` is still needed in full alongside it because
/// restoring observability (specs/014-vowifi-metrics-restore) needs
/// `[control].socket_path` (where to send reports),
/// `[metrics].agent_report_interval_seconds` (how often), `[sms].db_path`
/// (the shared call/SMS history database), and `[bridge].sip_destination`
/// (recorded on every VoWiFi call row, the same destination Agent B dials)
/// — none of which live in `VowifiConfig` itself.
pub fn run(
    card_id: &str,
    vowifi_config: &VowifiConfig,
    app_config: &crate::config::AppConfig,
) -> ExitCode {
    match run_inner(card_id, vowifi_config, app_config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner(
    card_id: &str,
    config: &VowifiConfig,
    app_config: &crate::config::AppConfig,
) -> BridgeResult<()> {
    // The ePDG tunnel is one of two `ImsTransport`s feeding the same
    // registration machinery (specs/015-volte-host-ims); the LTE IMS PDN is
    // the other. For VoWiFi this is exactly the P-CSCF file read that used to
    // sit inline here — same source, same port, same error text.
    let mut transport = EpdgTransport::new(config.pcscf_source_path.clone(), 5060);
    let transport_handle = transport.prepare()?;
    tracing::info!(
        transport = transport.name(),
        pcscf = %transport_handle.pcscf,
        descriptor = %transport_handle.descriptor,
        "IMS transport ready"
    );
    let pcscf_addr = transport_handle.pcscf.ip();
    // Empty mcc/mnc means auto-derive (config::VowifiConfig::mcc docs). The
    // IMS realm is built from these, so derive them from the SIM the same
    // way `supervise::orchestrate`'s `vowifi-plmn` call does for the tunnel
    // side — over whichever transport this line's SIM is actually on. For a
    // modem line, opening the modem here is nothing new (registration
    // already uses it for AT+CIMI/AT+CSIM below); for a `pcsc_reader` line
    // there is no modem port to open, so EF_IMSI/EF_AD come off the reader
    // holding this line's card, which is why mcc/mnc need not be configured.
    let (mcc, mnc) = if config.mcc.is_empty() {
        let plmn = if config.pcsc_reader {
            let imsi = config.imsi_override.clone().ok_or_else(|| {
                BridgeError::Ims(
                    "pcsc_reader line has no imsi_override configured, so its \
                     reader cannot be identified to derive mcc/mnc"
                        .into(),
                )
            })?;
            let mut card = crate::modules::pcsc_card::PcscTransport::connect(&imsi)?;
            // Held exclusively: charon's eap-sim-pcsc may be mid-EAP on this
            // same card, and an interleaved SELECT would corrupt the read.
            card.with_transaction(crate::vowifi::plmn::derive_plmn_from_card)?
        } else {
            let mut at = crate::modules::at_commander::AtCommander::open(std::path::Path::new(
                &config.modem_port,
            ))?;
            crate::vowifi::plmn::derive_plmn(&mut at)?
        };
        tracing::info!(mcc = %plmn.mcc, mnc = %plmn.mnc, "derived home PLMN from the SIM");
        (plmn.mcc, plmn.mnc)
    } else {
        (config.mcc.clone(), config.mnc.clone())
    };
    let reg_cfg = ImsRegisterConfig {
        modem_port: PathBuf::from(&config.modem_port),
        pcsc_reader: config.pcsc_reader,
        pcscf_addr,
        pcscf_port: transport_handle.pcscf.port(),
        mcc,
        mnc,
        // `config.imsi_override`/`imei_override`: when set, skip the modem
        // reads entirely (`register_session` in ims/mod.rs falls back to
        // AT+CIMI/AT+CGSN only when these are `None`). Both are static per
        // SIM/modem, so pinning them removes this function's only AT-command
        // dependency from the per-registration hot path — the path that,
        // left unpinned, hit a wedged AT channel on every
        // `vowifi-ims-agent` restart (e.g. after a P-CSCF change) and
        // crash-looped for hours until the modem was power-cycled.
        imsi: config.imsi_override.clone(),
        imei: config.imei_override.clone(),
        use_tcp: config.use_tcp,
        sec_agree: config.sec_agree,
        msisdn: None,
        access_network_info: crate::ims::ACCESS_NETWORK_WLAN.to_string(),
    };

    let veth_local_ip: IpAddr = config
        .veth_local_addr
        .parse()
        .map_err(|e| BridgeError::Ims(format!("invalid vowifi.veth_local_addr: {e}")))?;
    let control_addr: SocketAddr = format!("{}:{}", config.veth_peer_addr, config.control_port)
        .parse()
        .map_err(|e| BridgeError::Ims(format!("invalid vowifi control address: {e}")))?;

    serve_inbound(InboundParams {
        card_id,
        reg_cfg: &reg_cfg,
        local_ip: veth_local_ip,
        control_addr,
        // Each Wi-Fi Agent A is alone in its own netns, so they all share the
        // one well-known status port.
        status_port: crate::vowifi::AGENT_A_STATUS_PORT,
        wideband: config.wideband,
        // The Wi-Fi path keeps its long-standing answer ordering (FR-020) and
        // has no attachment of its own to refresh.
        answer_preference: sdp::AnswerPreference::legacy(),
        veth_sip_port: VETH_SIP_PORT,
        pre_renewal: None,
        // The ePDG tunnel is charon's to watch, and a lost tunnel already
        // surfaces as the control connection dropping — no mid-call probe here.
        attachment_check: None,
        // No LTE modem on this path, so nothing competes for an AT port.
        modem_lock: None,
        // Wi-Fi Agent A cannot see Agent B's PBX registration (separate
        // processes), so it does not gate on it.
        pbx_registered: None,
        app_config,
        agent_label: "vowifi-ims-agent",
        agent_kind: AgentKind::Ims,
    })
}

/// Everything the carrier-facing half needs that is not the transport itself.
///
/// A struct rather than a long argument list because the two callers differ in
/// only four of these, and a positional list of nine would make it easy to
/// swap two addresses silently.
pub(crate) struct InboundParams<'a> {
    pub card_id: &'a str,
    pub reg_cfg: &'a ImsRegisterConfig,
    /// Address the status listener and the telephone-side leg's UAS bind to —
    /// the veth-local address for Wi-Fi, loopback for cellular.
    pub local_ip: IpAddr,
    /// Where the telephone-side half is listening for call signalling.
    pub control_addr: SocketAddr,
    /// Port on `local_ip` the registration-status listener binds. On Wi-Fi
    /// each Agent A sits in its own netns so they all share
    /// `vowifi::AGENT_A_STATUS_PORT`; the cellular path runs several carrier
    /// halves in one namespace over loopback, so each gets its own
    /// per-line-derived port (specs/018-volte-multi-modem).
    pub status_port: u16,
    pub wideband: bool,
    pub answer_preference: sdp::AnswerPreference,
    /// Port the telephone-side half dials for its leg. The two halves must
    /// agree; see `handle_invite`.
    pub veth_sip_port: u16,
    pub pre_renewal: Option<&'a PreRenewalHook>,
    /// Checks the network attachment during a call so a mid-call loss ends it
    /// with the cause stated (FR-011). `None` on the Wi-Fi path.
    pub attachment_check: Option<&'a AttachmentHook>,
    /// Serialises this half's modem AT access (registration, renewal) with any
    /// other user of the same port — the cellular path's modem SMS reader.
    /// `None` on the Wi-Fi path, which has no such competitor and no LTE modem.
    pub modem_lock: Option<Arc<Mutex<()>>>,
    /// Whether the telephone-side half holds the PBX registration the outbound
    /// bridge leg needs — shared from that half (cellular only; the two halves
    /// are one process there). `None` on the Wi-Fi path, where health does not
    /// track the PBX leg and so treats it as available.
    pub pbx_registered: Option<Arc<AtomicBool>>,
    pub app_config: &'a crate::config::AppConfig,
    /// What to call this agent in logs.
    pub agent_label: &'a str,
    /// Which agent this is, for the `transport` label its reports land under.
    /// Both paths run this same code, so reporting it is the only thing that
    /// keeps their metrics distinguishable.
    pub agent_kind: AgentKind,
}

/// Holds a registration open and answers inbound calls on it until stopped.
///
/// Shared verbatim by the Wi-Fi and host-side cellular paths — FR-019/SC-008
/// require one implementation, and a copy would drift while looking like it
/// had not. Everything transport-specific is already resolved by the caller
/// and arrives in [`InboundParams`].
pub(crate) fn serve_inbound(p: InboundParams) -> BridgeResult<()> {
    let InboundParams {
        card_id,
        reg_cfg,
        local_ip,
        control_addr,
        status_port,
        wideband,
        answer_preference,
        veth_sip_port,
        pre_renewal,
        attachment_check,
        modem_lock,
        pbx_registered,
        app_config,
        agent_label,
        agent_kind,
    } = p;

    // Best-effort: a store that fails to open must not stop the agent from
    // registering and carrying calls (FR-018) — call history is simply
    // unavailable for this run, logged once here rather than on every insert
    // attempt.
    let history_store = match StoreHandle::open(std::path::Path::new(&app_config.sms.db_path)) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(error = %e, "failed to open call/SMS store; call history will not be recorded this run");
            None
        }
    };
    let reporter = Reporter::spawn(
        app_config.control.socket_path.clone(),
        agent_kind,
        card_id.to_string(),
        Duration::from_secs(app_config.metrics.agent_report_interval_seconds),
    );
    // Both paths run this code; the store's call rows must carry the right
    // transport or VoLTE and VoWiFi history collapse into one.
    let transport = match agent_kind {
        AgentKind::Volte | AgentKind::VolteSip => crate::store::Transport::Volte,
        AgentKind::Ims | AgentKind::Sip => crate::store::Transport::Vowifi,
    };
    let obs = observability::AgentObservability::new(
        reporter,
        card_id.to_string(),
        history_store,
        app_config.bridge.sip_destination.clone(),
        transport,
    );

    // Under the modem lock: `register_session` reads the IMEI over the AT port
    // the cellular path's SMS reader also uses (no-op on Wi-Fi, where the lock
    // is `None`).
    let mut session = {
        let _guard = modem_lock
            .as_ref()
            .map(|l| l.lock().unwrap_or_else(|e| e.into_inner()));
        match super::register_session(reg_cfg) {
            Ok(s) => s,
            Err(e) => {
                drop(_guard);
                obs.report_registration_attempt(map_registration_error(&e));
                obs.set_registered(false);
                obs.set_tunnel_up(false);
                return Err(e);
            }
        }
    };
    if session.status != 200 {
        let status = session.status;
        let reason = session.reason.clone();
        obs.report_registration_attempt(map_registration_status_code(status));
        obs.set_registered(false);
        obs.set_tunnel_up(false);
        session.cleanup();
        return Err(BridgeError::Ims(format!(
            "IMS registration failed: {status} {reason}"
        )));
    }
    tracing::info!(
        agent = agent_label,
        "registered, listening for inbound calls"
    );
    obs.report_registration_attempt(RegistrationStatus::Success);
    obs.set_registered(true);
    obs.set_tunnel_up(true);
    obs.set_active_calls(0);
    // Before the SUBSCRIBE, so the listeners are up to catch its response and
    // the NOTIFY the network sends straight back on a new connection.
    let mut inbound = start_inbound(&session)?;
    subscribe_reg_event(&mut session);

    let status = Arc::new(Mutex::new(super::RegistrationStatus {
        state: super::RegistrationState::Registered,
        registered_at: Some(SystemTime::now()),
        expires_at: Some(SystemTime::now() + Duration::from_secs(super::DEFAULT_EXPIRES as u64)),
        last_failure: None,
        // Health starts able-to-answer: we reach here only after a successful
        // registration, and the attachment underneath it is up (the Wi-Fi path
        // has none and leaves this at its default).
        ..Default::default()
    }));

    let (place_call_tx, place_call_rx) = mpsc::channel();
    {
        let status_for_listener = status.clone();
        std::thread::spawn(move || {
            if let Err(e) =
                run_status_listener(local_ip, status_port, status_for_listener, place_call_tx)
            {
                tracing::warn!(error = %e, "registration-status listener failed");
            }
        });
    }

    let result = dispatch_loop(
        &mut session,
        &mut inbound,
        reg_cfg,
        &status,
        control_addr,
        local_ip,
        wideband,
        answer_preference,
        veth_sip_port,
        pre_renewal,
        attachment_check,
        modem_lock.as_ref(),
        pbx_registered.as_ref(),
        &obs,
        place_call_rx,
    );
    session.unregister();
    session.cleanup();
    result
}

/// A `PlaceCall` (specs/025-outbound-calling) handed off by
/// `run_status_listener` to `dispatch_loop`: the still-open connection back
/// to Agent B (reused for `CallPlaced`/`CallFailed` and, on success, the
/// rest of the call), plus what it asked for.
pub(crate) struct PendingPlaceCall {
    control: TcpStream,
    call_id: String,
    destination: String,
}

/// Answers `vowifi-status`/`volte-status` queries (`ControlMessage::StatusQuery`
/// → `RegistrationStatusReply`) on `status_port` for as long as the agent
/// runs. A separate, always-listening connection from the main dispatch
/// loop's own SIP transport, so a status query never competes with call
/// signaling.
///
/// Also the listener Agent B connects to for `PlaceCall`
/// (specs/025-outbound-calling) — a genuinely different shape (the
/// connection must stay open for the whole call, not close after one
/// reply), so a `PlaceCall` connection is handed off whole to
/// `place_call_tx` rather than answered inline here. `dispatch_loop` does
/// the actual work single-threadedly, since it is the sole owner of
/// `session` — this thread's only job is accepting and routing.
fn run_status_listener(
    veth_local_ip: IpAddr,
    status_port: u16,
    status: Arc<Mutex<super::RegistrationStatus>>,
    place_call_tx: mpsc::Sender<PendingPlaceCall>,
) -> BridgeResult<()> {
    let listener = std::net::TcpListener::bind((veth_local_ip, status_port))
        .map_err(|e| BridgeError::Ims(format!("status listener bind failed: {e}")))?;
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "status listener accept failed");
                continue;
            }
        };
        let mut reader = match stream.try_clone() {
            Ok(s) => BufReader::new(s),
            Err(_) => continue,
        };
        match read_msg(&mut reader) {
            Ok(ControlMessage::PlaceCall {
                call_id,
                destination,
            }) => {
                if place_call_tx
                    .send(PendingPlaceCall {
                        control: stream,
                        call_id,
                        destination,
                    })
                    .is_err()
                {
                    tracing::warn!("dispatch loop gone, dropping outbound call request");
                }
            }
            Ok(ControlMessage::StatusQuery) => {
                let snapshot = status.lock().unwrap_or_else(|e| e.into_inner()).clone();
                // One derivation of "can this line answer a call right now?",
                // straight from the model — so the status a `volte-status`
                // caller reads agrees by construction with the admission the
                // dispatch loop actually applies (`ims::lifecycle`).
                let health = snapshot.health();
                let reply = ControlMessage::RegistrationStatusReply {
                    state: format!("{:?}", snapshot.state),
                    registered_at: snapshot.registered_at.and_then(to_unix),
                    expires_at: snapshot.expires_at.and_then(to_unix),
                    last_failure: snapshot
                        .last_failure
                        .map(|(t, msg)| (to_unix(t).unwrap_or(0), msg)),
                    can_answer: health.can_answer(),
                    blocked_reason: health.blocked_reason().map(str::to_string),
                };
                let _ = write_msg(&mut stream, &reply);
            }
            Ok(other) => {
                tracing::debug!(message = ?other, "unexpected message on status port, ignoring");
            }
            Err(e) => {
                tracing::debug!(error = %e, "failed to read status query");
            }
        }
    }
    Ok(())
}

/// Acknowledges an inbound SIP `MESSAGE` (RFC 3428) — the carrier's
/// VoWiFi/IMS transport for SMS, the counterpart to `AT+CMTI`/`AT+CMGR` in
/// `modules::mod`'s circuit-switched flow — and relays it to Agent B over
/// the control channel so it can be forwarded to Discord the same way.
/// Acks first, unconditionally: a relay/Discord hiccup on Agent B's end must
/// never make the carrier retransmit the same `MESSAGE`. Agent B, not Agent
/// A, owns the actual Discord post — it holds the `[sms]` webhook config and
/// has LAN/Internet reachability, whereas Agent A's netns is IMS-tunnel-only
/// (see `ControlMessage::SmsReceived` docs).
/// Handles an inbound SIP `MESSAGE` (RFC 3428).
///
/// # Hand it on before acknowledging it
///
/// The acknowledgement goes out **after** the message has been handed to the
/// half that records it, never before. This ordering is the whole safety
/// property (specs/017 FR-026):
///
/// - Acknowledge first, and a crash in the window between the two loses the
///   message outright — while the network believes it was delivered, so it
///   never retries. A lost text announces itself to nobody.
/// - Acknowledge after, and the same crash costs a retransmission, which
///   `volte::sms::Dedupe` absorbs.
///
/// One ordering loses data; the other costs a duplicate that is then
/// suppressed. So a relay failure deliberately leaves the message
/// *unacknowledged*: the network retrying is the recovery mechanism, and
/// acknowledging something we failed to record would throw that away.
fn handle_message(sink: &SipSink, req: &SipRequest, control_addr: SocketAddr) {
    let sender = extract_caller(req);
    let body = req.body.clone();
    tracing::info!(sender = %sender, "received SIP MESSAGE");

    let msg = ControlMessage::SmsReceived {
        sender: sender.clone(),
        body,
        received_at: chrono::Utc::now().to_rfc3339(),
    };
    let relayed = match TcpStream::connect_timeout(&control_addr, CONTROL_TIMEOUT) {
        Ok(mut control) => match write_msg(&mut control, &msg) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "failed to relay SIP MESSAGE for recording");
                false
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "failed to reach the control channel to relay SIP MESSAGE");
            false
        }
    };

    if relayed {
        let _ = sink.send(&build_200_ok_message(req, &random_hex(4)));
    } else {
        // Deliberately silent toward the network: an unacknowledged MESSAGE is
        // retransmitted, which is the recovery we want. Acknowledging one we
        // failed to record would discard the only chance to get it back.
        tracing::warn!(
            sender = %sender,
            "not acknowledging the MESSAGE so the network retransmits it"
        );
    }
}

/// Holds what's needed to tear a bridged call down again once a `BYE`
/// arrives — the control connection Agent B expects `CallEnded` on, and the
/// flag that stops the background RTP relay threads.
struct ActiveCall {
    control: TcpStream,
    /// Agent B's side of the control channel. Kept alive for the whole call so
    /// the dispatch loop hears about a hangup that starts on the *PBX* side —
    /// without it, only a carrier-originated `BYE` could ever end a call, and
    /// hanging up the SIP extension would leave the caller on a dead line.
    ctrl_rx: mpsc::Receiver<ControlMessage>,
    stop: Arc<AtomicBool>,
    call_id: String,
    to_tag: String,
    /// What's needed to hang up on the carrier ourselves, captured from the
    /// INVITE while we still had it.
    dialog: DialogInfo,
    /// Observability bookkeeping (specs/014-vowifi-metrics-restore): who
    /// called and when the call was answered, needed at hangup time to
    /// report `CallCompleted`/write the history row.
    caller: String,
    answered_at: chrono::DateTime<Utc>,
    answered_instant: Instant,
    /// Per-direction packet counts on the carrier leg, read at teardown for the
    /// FR-017 one-way-audio verdict.
    meter: super::media_stats::MediaMeter,
    /// The transport-agnostic lifecycle record for this call (`ims::lifecycle`).
    /// A live `ActiveCall` only exists once the call actually bridged, so this
    /// is created already advanced to [`CallStage::Bridged`]; the dispatch loop
    /// attributes its ending through it so end-cause and success are decided by
    /// one model, not restated at each teardown site.
    lifecycle: BridgedCall,
}

/// The dialog state needed to send an in-dialog request (a `BYE`) on a call we
/// answered as a UAS. See `sip_client::ByeRequest` for the role reversal.
struct DialogInfo {
    /// The caller's `Contact` URI — where in-dialog requests must be sent.
    remote_target: String,
    /// `Record-Route` from the INVITE, reversed.
    route_headers: Vec<String>,
    /// Our `From` on outgoing in-dialog requests: the INVITE's `To` plus our tag.
    from: String,
    /// Our `To`: the INVITE's `From`, tag included.
    to: String,
    local_addr: SocketAddr,
    use_tcp: bool,
    /// Our own CSeq counter for this dialog. We answered the INVITE, so the
    /// caller's CSeq space is theirs; ours starts fresh.
    cseq: u32,
}

impl DialogInfo {
    fn from_invite(invite: &SipRequest, to_tag: &str, session: &super::RegisteredSession) -> Self {
        // Fall back to the Request-URI if the caller sent no Contact — a BYE to
        // the wrong target is still better than never hanging up at all.
        let remote_target = invite
            .header("Contact")
            .and_then(|c| {
                let start = c.find('<')? + 1;
                let end = c[start..].find('>')? + start;
                Some(c[start..end].to_string())
            })
            .unwrap_or_else(|| invite.request_uri.clone());

        let route_headers: Vec<String> = invite
            .headers_all("Record-Route")
            .iter()
            .rev()
            .map(|v| format!("Route: {v}"))
            .collect();

        let from = match invite.header("To") {
            Some(to) if to.contains(";tag=") => to.to_string(),
            Some(to) => format!("{to};tag={to_tag}"),
            None => format!("<sip:{}>;tag={to_tag}", session.public_uri),
        };
        let to = invite.header("From").unwrap_or_default().to_string();

        Self {
            remote_target,
            route_headers,
            from,
            to,
            local_addr: session.local_addr,
            use_tcp: session.use_tcp,
            cseq: 1,
        }
    }

    /// The UAC-role counterpart to [`from_invite`](Self::from_invite) —
    /// specs/025-outbound-calling, research.md R-010: we *sent* the INVITE
    /// this dialog started from, so unlike `from_invite`, `from`/`to` come
    /// from what we sent/received rather than the reverse, and `route_headers`
    /// reuses the same Service-Route set the INVITE itself was routed with
    /// (the same simplification `ims::call::run_call` already makes for its
    /// own BYE, rather than recomputing a dialog route set from
    /// `Record-Route` — `SipResponse` does not even expose repeated headers
    /// the way `SipRequest::headers_all` does, since nothing needed it before
    /// this).
    fn from_uac_response(
        resp: &crate::ims::sip_client::SipResponse,
        route_headers: Vec<String>,
        callee_uri: &str,
        public_uri: &str,
        from_tag: &str,
        next_cseq: u32,
        session: &super::RegisteredSession,
    ) -> Self {
        // The far end's Contact is where in-dialog requests belong (RFC 3261
        // §12.1.2); no Contact on the 200 OK is malformed but not fatal — the
        // original callee URI is still a request the network already proved
        // it could route once.
        let remote_target = resp
            .header("Contact")
            .and_then(|c| {
                let start = c.find('<')? + 1;
                let end = c[start..].find('>')? + start;
                Some(c[start..end].to_string())
            })
            .unwrap_or_else(|| callee_uri.to_string());

        let to = resp
            .header("To")
            .map(str::to_string)
            .unwrap_or_else(|| format!("<sip:{callee_uri}>"));
        let from = format!("<sip:{public_uri}>;tag={from_tag}");

        Self {
            remote_target,
            route_headers,
            from,
            to,
            local_addr: session.local_addr,
            use_tcp: session.use_tcp,
            cseq: next_cseq,
        }
    }
}

/// How long to wait for *any* response at all (even a bare `100 Trying`)
/// to an originated INVITE — the first phase of
/// `SipTransport::recv_final_response_for_origination`. If nothing arrives
/// in this window, something transport-level is actually wrong, not just
/// "the phone hasn't picked up yet". Well under RFC 3261 Timer B (32s).
/// `pub(crate)`: see `OUTBOUND_RING_TIMEOUT`'s doc for why both of these
/// need to stay visible to `vowifi::mod`.
pub(crate) const OUTBOUND_INVITE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long to keep waiting for a final response once *any* response
/// (including a provisional) has confirmed the call is genuinely in
/// flight — the second phase of
/// `SipTransport::recv_final_response_for_origination`. Generous enough to
/// cover a real human noticing and answering a ringing phone, and any
/// carrier-side routing/signaling gaps between provisional responses.
/// `pub(crate)` so `vowifi::mod`'s `CALL_ATTEMPT_TIMEOUT` — how long Agent
/// B waits for `CallPlaced`/`CallFailed` once a line has committed to an
/// attempt — can be checked directly against
/// `OUTBOUND_INVITE_TIMEOUT + OUTBOUND_RING_TIMEOUT` (a unit test there
/// asserts it stays comfortably larger; found live,
/// specs/025-outbound-calling T072 pass 3, that a single flat 15s timeout
/// for the whole transaction abandoned a call that was still being set up
/// — including an 18s gap between `100 Trying` and the next provisional
/// response, apparently carrier-side routing rather than the callee's own
/// ring time — and the real, eventual `200 OK` arrived after the
/// transaction had already been given up on).
pub(crate) const OUTBOUND_RING_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait for a final response to our own CANCEL of an
/// abandoned INVITE — normally a prompt `487 Request Terminated`, plus the
/// (legitimate, RFC 3261 §9.1) chance of a `200 OK` racing in from before
/// the CANCEL arrived at the carrier. Short: a carrier that hasn't reacted
/// to a CANCEL within a few seconds isn't going to.
const CANCEL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Sends CANCEL for a pending outbound INVITE we're giving up on before a
/// final response ever arrived (RFC 3261 §9.1) — reusing the original
/// INVITE's own branch/CSeq number, since this targets that same
/// transaction rather than starting a new one. Found live
/// (specs/025-outbound-calling review): abandoning the transaction without
/// this left the carrier free to keep ringing the destination for as long
/// as *it* was willing to wait, regardless of how long we'd already given
/// up — exactly the "the carrier went on to answer a call nobody was
/// listening for" symptom T072 pass 3 first described (that pass lengthened
/// the timeout, which reduces how often this is hit but never closes the
/// gap itself).
///
/// Gives the carrier a short, bounded window afterward for the resulting
/// final response to the *original* INVITE: normally nothing further is
/// needed once `487` arrives, but a `200 OK` can still legitimately race in
/// (the carrier answered right as we gave up) — that case is ACKed and
/// immediately BYE'd, or the carrier leg would stay up with nothing on our
/// end tracking it.
///
/// KNOWN LIMITATION, only reachable *from our own side*: only reaches this
/// function when *we* give up on a timeout. A caller hanging up mid-ring
/// (while `dispatch_loop` is still blocked inside `originate_and_bridge`,
/// see that call site's own doc comment) has no way to trigger this at
/// all — `dispatch_loop` cannot observe the phone/PBX control connection
/// during that whole window, so it never learns the caller gave up until
/// its own timeout eventually elapses regardless. Fixing that needs an
/// interruptible wait, not just this function.
#[allow(clippy::too_many_arguments)]
fn cancel_pending_invite(
    session: &mut super::RegisteredSession,
    callee_uri: &str,
    route_headers: &[String],
    via_transport: &str,
    call_id: &str,
    from_tag: &str,
    invite_cseq: u32,
    invite_branch: &str,
) {
    let cancel = super::call::build_cancel(&super::call::CancelParts {
        request_uri: callee_uri,
        route_headers,
        via_transport,
        local_addr: session.local_addr,
        public_uri: &session.public_uri,
        callee_uri,
        call_id,
        from_tag,
        cseq: invite_cseq,
        branch: invite_branch,
    });
    let Ok(transport) = session.transport_mut() else {
        return;
    };
    if transport.send(&cancel).is_err() {
        return;
    }
    tracing::info!(call_id, "outbound: sent CANCEL for an abandoned INVITE");

    let Ok(resp) = transport.recv_final_response_for_origination(
        CANCEL_RESPONSE_TIMEOUT,
        CANCEL_RESPONSE_TIMEOUT,
        |_| {},
    ) else {
        return;
    };
    if resp.status != 200 {
        return;
    }
    tracing::warn!(
        call_id,
        "outbound: carrier answered despite CANCEL; sending ACK then BYE to hang up"
    );
    let to_header = resp.header("To").unwrap_or(callee_uri).to_string();
    let ack = super::call::build_ack(&super::call::AckParts {
        request_uri: callee_uri,
        route_headers,
        via_transport,
        local_addr: session.local_addr,
        public_uri: &session.public_uri,
        to_header: &to_header,
        call_id,
        from_tag,
        cseq: invite_cseq,
        branch: invite_branch,
    });
    let _ = session.transport_mut().and_then(|t| t.send(&ack));
    let bye = super::call::build_bye(&super::call::AckParts {
        request_uri: callee_uri,
        route_headers,
        via_transport,
        local_addr: session.local_addr,
        public_uri: &session.public_uri,
        to_header: &to_header,
        call_id,
        from_tag,
        cseq: invite_cseq + 1,
        branch: &format!("z9hG4bK{}", random_hex(6)),
    });
    let _ = session.transport_mut().and_then(|t| t.send(&bye));
}

/// Originates a call to `destination` over the already-registered carrier
/// session, waits for Agent B's veth call, and bridges the two legs — the
/// outbound mirror of `handle_invite`'s inbound answer path
/// (specs/025-outbound-calling, research.md R-009/R-010/R-011). The caller
/// (`dispatch_loop`) has already sent `CallAttempting` on `control` before
/// calling this; from here on it sends `CallPlaced`/`CallFailed` itself
/// before returning, so the caller only needs to fold the result into
/// `active_call`.
///
/// Hangs up a carrier leg that was answered (200 OK + ACK already
/// exchanged) but can't be usably bridged for some reason discovered
/// afterward — sends a BYE to the carrier (reusing `dialog`, built right
/// after ACK) and `CallEnded` to Agent B over `control`. Found live
/// (specs/025-outbound-calling review): by the time any of these failures
/// are discovered, Agent B has very likely *already* answered its own
/// phone/PBX leg (`Call::make` dispatches the veth INVITE and returns
/// immediately; `bridge_outbound_leg` answers the phone leg right after,
/// without waiting for it to connect) — leaving Agent B silent here would
/// strand a caller who thinks they're connected, on top of leaking the
/// carrier leg itself. Best-effort on both: there is nothing further to
/// retry from this point.
fn hangup_answered_carrier_leg(
    session: &mut super::RegisteredSession,
    control: &mut TcpStream,
    dialog: &DialogInfo,
    call_id: &str,
    reason: &str,
) {
    let bye = build_bye(&ByeRequest {
        request_uri: &dialog.remote_target,
        route_headers: &dialog.route_headers,
        via_transport: if dialog.use_tcp { "TCP" } else { "UDP" },
        local_addr: dialog.local_addr,
        from: &dialog.from,
        to: &dialog.to,
        call_id,
        cseq: dialog.cseq,
        branch: &format!("z9hG4bK{}", random_hex(6)),
    });
    let _ = session.transport_mut().and_then(|t| t.send(&bye));
    let _ = write_msg(
        control,
        &ControlMessage::CallEnded {
            call_id: call_id.to_string(),
            reason: reason.to_string(),
        },
    );
}

/// Never re-registers (`super::register_session`) — a second registration
/// for the same IMSI would tear down the live one (research.md R-010's hard
/// constraint). Everything here reuses `session`, exactly like every other
/// request this agent sends.
#[allow(clippy::too_many_arguments)]
fn originate_and_bridge(
    session: &mut super::RegisteredSession,
    mut control: TcpStream,
    call_id: String,
    destination: &str,
    veth_local_ip: IpAddr,
    veth_sip_port: u16,
    wideband: bool,
) -> Option<ActiveCall> {
    let fail = |control: &mut TcpStream, call_id: String, reason: &str| {
        tracing::warn!(call_id, reason, "outbound: could not place carrier call");
        let _ = write_msg(
            control,
            &ControlMessage::CallFailed {
                call_id,
                reason: reason.to_string(),
            },
        );
    };

    // RFC 3608, same simplification `ims::call::run_call` already makes:
    // subsequent requests in this dialog route via the Service-Route the
    // registrar returned, computed once here and reused for the INVITE and
    // (via `DialogInfo::from_uac_response`) the eventual BYE.
    let route_headers: Vec<String> = session
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("Service-Route"))
        .map(|(_, v)| format!("Route: {v}"))
        .collect();

    let rtp_socket = match UdpSocket::bind((session.local_addr.ip(), 0)) {
        Ok(s) => s,
        Err(e) => {
            fail(
                &mut control,
                call_id,
                &format!("RTP socket bind failed: {e}"),
            );
            return None;
        }
    };
    let rtp_port = match rtp_socket.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            fail(
                &mut control,
                call_id,
                &format!("RTP local_addr failed: {e}"),
            );
            return None;
        }
    };

    let session_id: u64 = rand::random::<u32>() as u64;
    // Offering, not answering — "prefer wideband when available" for both
    // carrier paths, rather than reusing `AnswerPreference`'s legacy/cellular
    // split, which is about the *answer* fallback order (AMR-NB vs. PCMU)
    // when the far end's own offer lacks AMR-WB — not applicable to what we
    // ourselves offer here.
    let offer = sdp::build_offer(
        session.local_addr.ip(),
        rtp_port,
        session_id,
        sdp::CodecOffer::preferring_wideband(wideband && amr_safe::is_available()),
    );

    // `;user=phone` (RFC 3261 §19.1.1 / TS 24.229): tells the network this is
    // a PSTN/mobile number, not a resolvable SIP address — the same header
    // `ims::call::run_call` adds after finding a bare `sip:` URI reached a
    // terminating application server that never rang the callee.
    let callee_uri = format!("{destination}@{};user=phone", session.realm);
    let from_tag = random_hex(4);
    let invite_cseq = session.cseq;
    let via_transport = if session.use_tcp { "TCP" } else { "UDP" };
    let branch = format!("z9hG4bK{}", random_hex(6));

    let invite = super::call::build_invite(&super::call::InviteParts {
        request_uri: &callee_uri,
        route_headers: &route_headers,
        via_transport,
        local_addr: session.local_addr,
        contact_addr: session.contact_addr,
        public_uri: &session.public_uri,
        callee_uri: &callee_uri,
        call_id: &call_id,
        from_tag: &from_tag,
        cseq: invite_cseq,
        branch: &branch,
        body: &offer,
    });

    tracing::info!(call_id, destination, "outbound: sending INVITE to carrier");
    let transport = match session.transport_mut() {
        Ok(t) => t,
        Err(e) => {
            fail(&mut control, call_id, &format!("no carrier transport: {e}"));
            return None;
        }
    };
    if let Err(e) = transport.send(&invite) {
        fail(&mut control, call_id, &format!("INVITE send failed: {e}"));
        return None;
    }
    // Sent at most once regardless of how many `180`s (retransmissions
    // included) the carrier sends — Agent B only needs to know "start
    // playing ringback," not track every repeat.
    let mut ringing_relayed = false;
    let resp = match transport.recv_final_response_for_origination(
        OUTBOUND_INVITE_TIMEOUT,
        OUTBOUND_RING_TIMEOUT,
        |provisional| {
            if provisional.status == 180 && !ringing_relayed {
                ringing_relayed = true;
                let _ = write_msg(
                    &mut control,
                    &ControlMessage::CallRinging {
                        call_id: call_id.clone(),
                    },
                );
            }
        },
    ) {
        Ok(r) => r,
        Err(e) => {
            cancel_pending_invite(
                session,
                &callee_uri,
                &route_headers,
                via_transport,
                &call_id,
                &from_tag,
                invite_cseq,
                &branch,
            );
            // Marked with `reason::CARRIER_TIMEOUT` so Agent B can tell
            // "genuinely rang out, nobody ever answered or declined" apart
            // from other post-commitment failures (SC-005,
            // specs/025-outbound-calling review: `OutboundAttemptOutcome::
            // Unanswered` existed on the wire but nothing ever reported
            // it) — the detailed error stays after the marker for logging.
            fail(
                &mut control,
                call_id,
                &format!(
                    "{}: no final response from carrier: {e}",
                    reason::CARRIER_TIMEOUT
                ),
            );
            return None;
        }
    };
    tracing::info!(call_id, status = resp.status, reason = %resp.reason, "outbound: final INVITE response");

    if resp.status != 200 {
        // Non-2xx final response: ACK reuses the INVITE's own branch/CSeq
        // (RFC 3261 §17.1.1.3), best-effort.
        let ack = super::call::build_ack(&super::call::AckParts {
            request_uri: &callee_uri,
            route_headers: &route_headers,
            via_transport,
            local_addr: session.local_addr,
            public_uri: &session.public_uri,
            to_header: resp.header("To").unwrap_or(&callee_uri),
            call_id: &call_id,
            from_tag: &from_tag,
            cseq: invite_cseq,
            branch: &branch,
        });
        let _ = session.transport_mut().and_then(|t| t.send(&ack));
        fail(
            &mut control,
            call_id,
            &format!("{} {}", resp.status, resp.reason),
        );
        return None;
    }

    let answer = match sdp::parse_answer(&resp.body) {
        Ok(a) => a,
        Err(e) => {
            fail(&mut control, call_id, &format!("bad SDP answer: {e}"));
            return None;
        }
    };
    if let Err(e) = rtp_socket.connect(answer.remote_rtp) {
        fail(&mut control, call_id, &format!("RTP connect failed: {e}"));
        return None;
    }

    let ack_branch = format!("z9hG4bK{}", random_hex(6));
    let ack = super::call::build_ack(&super::call::AckParts {
        request_uri: &callee_uri,
        route_headers: &route_headers,
        via_transport,
        local_addr: session.local_addr,
        public_uri: &session.public_uri,
        to_header: resp.header("To").unwrap_or(&callee_uri),
        call_id: &call_id,
        from_tag: &from_tag,
        cseq: invite_cseq,
        branch: &ack_branch,
    });
    if let Err(e) = session.transport_mut().and_then(|t| t.send(&ack)) {
        fail(&mut control, call_id, &format!("ACK send failed: {e}"));
        return None;
    }
    session.cseq = invite_cseq + 1;

    let dialog = DialogInfo::from_uac_response(
        &resp,
        route_headers,
        &callee_uri,
        &session.public_uri,
        &from_tag,
        session.cseq,
        session,
    );

    // Spawn the veth listener *before* telling Agent B to call in — same
    // ordering `handle_invite` uses for the inbound direction, so the
    // listener is guaranteed up by the time Agent B's `Call::make` reaches it.
    let veth_rx = match spawn_veth_uas_listener(veth_local_ip, veth_sip_port, wideband) {
        Ok(rx) => rx,
        Err(e) => {
            fail(&mut control, call_id, &format!("veth listener failed: {e}"));
            return None;
        }
    };

    if let Err(e) = write_msg(
        &mut control,
        &ControlMessage::CallPlaced {
            call_id: call_id.clone(),
        },
    ) {
        tracing::warn!(call_id, error = %e, "outbound: failed to notify Agent B the carrier leg is up");
        return None;
    }

    let veth = match veth_rx
        .recv_timeout(VETH_INVITE_TIMEOUT)
        .map_err(|_| "timed out waiting for Agent B's veth call".to_string())
        .and_then(|r| r.map_err(|e| e.to_string()))
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(call_id, error = %e, "outbound: Agent B's veth call never arrived");
            // The carrier leg is already up — hang it up rather than leaving
            // it connected with no media path. Agent B's own phone/PBX leg
            // is very likely already answered too by this point (`Call::make`
            // dispatches the veth INVITE and returns immediately;
            // `bridge_outbound_leg` answers the phone leg right after,
            // without waiting for it to connect) — tell it to hang up rather
            // than leave a caller thinking they're connected to dead air.
            hangup_answered_carrier_leg(
                session,
                &mut control,
                &dialog,
                &call_id,
                reason::VETH_LEG_FAILED,
            );
            return None;
        }
    };

    // `parse_answer` only returns which codec the answer picked
    // (`NegotiatedCodec`), not a payload type — by RFC 3264, a re-used
    // dynamic payload type on the answer must mean what *our own offer*
    // said it meant, so there is nothing to re-parse. Reconstructs the rest
    // from what `sdp::build_offer` is known to always send, the same
    // necessary duplication `ims::call`'s `AMR_WB_RTP_PAYLOAD_TYPE` already
    // accepts.
    let chosen = match answer.codec {
        NegotiatedCodec::Pcmu => sdp::ChosenCodec {
            codec: NegotiatedCodec::Pcmu,
            payload_type: 0,
            octet_aligned: false,
        },
        NegotiatedCodec::AmrWb => sdp::ChosenCodec {
            codec: NegotiatedCodec::AmrWb,
            payload_type: 96,
            octet_aligned: true,
        },
        other => {
            // Never offered — `sdp::build_offer` only ever lists PCMU/AMR-WB
            // for an outbound offer (CodecOffer has no AMR-NB/L16 variant).
            // Agent B's phone/PBX leg is already answered by this point
            // (see the veth-timeout branch above) — leaving it stranded on
            // dead air on top of leaking the carrier leg would compound the
            // failure, not just fail to bridge it.
            tracing::error!(
                call_id,
                codec = other.name(),
                "outbound: carrier answered with a codec we never offered"
            );
            hangup_answered_carrier_leg(
                session,
                &mut control,
                &dialog,
                &call_id,
                reason::TRANSPORT_ERROR,
            );
            return None;
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    let meter = super::media_stats::MediaMeter::new();
    let transcoding = chosen.codec != veth.codec.codec;
    let relay_result = if transcoding {
        super::transcode::spawn_transcoding_relay(
            rtp_socket,
            veth.rtp_socket,
            chosen,
            veth.codec,
            stop.clone(),
            &meter,
        )
    } else {
        spawn_relay(rtp_socket, veth.rtp_socket, stop.clone(), &meter);
        Ok(())
    };
    if let Err(e) = relay_result {
        // `spawn_transcoding_relay` only reaches its two `std::thread::spawn`
        // calls after every decoder/encoder/resampler construction (each
        // fallible) has already succeeded — an `Err` here means neither
        // relay thread was actually started, so there is nothing to signal
        // `stop` for, unlike the `try_clone` failure below.
        tracing::error!(call_id, error = %e, "outbound: failed to start media relay");
        hangup_answered_carrier_leg(
            session,
            &mut control,
            &dialog,
            &call_id,
            reason::TRANSPORT_ERROR,
        );
        return None;
    }

    tracing::info!(
        call_id,
        destination,
        carrier_codec = chosen.codec.name(),
        transcoding,
        "outbound: call placed and bridged"
    );

    let ctrl_rx = match control.try_clone() {
        Ok(s) => spawn_control_reader(s),
        Err(e) => {
            // Unlike the two failures above, the relay is already running
            // at this point — ask it to stop, or it leaks as an orphaned
            // background thread with nothing left to ever signal it.
            stop.store(true, Ordering::Release);
            tracing::warn!(call_id, error = %e, "outbound: control connection clone failed");
            hangup_answered_carrier_leg(
                session,
                &mut control,
                &dialog,
                &call_id,
                reason::TRANSPORT_ERROR,
            );
            return None;
        }
    };

    let mut lifecycle = BridgedCall::new(call_id.clone(), destination.to_string(), None);
    lifecycle.advance_to(CallStage::Answering);
    lifecycle.advance_to(CallStage::Bridged);

    Some(ActiveCall {
        control,
        ctrl_rx,
        stop,
        // Our own from_tag doubles as `to_tag`: `handle_bye`'s
        // `build_200_ok_bye` only falls back to it when the incoming
        // request's own To header lacks a tag, which a real in-dialog BYE
        // never does — see `DialogInfo::from_uac_response`'s doc comment.
        to_tag: from_tag,
        dialog,
        call_id,
        caller: destination.to_string(),
        answered_at: Utc::now(),
        answered_instant: Instant::now(),
        meter,
        lifecycle,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_loop(
    session: &mut super::RegisteredSession,
    inbound: &mut Inbound,
    reg_cfg: &ImsRegisterConfig,
    status: &Arc<Mutex<super::RegistrationStatus>>,
    control_addr: SocketAddr,
    veth_local_ip: IpAddr,
    wideband: bool,
    answer_preference: sdp::AnswerPreference,
    veth_sip_port: u16,
    pre_renewal: Option<&PreRenewalHook>,
    attachment_check: Option<&AttachmentHook>,
    modem_lock: Option<&Arc<Mutex<()>>>,
    pbx_registered: Option<&Arc<AtomicBool>>,
    obs: &observability::AgentObservability,
    place_call_rx: mpsc::Receiver<PendingPlaceCall>,
) -> BridgeResult<()> {
    let mut active_call: Option<ActiveCall> = None;
    let mut backoff = RETRY_INITIAL_BACKOFF;
    // Set after a failed renewal, cleared on success. Gates *retries* only —
    // unlike a blocking `thread::sleep(backoff)` (the previous approach),
    // this loop keeps calling `inbound.rx.recv_timeout` every iteration
    // regardless, so an inbound INVITE/BYE arriving during the backoff
    // window is still dispatched immediately instead of queuing unanswered
    // until the sleep ends (a carrier's transaction timer can expire and
    // drop an otherwise-valid call within that window — found in review,
    // not live-testing).
    let mut next_renewal_attempt: Option<Instant> = None;
    // Formalises the "maintenance must yield to a call" rule (`ims::lifecycle`):
    // it decides whether a due renewal may run or must be held for the call in
    // progress, and remembers that it was held so status can report the
    // deferral as deliberate rather than as a stall (the re-attachment the
    // renewal hook performs inherits the same deferral — see `PreRenewalHook`).
    let mut maintenance = MaintenancePolicy::new();
    // FR-011 mid-call attachment watch, reset per call (see the INVITE branch).
    let mut watch = AttachmentWatch::default();
    loop {
        // Keep the shared health inputs the status listener reads current — the
        // busy flag and any deferred maintenance — so a `volte-status` query is
        // answered from the same state the loop is acting on. Cheap: one lock
        // per poll, and the values are eventually consistent within a poll
        // interval regardless.
        {
            let mut guard = status.lock().unwrap_or_else(|e| e.into_inner());
            guard.busy = active_call.is_some();
            guard.deferred_maintenance = maintenance.deferred();
            // Reflect the telephone-side half's PBX registration. Absent (Wi-Fi
            // path), the PBX leg is not tracked here, so treat it as available
            // rather than falsely reporting the line unable to answer.
            guard.pbx_registered =
                pbx_registered.is_none_or(|f| f.load(std::sync::atomic::Ordering::SeqCst));
        }

        // A hangup can start on *either* side. The carrier's arrives as a BYE
        // below; the PBX's arrives here, as a `CallEnded` from Agent B — and
        // must be turned into a BYE toward the carrier, or hanging up the SIP
        // extension would leave the caller listening to a call that is already
        // over.
        if let Some(call) = &mut active_call {
            match call.ctrl_rx.try_recv() {
                Ok(ControlMessage::CallEnded { reason, .. }) => {
                    let mut call = active_call.take().expect("just matched Some");
                    // The telephone side hung up first (or reported its leg
                    // failed). Attribute it before reporting; Agent B's own
                    // reason string still drives the BYE for the finer detail.
                    call.lifecycle.end(EndedBy::Pbx);
                    report_answered_call_ended(obs, &call);
                    hangup_carrier(session, inbound, call, &reason);
                    // The call is over; any maintenance held for it may now run.
                    maintenance.release();
                    continue;
                }
                Ok(other) => {
                    tracing::debug!(message = ?other, "ignoring control message during an active call");
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Agent B is gone; we can't keep a half-bridged call up.
                    let mut call = active_call.take().expect("just matched Some");
                    tracing::warn!(call_id = %call.call_id, "Agent B's control connection dropped mid-call");
                    call.lifecycle.end(EndedBy::Pbx);
                    report_answered_call_ended(obs, &call);
                    hangup_carrier(session, inbound, call, reason::TRANSPORT_ERROR);
                    maintenance.release();
                    continue;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        // FR-011: end a call whose attachment genuinely died mid-call, stated
        // as such rather than as a caller hangup. Cheap on a healthy call —
        // the modem is only touched once the carrier leg has gone fully silent,
        // and even then only to tell a dead attachment from a quiet caller.
        if let Some(call) = &active_call {
            if let Some(check) = attachment_check {
                if watch.attachment_lost(call.meter.carrier_rx(), check) {
                    let mut call = active_call.take().expect("just matched Some");
                    tracing::warn!(
                        call_id = %call.call_id,
                        "ending call: the network attachment was lost mid-call \
                         (not a caller hangup) — FR-011"
                    );
                    call.lifecycle.end(EndedBy::AttachmentLost);
                    report_answered_call_ended(obs, &call);
                    end_call_attachment_lost(session, call);
                    maintenance.release();
                    watch = AttachmentWatch::default();
                    continue;
                }
            }
        }

        // Outbound calling (specs/025-outbound-calling) — the same
        // one-call-at-a-time rule `Admission::RejectBusy` already applies to
        // a *carrier*-originated INVITE, for the other direction. A request
        // arriving while `active_call.is_some()` gets an immediate `busy`
        // `CallFailed` — never left queued in the channel for whenever the
        // current call happens to end, which could be a long, silent wait
        // from Agent B's side. `contains("busy")` is what
        // `run_outbound_listener` (`vowifi/mod.rs`) checks to decide whether
        // to try a different line rather than giving up outright.
        if let Ok(mut pending) = place_call_rx.try_recv() {
            if active_call.is_some() {
                let _ = write_msg(
                    &mut pending.control,
                    &ControlMessage::CallFailed {
                        call_id: pending.call_id,
                        reason: "busy".to_string(),
                    },
                );
            } else {
                // Ack receipt *before* touching the carrier transport, so
                // Agent B can tell "committed, now genuinely placing the
                // call" apart from "busy" and switch to a much longer wait
                // for the real outcome (`vowifi::mod`'s `CallAttempting`
                // handling). Best-effort like `fail()`'s writes below: a
                // dead connection here will fail again, harmlessly, at the
                // next write.
                let _ = write_msg(
                    &mut pending.control,
                    &ControlMessage::CallAttempting {
                        call_id: pending.call_id.clone(),
                    },
                );
                // KNOWN LIMITATION (specs/025-outbound-calling review):
                // `originate_and_bridge` blocks this whole loop for up to
                // `OUTBOUND_INVITE_TIMEOUT + OUTBOUND_RING_TIMEOUT +
                // VETH_INVITE_TIMEOUT` (~80s) — nothing else this loop is
                // responsible for runs during that window: no inbound
                // carrier INVITE is answered, no hangup on an *existing*
                // call is processed (there can't be one — `active_call`
                // is `None` here, or we'd have taken the `busy` branch
                // above — but a *new* inbound call arriving during this
                // window is effectively dropped from the caller's
                // perspective even though its bytes are only queued, not
                // lost: `inbound.rx` is fed by independent reader threads
                // and does keep it, but by the time this loop gets back
                // around to it the caller's own SIP Timer B (32s) will
                // very likely have already given up), and this call's own
                // mid-ring CANCEL can't be triggered by anything on the
                // phone/PBX side either (see `cancel_pending_invite`'s doc
                // comment for that half of the same gap). Registration
                // renewal is unaffected (300s headroom). Properly bounding
                // this needs an interruptible wait on both this loop and
                // `vowifi::mod`'s `run_outbound_listener` — a materially
                // bigger change than this pass makes; documenting the gap
                // rather than leaving it silent.
                active_call = originate_and_bridge(
                    session,
                    pending.control,
                    pending.call_id,
                    &pending.destination,
                    veth_local_ip,
                    veth_sip_port,
                    wideband,
                );
            }
            continue;
        }

        // Poll fast enough to notice a PBX-side hangup promptly while a call is
        // up; idle otherwise, where the only deadline is registration renewal.
        let poll = if active_call.is_some() {
            ACTIVE_CALL_POLL_INTERVAL
        } else {
            IDLE_POLL_INTERVAL
        };
        match inbound.rx.recv_timeout(poll) {
            Ok((SipMessage::Request(req), sink)) if req.method == "INVITE" => {
                if Admission::for_current(active_call.as_ref().map(|c| &c.lifecycle))
                    == Admission::RejectBusy
                {
                    tracing::info!("declining inbound call: another VoWiFi call is already active");
                    let _ = sink.send(&build_486_busy_here(&req, &random_hex(4)));
                    obs.report_call_not_answered(
                        CallStatus::Failed,
                        BridgeFailureReason::BridgeSetupFailed,
                        &extract_caller(&req),
                        Utc::now(),
                    );
                    continue;
                }
                // If the telephone-side half has no PBX registration, the
                // outbound leg cannot be placed — decline immediately with
                // `480` rather than dialling into the void and making the caller
                // wait out a ~32s transaction timeout. `480`, not `486`: the
                // line is not busy, the bridge is temporarily unavailable.
                if pbx_registered.is_some_and(|f| !f.load(std::sync::atomic::Ordering::SeqCst)) {
                    tracing::warn!(
                        caller = %extract_caller(&req),
                        "declining inbound call: the PBX registration is down, so the \
                         outbound bridge leg cannot be placed"
                    );
                    let _ = sink.send(&build_uas_response(
                        480,
                        "Temporarily Unavailable",
                        &req,
                        Some(&random_hex(4)),
                        None,
                        None,
                    ));
                    obs.report_call_not_answered(
                        CallStatus::Failed,
                        BridgeFailureReason::AgentUnreachable,
                        &extract_caller(&req),
                        Utc::now(),
                    );
                    continue;
                }
                match handle_invite(
                    session,
                    &req,
                    &sink,
                    inbound,
                    control_addr,
                    veth_local_ip,
                    wideband,
                    answer_preference,
                    veth_sip_port,
                    obs,
                ) {
                    Ok(call) => {
                        if call.is_some() {
                            obs.set_active_calls(1);
                            // Fresh call, fresh media baseline (the meter starts
                            // at zero) — so a previous call's counts cannot read
                            // as a stall on this one.
                            watch = AttachmentWatch::default();
                        }
                        active_call = call;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to handle inbound INVITE");
                        // Tell the caller. Without this the carrier never gets
                        // a final response, so the caller keeps hearing the
                        // ringback our earlier `180` started and waits out the
                        // network's own timer — a call that rings forever and
                        // never connects, with no indication anything failed
                        // (FR-005, observed live: specs/017 R17).
                        //
                        // `480 Temporarily Unavailable` rather than `486 Busy`:
                        // the line is not busy, the bridge could not be built.
                        // Saying which is the difference between a caller
                        // redialling now and one redialling later.
                        if let Err(send_err) = sink.send(&build_uas_response(
                            480,
                            "Temporarily Unavailable",
                            &req,
                            Some(&random_hex(4)),
                            None,
                            None,
                        )) {
                            tracing::warn!(
                                error = %send_err,
                                "could not tell the caller the bridge failed"
                            );
                        }
                        obs.report_call_not_answered(
                            CallStatus::Failed,
                            BridgeFailureReason::AgentUnreachable,
                            &extract_caller(&req),
                            Utc::now(),
                        );
                    }
                }
            }
            Ok((SipMessage::Request(req), sink)) if req.method == "BYE" => {
                match active_call.take() {
                    Some(mut call) => {
                        // The carrier's BYE is the caller hanging up.
                        call.lifecycle.end(EndedBy::Caller);
                        report_answered_call_ended(obs, &call);
                        handle_bye(&sink, &req, call);
                        maintenance.release();
                    }
                    None => {
                        let _ = sink.send(&build_200_ok_bye(&req, &random_hex(4)));
                    }
                }
            }
            Ok((SipMessage::Request(req), _)) if req.method == "ACK" => {
                tracing::debug!("received ACK, dialog confirmed");
            }
            Ok((SipMessage::Request(req), sink)) if req.method == "NOTIFY" => {
                handle_notify(&sink, &req);
            }
            Ok((SipMessage::Request(req), sink)) if req.method == "MESSAGE" => {
                handle_message(&sink, &req, control_addr);
            }
            Ok((SipMessage::Request(req), _)) => {
                tracing::info!(method = %req.method, "ignoring unsupported inbound request");
            }
            Ok((SipMessage::Response(resp), _)) => {
                // Outside a call the only requests we originate are reg-event
                // SUBSCRIBEs, so their outcome is worth surfacing rather than
                // burying at debug.
                tracing::info!(
                    status = resp.status,
                    reason = %resp.reason,
                    "received response outside an active transaction"
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(BridgeError::Ims(
                    "every Gm connection reader has stopped; the registration is unreachable"
                        .into(),
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Idle wake-up: nothing arrived within the poll interval.
                // Never renew mid-call — that would tear down the transport
                // a call's own signaling (e.g. the eventual BYE) still
                // needs; renewal is deferred until the call ends.
                let Some(expires_at) = status.lock().unwrap_or_else(|e| e.into_inner()).expires_at
                else {
                    continue;
                };
                if !super::renewal_due(SystemTime::now(), expires_at, RENEWAL_HEADROOM) {
                    continue;
                }
                // Renewal is genuinely due. Hold it if a call is in progress —
                // recorded by the policy so the deferral is visible in status,
                // and so the model, not an inline `is_some()`, owns the rule.
                if maintenance.decide(Maintenance::Renewal, active_call.is_some())
                    == MaintenanceDecision::Defer
                {
                    continue;
                }
                // A previous attempt failed and its backoff hasn't elapsed
                // yet — `renewal_due` alone would otherwise fire again on
                // every idle wake-up regardless of backoff, hammering a
                // still-failing renewal every poll interval.
                if let Some(next_attempt) = next_renewal_attempt {
                    if Instant::now() < next_attempt {
                        continue;
                    }
                }
                status.lock().unwrap_or_else(|e| e.into_inner()).state =
                    super::RegistrationState::Renewing;
                // Hold the modem lock across the whole renewal: the hook
                // re-attaches (drives the modem) and `attempt_renewal` re-reads
                // the IMEI over the AT port. Serialises with the cellular SMS
                // reader that shares that port (research R6); `None`, so a
                // no-op, on the Wi-Fi path. Released when this arm ends or on
                // any `continue` below.
                let _modem_guard = modem_lock.map(|l| l.lock().unwrap_or_else(|e| e.into_inner()));
                // Rebuild the layer underneath before spending a REGISTER on
                // it. Reaching here already means no call is in progress (the
                // maintenance policy deferred it above otherwise), which is
                // precisely how re-attachment inherits renewal's deferral
                // instead of needing its own — see `PreRenewalHook`.
                if let Some(hook) = pre_renewal {
                    if let Err(reason) = hook() {
                        tracing::warn!(
                            error = %reason,
                            retry_in_secs = backoff.as_secs(),
                            "cannot renew: the network attachment is down"
                        );
                        let mut guard = status.lock().unwrap_or_else(|e| e.into_inner());
                        guard.state = super::RegistrationState::Failed;
                        guard.last_failure = Some((SystemTime::now(), reason));
                        // The re-attach hook is what just failed, so the
                        // attachment underneath is down — health must say so.
                        guard.attached = false;
                        drop(guard);
                        obs.set_registered(false);
                        next_renewal_attempt = Some(Instant::now() + backoff);
                        backoff = next_backoff(backoff, RETRY_MAX_BACKOFF);
                        continue;
                    }
                }
                match attempt_renewal(reg_cfg) {
                    Ok(new_session) => {
                        session.cleanup();
                        *session = new_session;
                        // A renewal negotiates a fresh Gm SA on fresh ports,
                        // so the old listeners are now bound to dead ones.
                        *inbound = start_inbound(session)?;
                        let mut guard = status.lock().unwrap_or_else(|e| e.into_inner());
                        guard.state = super::RegistrationState::Registered;
                        guard.registered_at = Some(SystemTime::now());
                        guard.expires_at = Some(
                            SystemTime::now() + Duration::from_secs(super::DEFAULT_EXPIRES as u64),
                        );
                        // A renewal only reaches here through a successful
                        // re-attach (the hook above), so the attachment is up.
                        guard.attached = true;
                        drop(guard);
                        backoff = RETRY_INITIAL_BACKOFF;
                        next_renewal_attempt = None;
                        tracing::info!("registration renewed");
                        obs.report_registration_attempt(RegistrationStatus::Success);
                        obs.set_registered(true);
                        obs.set_tunnel_up(true);
                        subscribe_reg_event(session);
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            retry_in_secs = backoff.as_secs(),
                            "registration renewal failed, retrying with backoff"
                        );
                        obs.report_registration_attempt(map_registration_error(&e));
                        obs.set_registered(false);
                        obs.set_tunnel_up(false);
                        let mut guard = status.lock().unwrap_or_else(|e| e.into_inner());
                        guard.state = super::RegistrationState::Failed;
                        guard.last_failure = Some((SystemTime::now(), e.to_string()));
                        drop(guard);
                        // Not a blocking sleep: the loop keeps dispatching
                        // inbound SIP every iteration in the meantime (see
                        // `next_renewal_attempt`'s doc comment above).
                        next_renewal_attempt = Some(Instant::now() + backoff);
                        backoff = next_backoff(backoff, RETRY_MAX_BACKOFF);
                    }
                }
            }
        }
    }
}

/// Reports an answered call ending — `CallCompleted{Answered}`, the history
/// row, and `active_calls` back to 0 — shared by every path that can end an
/// `ActiveCall` (carrier `BYE`, PBX-originated `CallEnded`, Agent B's
/// control connection dropping mid-call).
fn report_answered_call_ended(obs: &observability::AgentObservability, call: &ActiveCall) {
    let verdict = call
        .meter
        .verdict(super::media_stats::DEFAULT_ONE_WAY_THRESHOLD_PERCENT);
    tracing::info!(
        call_id = %call.call_id,
        media = verdict.as_str(),
        carrier_rx = call.meter.carrier_rx(),
        pbx_rx = call.meter.pbx_rx(),
        // The lifecycle model's own account of the call: who ended it and the
        // status it derives from the same media verdict. Logged so the model
        // that drives admission and teardown is auditable against the metric
        // reported just below (`ims::lifecycle`).
        ended_by = call.lifecycle.ended_by.map(|e| e.as_str()).unwrap_or("unknown"),
        outcome = call.lifecycle.call_status(verdict.is_success()).as_str(),
        "call media verdict"
    );
    if !verdict.is_success() {
        tracing::warn!(
            call_id = %call.call_id,
            media = verdict.as_str(),
            "answered call did not carry audio both ways: {}",
            verdict.diagnosis()
        );
    }
    obs.report_call_answered_and_ended(
        &call.caller,
        call.answered_at,
        call.answered_instant.elapsed().as_secs_f64(),
        verdict,
    );
    obs.set_active_calls(0);
}

/// Answers (or declines) one inbound carrier `INVITE`. Returns `Some` with
/// the bookkeeping `handle_bye` will need once the call is actually
/// bridged; `None` if it was declined (busy line, no compatible codec, or
/// Agent B couldn't bridge it) — every decline path sends a fast, explicit
/// `486 Busy Here` per the spec's Clarifications answer, never silence or
/// unanswered ringing (FR-009/FR-010).
#[allow(clippy::too_many_arguments)]
fn handle_invite(
    session: &super::RegisteredSession,
    req: &SipRequest,
    sink: &SipSink,
    inbound: &Inbound,
    control_addr: SocketAddr,
    veth_local_ip: IpAddr,
    wideband: bool,
    answer_preference: sdp::AnswerPreference,
    // `veth_sip_port` is the port on `veth_local_ip` where the telephone-side
    // half's leg is expected. It MUST match what that half dials — a mismatch
    // produces a call that rings the PBX, is answered, and then times out with
    // the caller still hearing ringback (observed live, specs/017 R17).
    veth_sip_port: u16,
    obs: &observability::AgentObservability,
) -> BridgeResult<Option<ActiveCall>> {
    let call_id = req.header("Call-ID").unwrap_or_default().to_string();
    let caller = extract_caller(req);
    let started_at = Utc::now();
    tracing::info!(
        call_id = %call_id,
        caller = %caller,
        request_uri = %req.request_uri,
        "inbound VoWiFi call"
    );

    sink.send(&build_100_trying(req))?;

    let offer = sdp::parse_offer(&req.body)?;
    // A carrier's mobile-terminating VoWiFi INVITE often offers no PCMU at
    // all (Airtel: AMR-WB+AMR-NB on some calls, AMR-NB alone on others), so
    // anything AMR gets answered and transcoded rather than declined. Uses
    // `sdp::select_codec` — the same decision `build_answer` makes below, with
    // the same arguments — so we can never accept a call we then can't build an
    // answer for.
    let Some(precheck) = sdp::select_codec_with(
        &offer,
        amr_safe::is_available(),
        wideband,
        answer_preference,
    ) else {
        tracing::info!(
            call_id = %call_id,
            amr_linked = amr_safe::is_available(),
            offered = ?offer.offered.iter().map(|c| (c.payload_type, c.codec)).collect::<Vec<_>>(),
            "offer has no codec we can answer with; declining"
        );
        sink.send(&build_486_busy_here(req, &random_hex(4)))?;
        obs.report_call_not_answered(
            CallStatus::Failed,
            BridgeFailureReason::BridgeSetupFailed,
            &caller,
            started_at,
        );
        return Ok(None);
    };

    // Only a wideband *carrier* leg has anything for a wideband veth leg to
    // preserve. A narrowband call (PCMU or AMR-NB — the two shapes Airtel
    // sends when the originating leg is narrowband) keeps the veth link on
    // PCMU, exactly the path it took before L16 existed.
    let veth_wideband = precheck.codec == NegotiatedCodec::AmrWb;
    let veth_rx = spawn_veth_uas_listener(veth_local_ip, veth_sip_port, veth_wideband)?;

    // Generated once and reused for every response in this dialog (180 and
    // 200 OK alike) — RFC 3261 requires the same To-tag across all
    // responses that establish/confirm one dialog.
    let to_tag = random_hex(4);
    let public_user = session
        .public_uri
        .split('@')
        .next()
        .unwrap_or(&session.public_uri)
        .to_string();
    let via_transport = if session.use_tcp { "TCP" } else { "UDP" };
    // The protected server port, not the client port we send from — this is
    // the address the carrier's in-dialog requests (the eventual BYE) come
    // back to. See `RegisteredSession::contact_addr`.
    let contact = format!(
        "<sip:{public_user}@{};transport={via_transport}>",
        format_sip_addr(session.contact_addr)
    );
    // Ring the caller. The network turns this into audible ringback and keeps
    // playing it until we answer — which we now deliberately don't do until a
    // human picks up the PBX extension (see `await_pbx_answer`).
    sink.send(&build_180_ringing(req, &to_tag, &contact))?;

    let mut control = TcpStream::connect_timeout(&control_addr, CONTROL_TIMEOUT)
        .map_err(|e| BridgeError::Ims(format!("failed to reach Agent B control channel: {e}")))?;
    write_msg(
        &mut control,
        &ControlMessage::IncomingCall {
            call_id: call_id.clone(),
            caller: caller.clone(),
        },
    )
    .map_err(BridgeError::Ims)?;
    let ctrl_rx = spawn_control_reader(
        control
            .try_clone()
            .map_err(|e| BridgeError::Ims(format!("control connection clone failed: {e}")))?,
    );
    let reply = ctrl_rx
        .recv_timeout(CONTROL_TIMEOUT)
        .map_err(|_| BridgeError::Ims("timed out waiting for Agent B to place its legs".into()))?;

    match reply {
        ControlMessage::BridgeReady { .. } => {
            let veth = veth_rx.recv_timeout(VETH_INVITE_TIMEOUT).map_err(|_| {
                BridgeError::Ims("timed out waiting for Agent B's veth call".into())
            })??;

            let ims_rtp_socket = UdpSocket::bind((session.local_addr.ip(), 0))
                .map_err(|e| BridgeError::Ims(format!("IMS RTP socket bind failed: {e}")))?;
            let ims_rtp_port = ims_rtp_socket
                .local_addr()
                .map_err(|e| BridgeError::Ims(format!("IMS RTP local_addr failed: {e}")))?
                .port();
            ims_rtp_socket
                .connect(offer.remote_rtp)
                .map_err(|e| BridgeError::Ims(format!("IMS RTP connect failed: {e}")))?;

            let session_id: u64 = rand::random::<u32>() as u64;
            // Re-runs the same selection as the `precheck` above and so lands
            // on the same codec. It hands back the payload type and framing it
            // committed us to, both of which the media path must honour
            // exactly.
            let (answer_sdp, chosen) = sdp::build_answer(
                session.local_addr.ip(),
                ims_rtp_port,
                session_id,
                &offer,
                amr_safe::is_available(),
                wideband,
                answer_preference,
            )?;

            // Do NOT answer yet. The PBX extension is only ringing; our
            // `180 Ringing` above is what makes the network play ringback to
            // the caller, and a `200 OK` here would cut that off and leave them
            // in silence until someone picks up. Wait for Agent B to report a
            // real answer — while still watching the carrier's own signaling,
            // since the caller may give up (`CANCEL`) while it rings.
            match await_pbx_answer(&call_id, &ctrl_rx, inbound, req, &to_tag, sink)? {
                RingOutcome::Answered => {}
                RingOutcome::PbxDeclined => {
                    obs.report_call_not_answered(
                        CallStatus::Missed,
                        BridgeFailureReason::PbxDeclined,
                        &caller,
                        started_at,
                    );
                    return Ok(None);
                }
                RingOutcome::Abandoned { reason } => {
                    // Agent B is still ringing the extension — stop it.
                    let _ = write_msg(
                        &mut control,
                        &ControlMessage::CallEnded {
                            call_id: call_id.clone(),
                            reason: reason.to_string(),
                        },
                    );
                    obs.report_call_not_answered(
                        CallStatus::Missed,
                        observability::map_bridge_failure_reason(reason),
                        &caller,
                        started_at,
                    );
                    return Ok(None);
                }
            }

            sink.send(&build_200_ok_invite(req, &to_tag, &contact, &answer_sdp))?;

            let stop = Arc::new(AtomicBool::new(false));
            // Counts audio each way so the completed call can be judged
            // both-ways or one-way (FR-017) — the same guard the outbound path
            // applies, here on the shared inbound bridge both transports use.
            let meter = super::media_stats::MediaMeter::new();
            let transcoding = chosen.codec != veth.codec.codec;
            if transcoding {
                // The two legs speak different codecs (or the same codec at
                // different rates), so it has to be terminated on each side
                // and re-encoded.
                super::transcode::spawn_transcoding_relay(
                    ims_rtp_socket,
                    veth.rtp_socket,
                    chosen,
                    veth.codec,
                    stop.clone(),
                    &meter,
                )?;
            } else {
                // Both legs speak PCMU: forward the payloads untouched.
                spawn_relay(ims_rtp_socket, veth.rtp_socket, stop.clone(), &meter);
            }
            // Both sides of Agent A's bridge, so a one-way-audio or
            // lost-your-wideband report can be read straight off the log: what
            // the carrier negotiated, and what goes over the veth to Agent B.
            tracing::info!(
                call_id = %call_id,
                carrier_codec = chosen.codec.name(),
                carrier_sample_rate = chosen.codec.sample_rate(),
                carrier_payload_type = chosen.payload_type,
                carrier_octet_aligned = chosen.octet_aligned,
                veth_codec = veth.codec.codec.name(),
                veth_sample_rate = veth.codec.codec.sample_rate(),
                transcoding,
                "call answered and bridged"
            );

            // Walk the lifecycle through the stages this call actually passed —
            // offered, telephone-leg placed, PBX ringing, then bridged — so the
            // record carries the real path and `reached_bridged` is set through
            // the legal transitions rather than stamped on. Reaching here means
            // all four happened, in this order.
            let mut lifecycle = BridgedCall::new(call_id.clone(), caller.clone(), None);
            lifecycle.advance_to(CallStage::Answering);
            lifecycle.advance_to(CallStage::PbxRinging);
            lifecycle.advance_to(CallStage::Bridged);

            Ok(Some(ActiveCall {
                control,
                ctrl_rx,
                stop,
                dialog: DialogInfo::from_invite(req, &to_tag, session),
                call_id,
                to_tag,
                caller,
                answered_at: Utc::now(),
                answered_instant: Instant::now(),
                meter,
                lifecycle,
            }))
        }
        ControlMessage::BridgeFailed {
            reason: fail_reason,
            ..
        } => {
            tracing::info!(call_id = %call_id, reason = %fail_reason, "Agent B could not bridge the call, declining");
            sink.send(&build_486_busy_here(req, &random_hex(4)))?;
            obs.report_call_not_answered(
                CallStatus::Failed,
                observability::map_bridge_failure_reason(&fail_reason),
                &caller,
                started_at,
            );
            Ok(None)
        }
        other => Err(BridgeError::Ims(format!(
            "unexpected control-channel reply to IncomingCall: {other:?}"
        ))),
    }
}

/// Why we stopped ringing. The carrier has already been sent its final
/// response in every case; the distinction is whether **Agent B** still needs
/// telling — if it does and we don't, the PBX extension keeps ringing at
/// someone long after the call is over.
enum RingOutcome {
    /// A human picked up the PBX extension; answer the carrier.
    Answered,
    /// Agent B gave up on the PBX itself (`BridgeFailed`) and has already torn
    /// its own legs down. Nothing more to tell it.
    PbxDeclined,
    /// We stopped ringing while Agent B still thinks the call is alive — the
    /// caller hung up, or we hit our own ring timeout. Agent B must be told to
    /// stop ringing the extension.
    Abandoned { reason: &'static str },
}

/// Hold the carrier in the ringing state until Agent B reports the PBX
/// extension was actually answered.
///
/// While waiting, the carrier's own signaling still has to be serviced: the
/// caller can give up at any point, which arrives as a `CANCEL` and must be
/// answered promptly (`200 OK` to the CANCEL, `487` to the INVITE it cancels —
/// RFC 3261 §9.2) or the network keeps retransmitting and the caller is left
/// listening to a phone that has already been hung up. So this polls both the
/// control channel and the inbound SIP queue rather than blocking on either.
fn await_pbx_answer(
    call_id: &str,
    ctrl_rx: &mpsc::Receiver<ControlMessage>,
    inbound: &Inbound,
    invite: &SipRequest,
    to_tag: &str,
    sink: &SipSink,
) -> BridgeResult<RingOutcome> {
    let decline = |status: u16, reason: &str| {
        respond(
            sink,
            reason,
            &build_uas_response(status, reason, invite, Some(to_tag), None, None),
        );
    };

    let deadline = std::time::Instant::now() + RING_TIMEOUT;
    while std::time::Instant::now() < deadline {
        // 1. Did the caller give up while it rang? A CANCEL must be answered
        //    promptly or the network keeps retransmitting it.
        while let Ok((msg, cancel_sink)) = inbound.rx.try_recv() {
            let SipMessage::Request(req) = msg else {
                continue;
            };
            if req.method == "CANCEL" && req.header("Call-ID") == Some(call_id) {
                tracing::info!(call_id = %call_id, "caller hung up while the PBX was still ringing");
                // RFC 3261 §9.2: 200 OK to the CANCEL, 487 to the INVITE it
                // cancels. The CANCEL is its own transaction, so it is answered
                // on the connection it arrived on.
                respond(
                    &cancel_sink,
                    "200 OK (CANCEL)",
                    &build_uas_response(200, "OK", &req, Some(to_tag), None, None),
                );
                decline(487, "Request Terminated");
                return Ok(RingOutcome::Abandoned {
                    reason: reason::CALLER_CANCELLED,
                });
            }
            tracing::debug!(method = %req.method, "ignoring inbound request received while ringing");
        }

        // 2. Did Agent B report an answer, or give up on the PBX?
        match ctrl_rx.recv_timeout(RING_POLL_INTERVAL) {
            Ok(ControlMessage::CallAnswered { .. }) => return Ok(RingOutcome::Answered),
            Ok(ControlMessage::BridgeFailed { reason, .. }) => {
                tracing::info!(call_id = %call_id, reason = %reason, "PBX leg did not answer; declining");
                decline(480, "Temporarily Unavailable");
                return Ok(RingOutcome::PbxDeclined);
            }
            Ok(other) => {
                tracing::warn!(call_id = %call_id, message = ?other, "unexpected control message while ringing");
            }
            // Still ringing.
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(BridgeError::Ims(
                    "Agent B's control connection closed while the PBX was ringing".into(),
                ));
            }
        }
    }

    tracing::info!(call_id = %call_id, "PBX extension rang out; declining");
    decline(480, "Temporarily Unavailable");
    Ok(RingOutcome::Abandoned {
        reason: reason::PBX_NO_ANSWER,
    })
}

/// Reads Agent B's control messages on a thread, so the caller can wait on
/// them with a timeout while also servicing the carrier's SIP signaling —
/// without a partially-read line ever corrupting the newline-JSON framing,
/// which is what polling the socket with a read timeout would risk.
fn spawn_control_reader(stream: TcpStream) -> mpsc::Receiver<ControlMessage> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        loop {
            match read_msg(&mut reader) {
                Ok(msg) => {
                    if tx.send(msg).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "Agent B control connection reader stopped");
                    return;
                }
            }
        }
    });
    rx
}

/// End a call that was hung up on the *PBX* side, by sending a `BYE` to the
/// carrier. The mirror image of `handle_bye` (which handles the carrier hanging
/// up on us); between them, a hangup from either end tears the whole bridge
/// down.
///
/// The BYE goes out on the registered client transport, like every other
/// request we originate — it is routed by the dialog's route set, not by which
/// connection the INVITE happened to arrive on.
/// Watches a call's carrier leg for a genuinely lost attachment (FR-011).
///
/// The signal is two-stage on purpose. Downlink packets stalling is cheap to
/// notice and happens first, but on its own it cannot tell a dropped attachment
/// from a caller who simply went quiet. So a stall only *arms* the check; the
/// authoritative answer — "is the modem still attached?" — is asked over the AT
/// port, and only after a stall has persisted, so a healthy call never touches
/// the modem at all. Loss is declared only after it is confirmed more than once,
/// so a single glitched read cannot tear down a live call.
#[derive(Default)]
struct AttachmentWatch {
    carrier_rx_mark: u64,
    media_stalled_since: Option<Instant>,
    last_probe: Option<Instant>,
    down_count: u32,
}

impl AttachmentWatch {
    /// Feeds the current downlink packet count and, once the carrier leg has
    /// been silent long enough, probes `check`. Returns `true` only when the
    /// attachment is confirmed lost.
    fn attachment_lost(&mut self, carrier_rx: u64, check: &AttachmentHook) -> bool {
        if carrier_rx > self.carrier_rx_mark {
            // Audio is still arriving from the carrier — healthy; reset.
            self.carrier_rx_mark = carrier_rx;
            self.media_stalled_since = None;
            self.last_probe = None;
            self.down_count = 0;
            return false;
        }
        // The carrier leg is silent. Wait out the stall window before spending
        // an AT round-trip on it.
        let stalled_since = *self.media_stalled_since.get_or_insert_with(Instant::now);
        if stalled_since.elapsed() < MEDIA_STALL_BEFORE_ATTACHMENT_CHECK {
            return false;
        }
        if let Some(last) = self.last_probe {
            if last.elapsed() < ATTACHMENT_PROBE_INTERVAL {
                return false;
            }
        }
        self.last_probe = Some(Instant::now());
        if check() {
            // Attached: the silence is the caller, not a lost attachment. Rearm
            // the stall window rather than re-probing on every tick.
            self.media_stalled_since = Some(Instant::now());
            self.down_count = 0;
            false
        } else {
            self.down_count += 1;
            self.down_count >= ATTACHMENT_LOSS_CONFIRMATIONS
        }
    }
}

/// Ends a call because the network attachment was lost mid-call (FR-011).
///
/// The same coordinated teardown as a carrier `BYE` — stop the relay, tell
/// Agent B over the control channel so it drops the PBX leg — plus a
/// best-effort `BYE` toward the carrier. That `BYE` will usually not arrive
/// (the attachment it would travel over is the thing that died), but sending it
/// costs nothing and closes the dialog on any path that survived.
fn end_call_attachment_lost(session: &mut super::RegisteredSession, mut call: ActiveCall) {
    call.stop.store(true, Ordering::Relaxed);
    if let Err(e) = write_msg(
        &mut call.control,
        &ControlMessage::CallEnded {
            call_id: call.call_id.clone(),
            reason: reason::ATTACHMENT_LOST.to_string(),
        },
    ) {
        tracing::warn!(call_id = %call.call_id, error = %e, "failed to notify Agent B of the attachment-loss teardown");
    }
    let d = &call.dialog;
    let bye = build_bye(&ByeRequest {
        request_uri: &d.remote_target,
        route_headers: &d.route_headers,
        via_transport: if d.use_tcp { "TCP" } else { "UDP" },
        local_addr: d.local_addr,
        from: &d.from,
        to: &d.to,
        call_id: &call.call_id,
        cseq: d.cseq,
        branch: &format!("z9hG4bK{}", random_hex(6)),
    });
    let _ = session.transport_mut().and_then(|t| t.send(&bye));
    tracing::info!(call_id = %call.call_id, reason = reason::ATTACHMENT_LOST, "call ended");
}

/// Tells the carrier the call is over after the PBX side hangs up first.
///
/// The client transport can die silently mid-call — a NAT or the P-CSCF
/// itself dropping an idle TCP connection, since no SIP traffic crosses this
/// leg for the whole call duration (media is a separate RTP path; see
/// `RegisteredSession::reconnect_transport`) — so the first `send` failing
/// does not mean the carrier leg is unreachable, only that this particular
/// socket is dead. One reconnect-and-retry recovers that case; if the retry
/// also fails, the carrier leg is left stuck up (rare: reconnect only fails
/// if the underlying network attachment itself is down, in which case the
/// carrier's own side will eventually time the call out).
fn hangup_carrier(
    session: &mut super::RegisteredSession,
    inbound: &Inbound,
    call: ActiveCall,
    reason: &str,
) {
    call.stop.store(true, Ordering::Relaxed);
    let d = &call.dialog;
    let bye = build_bye(&ByeRequest {
        request_uri: &d.remote_target,
        route_headers: &d.route_headers,
        via_transport: if d.use_tcp { "TCP" } else { "UDP" },
        local_addr: d.local_addr,
        from: &d.from,
        to: &d.to,
        call_id: &call.call_id,
        cseq: d.cseq,
        branch: &format!("z9hG4bK{}", random_hex(6)),
    });
    match session.transport_mut().and_then(|t| t.send(&bye)) {
        Ok(()) => {
            tracing::info!(call_id = %call.call_id, reason, "PBX hung up; sent BYE to the carrier");
            return;
        }
        Err(e) => {
            tracing::warn!(call_id = %call.call_id, error = %e, "failed to BYE the carrier after a PBX hangup; reconnecting to retry");
        }
    }
    if let Err(e) = session.reconnect_transport() {
        tracing::warn!(call_id = %call.call_id, error = %e, "could not reconnect the carrier transport; carrier leg may be left up until the network times it out");
        return;
    }
    match session.transport_mut().and_then(|t| t.send(&bye)) {
        Ok(()) => {
            tracing::info!(call_id = %call.call_id, reason, "PBX hung up; sent BYE to the carrier after reconnecting");
        }
        Err(e) => {
            tracing::warn!(call_id = %call.call_id, error = %e, "failed to BYE the carrier even after reconnecting; carrier leg may be left up until the network times it out");
        }
    }
    if let Err(e) = restart_client_reader(session, inbound) {
        tracing::warn!(call_id = %call.call_id, error = %e, "failed to restart the Gm client reader after a mid-call transport reconnect");
    }
}

fn handle_bye(sink: &SipSink, req: &SipRequest, mut call: ActiveCall) {
    call.stop.store(true, Ordering::Relaxed);
    if let Err(e) = write_msg(
        &mut call.control,
        &ControlMessage::CallEnded {
            call_id: call.call_id.clone(),
            reason: reason::CALLER_HANGUP.to_string(),
        },
    ) {
        tracing::warn!(call_id = %call.call_id, error = %e, "failed to notify Agent B of hangup");
    }
    respond(sink, "200 OK (BYE)", &build_200_ok_bye(req, &call.to_tag));
    tracing::info!(call_id = %call.call_id, "call ended");
}

/// Result of Agent A's veth-facing UAS answering Agent B's inbound call.
struct VethUasResult {
    rtp_socket: UdpSocket,
    /// The codec this UAS answered Agent B's offer with — `L16/16000` when the
    /// carrier leg is wideband and PJSIP offered it, PCMU otherwise. The media
    /// path must speak exactly this.
    codec: sdp::ChosenCodec,
}

/// Starts a background thread listening for Agent B's veth-side `INVITE`
/// (a single UDP datagram is expected — PJSIP's default offer is well under
/// any MTU), answers it, and delivers the resulting RTP socket (already
/// `connect()`-ed to Agent B's advertised RTP address) over the returned
/// channel. Started *before* signaling Agent B over the control channel so
/// the listener is guaranteed to be up by the time Agent B's `Call::make`
/// actually reaches it.
fn spawn_veth_uas_listener(
    veth_local_ip: IpAddr,
    veth_sip_port: u16,
    wideband: bool,
) -> BridgeResult<mpsc::Receiver<BridgeResult<VethUasResult>>> {
    let sip_socket = UdpSocket::bind((veth_local_ip, veth_sip_port))
        .map_err(|e| BridgeError::Ims(format!("veth SIP socket bind failed: {e}")))?;
    sip_socket
        .set_read_timeout(Some(VETH_INVITE_TIMEOUT))
        .map_err(|e| BridgeError::Ims(format!("veth SIP socket set_read_timeout failed: {e}")))?;

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(accept_veth_invite(
            &sip_socket,
            veth_local_ip,
            veth_sip_port,
            wideband,
        ));
    });
    Ok(rx)
}

#[allow(clippy::too_many_arguments)]
fn accept_veth_invite(
    sip_socket: &UdpSocket,
    veth_local_ip: IpAddr,
    veth_sip_port: u16,
    wideband: bool,
) -> BridgeResult<VethUasResult> {
    let mut buf = [0u8; 4096];
    let (n, peer) = sip_socket
        .recv_from(&mut buf)
        .map_err(|e| BridgeError::Ims(format!("veth INVITE recv failed: {e}")))?;
    let text = String::from_utf8_lossy(&buf[..n]);
    let (req, _consumed) = SipRequest::try_parse(&text)?
        .ok_or_else(|| BridgeError::Ims("incomplete veth INVITE datagram".into()))?;
    if req.method != "INVITE" {
        return Err(BridgeError::Ims(format!(
            "expected INVITE on the veth SIP link, got {}",
            req.method
        )));
    }

    let offer = sdp::parse_offer(&req.body)?;
    let rtp_socket = UdpSocket::bind((veth_local_ip, 0))
        .map_err(|e| BridgeError::Ims(format!("veth RTP socket bind failed: {e}")))?;
    let rtp_port = rtp_socket
        .local_addr()
        .map_err(|e| BridgeError::Ims(format!("veth RTP local_addr failed: {e}")))?
        .port();

    let session_id: u64 = rand::random::<u32>() as u64;
    // No AMR on this internal leg — Agent B's PJSIP offers PCMU always and
    // (with its 16 kHz conference bridge) L16/16000, which `build_veth_answer`
    // takes whenever the carrier leg has wideband worth carrying.
    let (answer_sdp, codec) =
        sdp::build_veth_answer(veth_local_ip, rtp_port, session_id, &offer, wideband)?;
    let to_tag = random_hex(4);
    let contact = format!("<sip:agent-a@{veth_local_ip}:{veth_sip_port}>");
    let response = build_200_ok_invite(&req, &to_tag, &contact, &answer_sdp);
    sip_socket
        .send_to(response.as_bytes(), peer)
        .map_err(|e| BridgeError::Ims(format!("veth 200 OK send failed: {e}")))?;

    // Trust the datagram's source address over the SDP's `c=` line, and take
    // only the port from the offer. PJSIP binds media to 0.0.0.0 and
    // advertises the container's *default-route* (LAN) address, which does
    // not exist inside netns "ims" — its only IPv4 route is the veth /30, so
    // connecting to the advertised address fails outright with "Network is
    // unreachable" and the call dies after being answered. On a
    // point-to-point link the peer that just sent us this INVITE is by
    // definition reachable at its source address, which makes this both
    // correct and independent of however the container's LAN is addressed.
    let rtp_dst = SocketAddr::new(peer.ip(), offer.remote_rtp.port());
    if rtp_dst.ip() != offer.remote_rtp.ip() {
        tracing::debug!(
            advertised = %offer.remote_rtp,
            using = %rtp_dst,
            "Agent B advertised a non-veth RTP address; using its veth source address instead"
        );
    }
    rtp_socket
        .connect(rtp_dst)
        .map_err(|e| BridgeError::Ims(format!("veth RTP connect to {rtp_dst} failed: {e}")))?;

    Ok(VethUasResult { rtp_socket, codec })
}

fn spawn_relay(
    carrier: UdpSocket,
    veth: UdpSocket,
    stop: Arc<AtomicBool>,
    meter: &super::media_stats::MediaMeter,
) {
    let carrier_rx = meter.carrier_rx_counter();
    let pbx_rx = meter.pbx_rx_counter();
    std::thread::spawn(move || relay_rtp(carrier, veth, stop, carrier_rx, pbx_rx));
}

/// Relays raw UDP payloads bidirectionally between `a` and `b` (both
/// already `connect()`-ed to their remote peer) until `stop` is set.
/// Forwards bytes verbatim rather than decoding/re-encoding: both legs
/// speak the same codec by construction — `handle_invite` only reaches this
/// point once the carrier offer negotiated PCMU, and Agent B's PJSIP leg is
/// always PCMU too — so the wire bytes (RTP header included: SSRC,
/// sequence, timestamp all stay whatever the real source generated) are
/// already correct for the other side without modification.
pub fn relay_rtp(
    carrier: UdpSocket,
    veth: UdpSocket,
    stop: Arc<AtomicBool>,
    carrier_rx: Arc<std::sync::atomic::AtomicU64>,
    pbx_rx: Arc<std::sync::atomic::AtomicU64>,
) {
    let (carrier2, veth2, stop2) = match (carrier.try_clone(), veth.try_clone()) {
        (Ok(a2), Ok(b2)) => (a2, b2, stop.clone()),
        (Err(e), _) | (_, Err(e)) => {
            tracing::error!(error = %e, "RTP relay socket clone failed, aborting relay");
            return;
        }
    };
    let _ = carrier.set_read_timeout(Some(RELAY_POLL_INTERVAL));
    let _ = veth.set_read_timeout(Some(RELAY_POLL_INTERVAL));

    // Each direction counts what it *receives* at its source: the carrier→veth
    // thread counts downlink from the carrier, the veth→carrier thread counts
    // uplink from the telephone leg. Read together at teardown, they are the
    // FR-017 both-ways verdict.
    let h1 = std::thread::spawn(move || forward(carrier, veth2, stop, carrier_rx));
    let h2 = std::thread::spawn(move || forward(veth, carrier2, stop2, pbx_rx));
    let _ = h1.join();
    let _ = h2.join();
}

fn forward(
    src: UdpSocket,
    dst: UdpSocket,
    stop: Arc<AtomicBool>,
    counter: Arc<std::sync::atomic::AtomicU64>,
) {
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        match src.recv(&mut buf) {
            Ok(n) => {
                super::media_stats::bump(&counter);
                let _ = dst.send(&buf[..n]);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // These moved to `ims::session` in the FR-019 extraction; the tests that
    // cover them stay here, exercising the same implementation.
    use crate::ims::session::{build_subscribe, SubscribeParts};
    use std::net::Ipv4Addr;

    fn loopback_socket() -> UdpSocket {
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap()
    }

    #[test]
    fn a_call_with_flowing_audio_never_probes_the_attachment() {
        // The load-bearing safety property of FR-011's watch: while audio keeps
        // arriving from the carrier, it must never touch the modem — and so can
        // never mistake a healthy call for a dropped attachment. If this holds,
        // a live call cannot be torn down by the watch.
        let mut w = AttachmentWatch::default();
        let probed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probed_c = probed.clone();
        let check = move || {
            probed_c.store(true, Ordering::Relaxed);
            false // would report "down" — but must never be consulted here
        };
        for rx in 1..=1000 {
            assert!(
                !w.attachment_lost(rx, &check),
                "a call with flowing audio must never be declared lost"
            );
        }
        assert!(
            !probed.load(Ordering::Relaxed),
            "a healthy call must never probe the modem"
        );
    }

    #[test]
    fn a_call_that_never_carried_downlink_does_not_immediately_declare_loss() {
        // A brand-new call sits at carrier_rx=0 for its first ticks before media
        // ramps up; the watch must not fire during that window on the strength
        // of the stall alone — the stall only *arms* the modem probe, which has
        // not even been reached yet here.
        let mut w = AttachmentWatch::default();
        let check = || false;
        assert!(!w.attachment_lost(0, &check));
        assert!(!w.attachment_lost(0, &check));
    }

    #[test]
    fn next_backoff_doubles_each_attempt() {
        let b1 = next_backoff(Duration::from_secs(5), Duration::from_secs(120));
        assert_eq!(b1, Duration::from_secs(10));
        let b2 = next_backoff(b1, Duration::from_secs(120));
        assert_eq!(b2, Duration::from_secs(20));
        let b3 = next_backoff(b2, Duration::from_secs(120));
        assert_eq!(b3, Duration::from_secs(40));
    }

    #[test]
    fn next_backoff_caps_at_max() {
        let b = next_backoff(Duration::from_secs(100), Duration::from_secs(120));
        assert_eq!(b, Duration::from_secs(120));
        // Already at (or past) the cap: stays capped, doesn't keep growing.
        let b2 = next_backoff(b, Duration::from_secs(120));
        assert_eq!(b2, Duration::from_secs(120));
    }

    #[test]
    fn next_backoff_never_overflows_on_pathological_input() {
        let b = next_backoff(Duration::MAX, Duration::from_secs(120));
        assert_eq!(b, Duration::from_secs(120));
    }

    #[test]
    fn relay_rtp_forwards_packets_in_both_directions_until_stopped() {
        // Simulate the two "legs": ims_side <-> veth_side, each with its own
        // peer socket standing in for the real remote endpoint.
        let ims_side = loopback_socket();
        let ims_peer = loopback_socket();
        ims_side.connect(ims_peer.local_addr().unwrap()).unwrap();
        ims_peer.connect(ims_side.local_addr().unwrap()).unwrap();

        let veth_side = loopback_socket();
        let veth_peer = loopback_socket();
        veth_side.connect(veth_peer.local_addr().unwrap()).unwrap();
        veth_peer.connect(veth_side.local_addr().unwrap()).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let meter = super::super::media_stats::MediaMeter::new();
        let carrier_rx = meter.carrier_rx_counter();
        let pbx_rx = meter.pbx_rx_counter();
        let handle = std::thread::spawn(move || {
            relay_rtp(ims_side, veth_side, stop_clone, carrier_rx, pbx_rx)
        });

        // ims_peer -> ims_side -> (relay) -> veth_side -> veth_peer
        ims_peer.send(b"hello-from-ims").unwrap();
        veth_peer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buf = [0u8; 64];
        let n = veth_peer.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello-from-ims");

        // veth_peer -> veth_side -> (relay) -> ims_side -> ims_peer
        veth_peer.send(b"hello-from-veth").unwrap();
        ims_peer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let n = ims_peer.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello-from-veth");

        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();

        // Each direction counted the one packet it carried — the input to the
        // FR-017 both-ways verdict.
        assert_eq!(meter.carrier_rx(), 1, "downlink packet should be counted");
        assert_eq!(meter.pbx_rx(), 1, "uplink packet should be counted");
        assert_eq!(
            meter.verdict(super::super::media_stats::DEFAULT_ONE_WAY_THRESHOLD_PERCENT),
            super::super::media_stats::DirectionVerdict::BothWays
        );
    }

    #[test]
    fn build_subscribe_formats_a_reg_event_subscription() {
        let msg = build_subscribe(&SubscribeParts {
            impu: "sip:+919043062139@ims.mnc094.mcc404.3gppnetwork.org",
            route_headers: &["Route: <sip:pcscf.example:6000;lr>".to_string()],
            via_transport: "TCP",
            local_addr: "1.2.3.4:48584".parse().unwrap(),
            contact_addr: "1.2.3.4:48586".parse().unwrap(),
            public_user: "404940965025744",
            call_id: "cid1",
            from_tag: "tag1",
            cseq: 7,
            expires: 3600,
        });
        assert!(msg
            .starts_with("SUBSCRIBE sip:+919043062139@ims.mnc094.mcc404.3gppnetwork.org SIP/2.0"));
        assert!(msg.contains("Route: <sip:pcscf.example:6000;lr>\r\n"));
        assert!(msg
            .contains("From: <sip:+919043062139@ims.mnc094.mcc404.3gppnetwork.org>;tag=tag1\r\n"));
        assert!(msg.contains("To: <sip:+919043062139@ims.mnc094.mcc404.3gppnetwork.org>\r\n"));
        assert!(msg.contains("CSeq: 7 SUBSCRIBE\r\n"));
        assert!(msg.contains("Event: reg\r\n"));
        assert!(msg.contains("Expires: 3600\r\n"));
        assert!(msg.contains("Accept: application/reginfo+xml\r\n"));
        // Contact carries the protected server port, Via the client port.
        assert!(msg.contains("Contact: <sip:404940965025744@1.2.3.4:48586;transport=TCP>\r\n"));
        assert!(msg.contains("Via: SIP/2.0/TCP 1.2.3.4:48584;"));
        assert!(msg.ends_with("Content-Length: 0\r\n\r\n"));
    }

    #[test]
    fn extract_caller_pulls_the_user_part_from_a_quoted_from_header() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: <sip:+919789063708@ims.mnc094.mcc404.3gppnetwork.org>;tag=abc\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw).unwrap().unwrap();
        assert_eq!(extract_caller(&req), "+919789063708");
    }

    #[test]
    fn extract_caller_falls_back_to_unknown_when_from_is_unparseable() {
        let raw = "INVITE sip:x SIP/2.0\r\nFrom: garbage\r\nCall-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw).unwrap().unwrap();
        assert_eq!(extract_caller(&req), "unknown");
    }
}

/// What an inbound probe observed.
#[derive(Debug, Default)]
pub struct InboundProbeReport {
    pub invites: u32,
    pub other_requests: u32,
    /// True once anything at all arrives on the protected port. The probe's
    /// positive control: without it, "no incoming call" is uninterpretable.
    pub port_proven_reachable: bool,
    /// Method and caller for each request the network delivered, in order.
    pub log: Vec<String>,
}

/// Registers, holds the protected server port open, and reports everything the
/// network delivers (specs/017-volte-inbound-bridge).
///
/// This answers that feature's gating question: **does the carrier route
/// mobile-terminating calls to us over this registration at all?** Registration
/// works and reg-event notifications already arrive, but an inbound `INVITE`
/// has never been observed on the LTE path — and if it never arrives, the
/// feature is not buildable rather than merely delayed.
///
/// Deliberately does not answer calls. An `INVITE` is acknowledged and then
/// declined with `486 Busy Here`, so the caller gets a clean, immediate result
/// instead of ringing at nothing — the probe is establishing reachability, not
/// carrying a conversation.
pub fn probe_inbound(
    cfg: &super::ImsRegisterConfig,
    listen_for: Duration,
) -> BridgeResult<InboundProbeReport> {
    let mut session = super::register_session(cfg)?;
    if session.status != 200 {
        let (status, reason) = (session.status, session.reason.clone());
        session.unregister();
        session.cleanup();
        return Err(BridgeError::Ims(format!(
            "registration failed, so nothing could be delivered to us: {status} {reason}"
        )));
    }

    // Positive control. Without this the probe has no way to tell "the carrier
    // does not route calls to us" from "our protected port is unreachable" —
    // and those demand completely different responses. A reg-event
    // notification arriving proves the network can reach us, which is what
    // makes a subsequent *absent* INVITE meaningful evidence.
    let inbound = start_inbound(&session)?;
    subscribe_reg_event(&mut session);
    match session.gm_server_addr() {
        Some(addr) => tracing::info!(
            %addr,
            "registered — listening for network-initiated requests. Dial the SIM now."
        ),
        None => tracing::warn!(
            "registered, but with no protected server port — the network has nowhere to \
             deliver an inbound call"
        ),
    }

    let mut report = InboundProbeReport::default();
    let deadline = Instant::now() + listen_for;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok((msg, sink)) = inbound
            .rx
            .recv_timeout(remaining.min(Duration::from_secs(2)))
        else {
            continue;
        };
        let SipMessage::Request(req) = msg else {
            continue;
        };
        let from = req.header("From").unwrap_or("<unknown>").to_string();
        report.port_proven_reachable = true;
        let entry = format!("{} from {}", req.method, from);
        tracing::info!(method = %req.method, from = %from, "network delivered a request");
        report.log.push(entry);

        if req.method.eq_ignore_ascii_case("INVITE") {
            report.invites += 1;
            // Acknowledge, then decline: the caller gets an immediate busy
            // rather than ringing at a probe that will never answer.
            let _ = sink.send(&build_100_trying(&req));
            let _ = sink.send(&build_486_busy_here(&req, &random_hex(4)));
        } else {
            report.other_requests += 1;
        }
    }

    drop(inbound);
    session.unregister();
    session.cleanup();
    Ok(report)
}
