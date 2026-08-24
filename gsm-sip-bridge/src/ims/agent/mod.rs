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
//!
//! This file holds the entry points and the dispatch loop that multiplexes
//! everything; the concerns it drives live in the submodules below:
//!
//! | Concern | Module |
//! |---|---|
//! | Answering an inbound carrier `INVITE` | [`inbound`] |
//! | Placing an outbound call | [`origination`] |
//! | A bridged call's state and every way it ends | [`call`] |
//! | Gm connection liveness and repair | [`ping`] |
//! | The veth link to Agent B, and the RTP relay | [`veth`] |
//! | The `probe-inbound` diagnostic | [`probe`] |

mod call;
mod inbound;
mod origination;
mod ping;
mod probe;
mod veth;
pub(crate) mod watchdog;

use crate::config::VowifiConfig;
use crate::control::protocol::{AgentKind, BridgeFailureReason, CallStatus, RegistrationStatus};
use crate::error::{BridgeError, BridgeResult};
use crate::ims::lifecycle::{
    Admission, EndedBy, Maintenance, MaintenanceDecision, MaintenancePolicy,
};
use crate::ims::observability;
// Extracted to `ims::session` so the host-side cellular service uses the same
// implementation rather than a copy (FR-019, SC-008). Imported by name so the
// call sites below read exactly as they did before the move.
use crate::ims::sdp;
use crate::ims::session::{
    attempt_renewal, extract_caller, handle_notify, header_uri, map_registration_error,
    map_registration_status_code, next_backoff, respond, send_sms_delivery_report, start_inbound,
    subscribe_reg_event, to_unix, Inbound,
};
use crate::ims::sip_client::{
    build_200_ok_bye, build_200_ok_message, build_486_busy_here, build_uas_response,
    build_uas_response_with_headers, random_hex, SipMessage, SipRequest, SipResponse, SipSink,
};
use crate::ims::transport::{EpdgTransport, ImsTransport};
use crate::ims::ImsRegisterConfig;
use crate::observability::reporter::Reporter;
use crate::store::StoreHandle;
use crate::vowifi::control::{read_msg, reason, write_msg, ControlMessage};
use crate::vowifi::VETH_SIP_PORT;
use chrono::Utc;
use std::io::BufReader;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use call::{handle_bye, hangup_carrier, report_answered_call_ended, ActiveCall, AttachmentWatch};
use origination::{
    begin_origination, tick_pending_origination, OriginationStatus, PendingOrigination,
};
use ping::{parse_cseq_number, probe_gm_connection, PingState};

pub(crate) use call::AttachmentHook;
pub use probe::{probe_inbound, InboundProbeReport};
pub use veth::relay_rtp;

/// How long to wait for *any* response at all (even a bare `100 Trying`)
/// to an originated INVITE — the first phase of
/// `SipTransport::recv_final_response_for_origination`. If nothing arrives
/// in this window, something transport-level is actually wrong, not just
/// "the phone hasn't picked up yet". Well under RFC 3261 Timer B (32s).
/// `pub(crate)`: see `OUTBOUND_RING_TIMEOUT`'s doc for why both of these
/// need to stay visible to `vowifi::mod`. Declared here rather than in
/// [`origination`] (which is what actually applies them) only because a
/// `pub(crate) use` re-export used solely from another module's tests reads
/// as unused in a non-test build.
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

/// How long Agent A waits for Agent B to *place* its two legs (`BridgeReady`)
/// before giving up and declining the carrier's INVITE. Only covers getting the
/// PBX ringing — the wait for a human to actually pick up is `RING_TIMEOUT`,
/// and the caller hears ringback throughout it.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(4);
/// How often the dispatch loop comes up for air while a call is up, so a
/// hangup that starts on the PBX side is turned into a `BYE` toward the carrier
/// promptly rather than leaving the caller on a dead line.
const ACTIVE_CALL_POLL_INTERVAL: Duration = Duration::from_millis(100);
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

/// Whether this line's own modem storage should be swept for SMS the carrier
/// delivered through the classic cellular bearer instead of the IMS
/// registration (specs/038-reliable-sms-delivery). `false` for a `pcsc_reader`
/// line: the SIM there sits in a PC/SC reader with no modem/cellular attach at
/// all, so there is no such bearer, and no `modem_port` to open either.
fn wants_modem_sms_reader(pcsc_reader: bool) -> bool {
    !pcsc_reader
}

fn run_inner(
    card_id: &str,
    config: &VowifiConfig,
    app_config: &crate::config::AppConfig,
) -> BridgeResult<()> {
    // Arm stall detection before anything touches the modem
    // (specs/039-at-stall-watchdog). `derive_plmn` below opens the AT port, and
    // that open is itself a wedge point — starting the watchdog only once the
    // dispatch loop exists would leave the whole startup path unmonitored.
    let progress = watchdog::register(Arc::new(watchdog::Progress::new("ims-dispatch")));
    watchdog::spawn(config.watchdog_recovery_enabled)?;
    let _startup = progress.phase_guard(watchdog::Phase::Startup);

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
        register_uri_home_domain: config.register_request_uri == "home-domain",
        gm_auth_alg: Some(config.gm_auth_alg.clone()).filter(|s| !s.is_empty()),
        gm_cipher_alg: Some(config.gm_cipher_alg.clone()).filter(|s| !s.is_empty()),
    };

    let veth_local_ip: IpAddr = config
        .veth_local_addr
        .parse()
        .map_err(|e| BridgeError::Ims(format!("invalid vowifi.veth_local_addr: {e}")))?;
    let control_addr: SocketAddr = format!("{}:{}", config.veth_peer_addr, config.control_port)
        .parse()
        .map_err(|e| BridgeError::Ims(format!("invalid vowifi control address: {e}")))?;

    // Every line gets a dedupe, whether or not it has a modem: `handle_message`
    // always consults one (specs/038-reliable-sms-delivery). Only a line with
    // a real modem also gets a `modem_lock` and a background sweep of that
    // modem's own SMS storage — a `pcsc_reader` line has no AT port to
    // protect and no cellular bearer to poll.
    let dedupe = Arc::new(Mutex::new(crate::volte::sms::Dedupe::default()));
    let modem_lock = if wants_modem_sms_reader(config.pcsc_reader) {
        let lock = Arc::new(crate::modules::modem_lock::ModemLock::new());
        let modem_port = PathBuf::from(&config.modem_port);
        let sweep_lock = lock.clone();
        let sweep_dedupe = dedupe.clone();
        if let Err(e) = std::thread::Builder::new()
            .name(format!("vowifi-sms-{card_id}"))
            .spawn(move || {
                crate::volte::sms::run_modem_reader(
                    modem_port,
                    control_addr,
                    sweep_lock,
                    sweep_dedupe,
                )
            })
        {
            tracing::error!(card_id, error = %e, "failed to start the modem SMS reader for this line");
        }
        Some(lock)
    } else {
        None
    };

    serve_inbound(InboundParams {
        card_id,
        reg_cfg: &reg_cfg,
        local_ip: veth_local_ip,
        control_addr,
        // Each Wi-Fi Agent A is alone in its own netns, so they all share the
        // one well-known status port.
        status_port: crate::vowifi::AGENT_A_STATUS_PORT,
        wideband: config.wideband,
        respond_on_client: config.respond_on_client,
        // The Wi-Fi path keeps its long-standing answer ordering (FR-020) and
        // has no attachment of its own to refresh.
        answer_preference: sdp::AnswerPreference::legacy(),
        veth_sip_port: VETH_SIP_PORT,
        pre_renewal: None,
        // The ePDG tunnel is charon's to watch, and a lost tunnel already
        // surfaces as the control connection dropping — no mid-call probe here.
        attachment_check: None,
        // `Some` for any line with a real modem, serialising registration/renewal
        // AT access with the modem SMS reader spawned above (specs/038) — `None`
        // only for a `pcsc_reader` line, which has no AT port at all.
        modem_lock,
        dedupe,
        // Wi-Fi Agent A cannot see Agent B's PBX registration (separate
        // processes), so it does not gate on it.
        pbx_registered: None,
        app_config,
        progress: Arc::clone(&progress),
        agent_label: "vowifi-ims-agent",
        agent_kind: AgentKind::Ims,
    })
}

/// Every method this UAS serves, and the exact `Allow` we state in a response.
///
/// One string for both so the claim and the behaviour cannot drift: each of
/// these has an arm in [`dispatch_loop`], and anything absent from it is
/// answered `405 Method Not Allowed` by [`unserved_method_response`] — which
/// states this same list back, as RFC 3261 §21.4.6 requires.
///
/// Shorter than what a real UE sends, deliberately. `Allow` is how the network
/// decides what it may send us, so listing `UPDATE`, `PRACK`, `INFO` and
/// `REFER` — as this did while nothing answered them — invites mid-call
/// requests we can only refuse. Growing the list is a matter of growing the
/// UAS, in that order.
const ALLOW: &str = "INVITE, ACK, CANCEL, BYE, OPTIONS, MESSAGE, NOTIFY";

/// Answers the carrier's `OPTIONS` — the keepalive a P-CSCF (and our own
/// `ping`, in the other direction) uses to decide whether a UE is still there.
/// Silence is indistinguishable from a UE that has gone away, and RFC 3261
/// §11.2 wants the same `Allow` a 200 to an INVITE carries.
fn options_response(req: &SipRequest, to_tag: &str) -> String {
    build_uas_response_with_headers(
        200,
        "OK",
        req,
        Some(to_tag),
        None,
        None,
        &[("Allow", ALLOW), ("Accept", "application/sdp")],
    )
}

/// Answers a request this UAS does not serve.
///
/// Before this, anything outside [`ALLOW`] was logged and dropped. A request
/// left unanswered is not "ignored" from the network's side — it retransmits
/// on the RFC 3261 §17.2.1 timers and then draws its own conclusion about the
/// endpoint, which is a worse outcome than a clear refusal and much harder to
/// read in a capture.
///
/// A `CANCEL` is the exception, and gets `481` rather than `405`: the one that
/// matters is answered inside `inbound::handle_invite`'s ring loop, so a
/// `CANCEL` reaching here names a transaction that is already over or was
/// never ours — the method is served, the transaction is gone (§9.2).
fn unserved_method_response(req: &SipRequest, to_tag: &str) -> String {
    if req.method == "CANCEL" {
        return build_uas_response(
            481,
            "Call/Transaction Does Not Exist",
            req,
            Some(to_tag),
            None,
            None,
        );
    }
    build_uas_response_with_headers(
        405,
        "Method Not Allowed",
        req,
        Some(to_tag),
        None,
        None,
        &[("Allow", ALLOW)],
    )
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
    /// Answer a network-initiated request over the Gm client leg rather than
    /// the socket it arrived on — a carrier quirk, off everywhere but where a
    /// capture says otherwise. See `config::VowifiConfig::respond_on_client`.
    pub respond_on_client: bool,
    pub answer_preference: sdp::AnswerPreference,
    /// Port the telephone-side half dials for its leg. The two halves must
    /// agree; see `inbound::handle_invite`.
    pub veth_sip_port: u16,
    pub pre_renewal: Option<&'a PreRenewalHook>,
    /// Checks the network attachment during a call so a mid-call loss ends it
    /// with the cause stated (FR-011). `None` on the Wi-Fi path.
    pub attachment_check: Option<&'a AttachmentHook>,
    /// Serialises this half's modem AT access (registration, renewal) with any
    /// other user of the same port — the modem SMS reader (specs/038), on
    /// either path now. `None` only for a `pcsc_reader` line, which has no
    /// modem/AT port at all.
    pub modem_lock: Option<Arc<crate::modules::modem_lock::ModemLock>>,
    /// Shared with this line's modem SMS reader (specs/038-reliable-sms-delivery)
    /// so a message delivered over both the registration and the modem
    /// collapses to one: whichever route sees it first wins, the other
    /// acknowledges/clears without forwarding again. Always present — every
    /// line has one, unlike `modem_lock`, which only exists where there is an
    /// AT port to protect.
    pub dedupe: Arc<Mutex<crate::volte::sms::Dedupe>>,
    /// Whether the telephone-side half holds the PBX registration the outbound
    /// bridge leg needs — shared from that half (cellular only; the two halves
    /// are one process there). `None` on the Wi-Fi path, where health does not
    /// track the PBX leg and so treats it as available.
    pub pbx_registered: Option<Arc<AtomicBool>>,
    pub app_config: &'a crate::config::AppConfig,
    /// This line's stall-detection handle (specs/039-at-stall-watchdog).
    /// Created and registered by the caller before it touches the modem, so
    /// the startup path is monitored too, and shared with the dispatch loop
    /// here so every blocking region publishes which phase it is in.
    pub progress: Arc<watchdog::Progress>,
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
        respond_on_client,
        answer_preference,
        veth_sip_port,
        pre_renewal,
        attachment_check,
        modem_lock,
        dedupe,
        pbx_registered,
        app_config,
        progress,
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
    // Watched, so the heartbeat stops while this line is stalled and the
    // liveness gauges tell the truth (specs/039-at-stall-watchdog, FR-021).
    let reporter = Reporter::spawn_watched(
        app_config.control.socket_path.clone(),
        agent_kind,
        card_id.to_string(),
        Duration::from_secs(app_config.metrics.agent_report_interval_seconds),
        Some(Arc::clone(&progress)),
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
        // A bounded acquire: if another user of this modem is wedged, fail the
        // registration attempt rather than waiting on it forever. The
        // supervisor retries, which is a far better outcome than an agent that
        // never finishes starting up and never says why.
        let _guard = match modem_lock.as_ref() {
            Some(l) => match l.lock() {
                Some(g) => Some(g),
                None => {
                    return Err(BridgeError::Ims(
                        "could not get the modem to register: another user of the AT port has \
                         held it beyond the timeout"
                            .into(),
                    ))
                }
            },
            None => None,
        };
        match crate::ims::register_session(reg_cfg) {
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
    obs.set_registration_expiry(
        SystemTime::now()
            + Duration::from_secs(session.granted_expires(crate::ims::DEFAULT_EXPIRES) as u64),
    );
    // Before the SUBSCRIBE, so the listeners are up to catch its response and
    // the NOTIFY the network sends straight back on a new connection.
    let mut inbound = start_inbound(&session)?;
    subscribe_reg_event(&mut session);

    // What the registrar actually granted, not what we asked for: renewing on
    // the requested value would leave a window where the binding has lapsed
    // while we still believe it is live (FR-023).
    let granted_expires = session.granted_expires(crate::ims::DEFAULT_EXPIRES);
    let status = Arc::new(Mutex::new(crate::ims::RegistrationStatus {
        state: crate::ims::RegistrationState::Registered,
        registered_at: Some(SystemTime::now()),
        expires_at: Some(SystemTime::now() + Duration::from_secs(granted_expires as u64)),
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
        &DispatchParams {
            reg_cfg,
            status: &status,
            control_addr,
            veth_local_ip: local_ip,
            wideband,
            respond_on_client,
            answer_preference,
            veth_sip_port,
            pre_renewal,
            attachment_check,
            modem_lock: modem_lock.as_ref(),
            dedupe: &dedupe,
            // Read from `[vowifi]` on both paths deliberately: SMS-over-IP is
            // the same TS 24.341 procedure over LTE as over Wi-Fi, so one
            // switch governs both rather than two that could disagree.
            sms_delivery_report: app_config.vowifi.sms_delivery_report,
            pbx_registered: pbx_registered.as_ref(),
            obs: &obs,
            progress: &progress,
        },
        place_call_rx,
    );
    session.unregister();
    session.cleanup();
    result
}

/// A `PlaceCall` (specs/025-outbound-calling) handed off by
/// `run_status_listener` to the dispatch loop: the still-open connection back
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
/// `place_call_tx` rather than answered inline here. The dispatch loop does
/// the actual work single-threadedly, since it is the sole owner of
/// `session` — this thread's only job is accepting and routing.
fn run_status_listener(
    veth_local_ip: IpAddr,
    status_port: u16,
    status: Arc<Mutex<crate::ims::RegistrationStatus>>,
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
                    gm_connection: snapshot.gm_connection.render(),
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

/// Decodes an inbound `MESSAGE`'s body when it's 3GPP SMS-over-IP
/// (`Content-Type: application/vnd.3gpp.sms`, TS 24.341) rather than text —
/// which is what a real carrier sends (Vi, measured 2026-08-15), and
/// `req.body` alone is unreadable for it: it's a raw binary TPDU, not text
/// that happened to look garbled. `None` for a plain-text `MESSAGE` (no
/// content type to key off, or one naming something else), so the caller's
/// existing `req.body`/`extract_caller` behaviour is unchanged for those.
///
/// Logs and returns `None` on a decode failure too, rather than propagating
/// the error — decoding better beats decoding not at all, but a `MESSAGE` we
/// can't parse is still one we received, and the caller must still relay and
/// acknowledge *something* rather than drop it (specs/017 FR-026's ordering
/// exists precisely so a message is never silently lost).
fn decode_pdu_body(req: &SipRequest) -> Option<crate::ims::sms_pdu::DecodedSms> {
    let is_3gpp_sms = req
        .header("Content-Type")
        .is_some_and(|ct| ct.trim().eq_ignore_ascii_case("application/vnd.3gpp.sms"));
    if !is_3gpp_sms {
        return None;
    }
    match crate::ims::sms_pdu::decode_vnd_3gpp_sms(&req.body_bytes) {
        Ok(decoded) => Some(decoded),
        Err(e) => {
            tracing::warn!(
                error = %e,
                body_len = req.body_bytes.len(),
                "could not decode a 3GPP SMS-over-IP body; forwarding it undecoded"
            );
            None
        }
    }
}

/// Handles an inbound SIP `MESSAGE` (RFC 3428) — the carrier's VoWiFi/IMS
/// transport for SMS, the counterpart to `AT+CMTI`/`AT+CMGR` in the
/// circuit-switched flow — and relays it to Agent B over the control channel
/// so it can be forwarded to Discord the same way. Agent B, not Agent A, owns
/// the actual Discord post — it holds the `[sms]` webhook config and has
/// LAN/Internet reachability, whereas Agent A's netns is IMS-tunnel-only (see
/// `ControlMessage::SmsReceived` docs).
///
/// # Two acknowledgements, not one
///
/// A `MESSAGE` carrying 3GPP SMS-over-IP is owed two separate answers, and
/// [`acknowledge`] sends both: the bare `200 OK` RFC 3428 asks for, saying
/// the *request* arrived, and — as a `MESSAGE` request of its own — the
/// delivery report of TS 24.341 §5.3.2.4, saying the *short message* was
/// taken. See [`send_sms_delivery_report`] for why the second one cannot ride
/// in the first's body.
///
/// # Hand it on before acknowledging it
///
/// The acknowledgement goes out **after** the message has been handed to the
/// half that records it, never before. This ordering is the whole safety
/// property (specs/017 FR-026), and it now governs the delivery report too —
/// that report, not the `200 OK`, is what stops the network retrying:
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
///
/// # Sharing `dedupe` with the modem SMS reader (specs/038)
///
/// `dedupe` is the same instance this line's modem-storage sweep
/// (`volte::sms::run_modem_reader`) consults. A carrier occasionally delivers
/// the identical text over both bearers; without a shared `Dedupe` each side
/// would admit it independently and the operator would see it twice. Whichever
/// side sees a message first calls it `Disposition::Handle` and the other gets
/// `Disposition::AcknowledgeOnly` — still acknowledged/cleared so the network
/// or the modem's own storage does not keep it pending, just not recorded or
/// forwarded again.
///
/// The two acknowledgements part company on that path. The `200 OK` closes
/// the SIP transaction either way, but the delivery report — which is what
/// actually stops the network retrying — waits for the other side's claim to
/// be `is_confirmed`. An admitted-but-unconfirmed claim can still fail and
/// `forget` itself, and the retransmission we would otherwise have suppressed
/// is exactly how that failure recovers.
///
/// Admission happens *before* the relay below, not after: checking
/// `contains` early but only calling `admit` once the relay is known to have
/// succeeded looks safer but is not — it reopens a window where this
/// function and the modem sweep, running on different threads, can both see
/// "not yet admitted" while each other's relay is in flight, and both then
/// relay it. [`decide`] closes that window by admitting atomically. If the
/// relay then fails, [`Dedupe::forget`] releases the admission so the
/// network's retransmission is treated as fresh rather than silently
/// swallowed as a duplicate of a delivery that never actually happened.
fn handle_message(
    session: &mut crate::ims::RegisteredSession,
    p: &DispatchParams,
    req: &SipRequest,
    sink: &SipSink,
) {
    // The SIP `From` on a real network's MT SMS names an IMS core element
    // relaying the message (a carrier-internal SMSC gateway hostname), not
    // the person who sent it — measured on Vi 2026-08-15:
    // `From: <sip:invitn14cbt5tasx05nk.ims.mnc043.mcc404.3gppnetwork.org>`.
    // The real sender is inside the TPDU (`decode_pdu_body` below), which is
    // why a successful decode's `sender` always wins over `extract_caller`.
    let mut sender = extract_caller(req);
    let mut body = req.body.clone();

    // Kept past the relay, not consumed by it: `rp_mr` is needed again at
    // whichever acknowledgement point this message reaches.
    let decoded = decode_pdu_body(req);
    if let Some(decoded) = &decoded {
        sender = decoded.sender.clone();
        body = match decoded.part {
            Some((seq, total)) => format!("[{seq}/{total}] {}", decoded.text),
            None => decoded.text.clone(),
        };
    }

    // TS 23.040 §9.2.3.9: a Short Message Type 0 is the network asking "is
    // this subscriber reachable?", not a message for anyone. The spec is
    // explicit that it must be acknowledged and its contents discarded, so it
    // returns here — before the dedupe, before the relay, before the store.
    //
    // Treating one as an ordinary text is what put a stream of identical
    // notifications in front of the operator (Jio probes this line every
    // couple of minutes), and made it look as though every message sent to
    // the line arrived with the same body. It is acknowledged exactly as any
    // other message is, delivery report included — that acknowledgement is
    // the entire point of the probe. The only change is that nothing
    // downstream ever hears of it.
    //
    // Unconditional, unlike the duplicate path above: there is no competing
    // claim to wait on, because a probe is never relayed to anyone.
    if decoded.as_ref().is_some_and(|d| d.is_type_zero) {
        tracing::info!(
            sender = %sender,
            "silent Type 0 SMS (reachability probe); acknowledging without recording it"
        );
        acknowledge(session, sink, req, decoded.as_ref(), p.sms_delivery_report);
        return;
    }

    let route = crate::volte::sms::MessageRoute::OverRegistration;
    tracing::info!(sender = %sender, route = route.as_str(), "received SIP MESSAGE");

    let inbound = crate::volte::sms::InboundMessage {
        route,
        sender: sender.clone(),
        body: body.clone(),
        modem_index: None,
    };
    let key = inbound.dedupe_key();
    let disposition = {
        let mut d = p.dedupe.lock().unwrap_or_else(|e| e.into_inner());
        crate::volte::sms::decide(&mut d, &inbound)
    };
    if disposition == crate::volte::sms::Disposition::AcknowledgeOnly {
        // Already handled via the other bearer (or a prior delivery of this
        // same MESSAGE) in this same process — the network still needs the
        // SIP transaction closed, but it must not be recorded or forwarded
        // again.
        //
        // The delivery report is held back unless that other claim is
        // *confirmed*. `Dedupe::admit` happens before the claimant's relay,
        // so a claim can still be in flight — and if it fails, the claimant
        // calls `forget` and relies on the network retransmitting to recover
        // the message. Reporting delivery here is what stops that
        // retransmission, so doing it on the strength of an unconfirmed claim
        // would turn a recoverable relay failure into a lost text. That is
        // the distinction `Dedupe::is_confirmed` exists to draw, and the one
        // `contains`'s own docs warn about.
        //
        // Costs nothing when the claim does succeed: the network retries,
        // the claim is confirmed by then, and that retry gets the report.
        let claim_confirmed = p
            .dedupe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_confirmed(&key);
        acknowledge(
            session,
            sink,
            req,
            decoded.as_ref(),
            p.sms_delivery_report && claim_confirmed,
        );
        return;
    }

    let msg = ControlMessage::SmsReceived {
        sender: sender.clone(),
        body,
        received_at: chrono::Utc::now().to_rfc3339(),
    };
    let relayed = match TcpStream::connect_timeout(&p.control_addr, CONTROL_TIMEOUT) {
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
        // Durably delivered now, not merely claimed — the modem sweep may be
        // waiting on exactly this distinction before it trusts this claim
        // enough to discard its own backup copy (specs/038 review follow-up).
        p.dedupe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .confirm(&key);
        acknowledge(session, sink, req, decoded.as_ref(), p.sms_delivery_report);
    } else {
        // Release the admission above so the retransmission this triggers is
        // treated as fresh, not as a duplicate of a delivery that never
        // happened.
        p.dedupe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .forget(&key);
        // Deliberately silent toward the network — at *both* layers: neither
        // the `200 OK` nor the delivery report goes out. An unacknowledged
        // MESSAGE is retransmitted, which is the recovery we want, and a
        // delivery report saying we took a message we failed to record would
        // discard the only chance to get it back.
        tracing::warn!(
            sender = %sender,
            "not acknowledging the MESSAGE so the network retransmits it"
        );
    }
}

/// Answer an inbound `MESSAGE` at both layers it is owed one: the SIP
/// transaction, and — for 3GPP SMS-over-IP only — the RP layer the short
/// message itself rides on.
///
/// `decoded` being `None` means a plain-text `MESSAGE` with no RP layer to
/// acknowledge, so the `200 OK` is the whole of it. That is also the fallback
/// when a 3GPP body failed to decode: the `200 OK` still goes out (the
/// message was received and relayed either way), but no report claims a
/// message we could not read, so the network is left free to retry.
fn acknowledge(
    session: &mut crate::ims::RegisteredSession,
    sink: &SipSink,
    req: &SipRequest,
    decoded: Option<&crate::ims::sms_pdu::DecodedSms>,
    send_delivery_report: bool,
) {
    respond(
        sink,
        "200 OK (MESSAGE)",
        &build_200_ok_message(req, &random_hex(4)),
    );

    let Some(decoded) = decoded.filter(|_| send_delivery_report) else {
        return;
    };
    // TS 24.341 §5.3.2.4 NOTE 1: the IP-SM-GW to report to is the one named
    // in the delivered message's `P-Asserted-Identity`. Falling back to
    // `From` is not in the spec — it is for a core that omits the asserted
    // identity, where `From` names the same gateway (measured on Vi
    // 2026-08-15) and reporting to it beats not reporting at all.
    let Some(ipsmgw) = header_uri(req, "P-Asserted-Identity").or_else(|| header_uri(req, "From"))
    else {
        tracing::warn!(
            "no P-Asserted-Identity or From URI on an SMS MESSAGE; cannot address a delivery report"
        );
        return;
    };
    send_sms_delivery_report(
        session,
        &ipsmgw,
        &crate::ims::sms_pdu::build_rp_ack(decoded.rp_mr),
    );
}

/// Everything the dispatch loop needs that does not change across iterations.
/// Was fifteen positional parameters, several of them same-typed `Option<&_>`.
struct DispatchParams<'a> {
    reg_cfg: &'a ImsRegisterConfig,
    status: &'a Arc<Mutex<crate::ims::RegistrationStatus>>,
    control_addr: SocketAddr,
    veth_local_ip: IpAddr,
    wideband: bool,
    /// See `config::VowifiConfig::respond_on_client`; consumed by the receive
    /// in `dispatch_loop`.
    respond_on_client: bool,
    answer_preference: sdp::AnswerPreference,
    veth_sip_port: u16,
    pre_renewal: Option<&'a PreRenewalHook>,
    attachment_check: Option<&'a AttachmentHook>,
    modem_lock: Option<&'a Arc<crate::modules::modem_lock::ModemLock>>,
    dedupe: &'a Arc<Mutex<crate::volte::sms::Dedupe>>,
    /// See `config::VowifiConfig::sms_delivery_report`; consumed by
    /// [`acknowledge`].
    sms_delivery_report: bool,
    pbx_registered: Option<&'a Arc<AtomicBool>>,
    obs: &'a observability::AgentObservability,
    /// Publishes which phase the loop is in, so the watchdog can tell a line
    /// that is working from one that has stopped (specs/039-at-stall-watchdog).
    progress: &'a Arc<watchdog::Progress>,
}

impl DispatchParams<'_> {
    fn invite_ctx(&self) -> inbound::InviteContext<'_> {
        inbound::InviteContext {
            control_addr: self.control_addr,
            veth_local_ip: self.veth_local_ip,
            wideband: self.wideband,
            answer_preference: self.answer_preference,
            veth_sip_port: self.veth_sip_port,
            obs: self.obs,
        }
    }

    fn origination_setup(&self) -> origination::OriginationSetup {
        origination::OriginationSetup {
            veth_local_ip: self.veth_local_ip,
            veth_sip_port: self.veth_sip_port,
            wideband: self.wideband,
        }
    }
}

/// The state the dispatch loop carries across iterations.
struct LoopState {
    active_call: Option<ActiveCall>,
    /// An outbound call being placed but not yet bridged (specs/029). Held
    /// here, not blocked on inside a helper, so the loop keeps answering
    /// everything else — inbound INVITEs, a caller hangup, the Gm keepalive —
    /// while the carrier is still ringing.
    origination: Option<PendingOrigination>,
    backoff: Duration,
    /// Set after a failed renewal, cleared on success. Gates *retries* only —
    /// unlike a blocking `thread::sleep(backoff)` (the previous approach),
    /// this loop keeps calling `inbound.rx.recv_timeout` every iteration
    /// regardless, so an inbound INVITE/BYE arriving during the backoff
    /// window is still dispatched immediately instead of queuing unanswered
    /// until the sleep ends (a carrier's transaction timer can expire and
    /// drop an otherwise-valid call within that window — found in review,
    /// not live-testing).
    next_renewal_attempt: Option<Instant>,
    /// Formalises the "maintenance must yield to a call" rule
    /// (`ims::lifecycle`): it decides whether a due renewal may run or must be
    /// held for the call in progress, and remembers that it was held so status
    /// can report the deferral as deliberate rather than as a stall (the
    /// re-attachment the renewal hook performs inherits the same deferral —
    /// see `PreRenewalHook`).
    maintenance: MaintenancePolicy,
    /// FR-011 mid-call attachment watch, reset per call.
    watch: AttachmentWatch,
    /// specs/028-gm-tcp-reconnect: Gm signaling-connection liveness. `ping`
    /// drives the idle OPTIONS keepalive; `reconnect_attempts` counts
    /// consecutive repair failures for the current episode (reset on a
    /// confirmed recovery); `force_renewal`, once set, makes the next idle
    /// iteration escalate to a full re-registration even though the
    /// registration is nowhere near expiry — the only thing that can
    /// renegotiate a dead Gm SA. `gm_conn` is the reported health, synced into
    /// the shared status each poll.
    ping: PingState,
    reconnect_attempts: u32,
    force_renewal: bool,
    gm_conn: crate::ims::GmConnectionState,
}

impl LoopState {
    fn new() -> Self {
        Self {
            active_call: None,
            origination: None,
            backoff: RETRY_INITIAL_BACKOFF,
            next_renewal_attempt: None,
            maintenance: MaintenancePolicy::new(),
            watch: AttachmentWatch::default(),
            ping: PingState::default(),
            reconnect_attempts: 0,
            force_renewal: false,
            gm_conn: crate::ims::GmConnectionState::Up,
        }
    }

    /// Is this line occupied — by a bridged call or an attempt still being
    /// placed? An in-flight origination counts: its INVITE transaction is live
    /// on this transport (specs/029).
    fn busy(&self) -> bool {
        self.active_call.is_some() || self.origination.is_some()
    }
}

fn dispatch_loop(
    session: &mut crate::ims::RegisteredSession,
    inbound: &mut Inbound,
    p: &DispatchParams,
    place_call_rx: mpsc::Receiver<PendingPlaceCall>,
) -> BridgeResult<()> {
    let mut st = LoopState::new();
    let respond_on_client = p.respond_on_client;
    if respond_on_client {
        tracing::warn!(
            "vowifi.respond_on_client is set — answering network-initiated requests on the client leg, not the socket they arrived on"
        );
    }
    loop {
        st.sync_shared_status(p);

        if st.handle_pbx_hangup(session, inbound, p) {
            continue;
        }
        if st.handle_attachment_loss(session, p) {
            continue;
        }
        st.advance_origination(session, p);
        if st.handle_place_call_request(session, p, &place_call_rx) {
            continue;
        }

        // Poll fast enough to notice a PBX-side hangup promptly while a call is
        // up — or a caller hangup / veth leg while an origination is in flight
        // (specs/029, so abandonment is observed within ~100ms rather than up
        // to ~80s); idle otherwise, where the only deadline is registration
        // renewal.
        let poll = if st.busy() {
            ACTIVE_CALL_POLL_INTERVAL
        } else {
            IDLE_POLL_INTERVAL
        };
        // Every completed pass of this loop is the definition of "still making
        // progress" (specs/039-at-stall-watchdog). Re-entering `Idle` each
        // iteration restarts the phase clock, so only a pass that never
        // finishes — because something inside it blocked — can go over budget.
        // `busy` drives the watchdog's deferral: a stalled control loop often
        // leaves a call's audio intact, so it is worth not killing.
        p.progress.set_busy(st.busy());
        p.progress.enter(watchdog::Phase::Idle);
        // `vowifi.respond_on_client`: answer a network-initiated request over
        // the *client* leg instead of the socket it arrived on.
        //
        // Jio ignores every response we send from `port_us` — verified on the
        // wire to both of its protected ports, with the correct SPI,
        // monotonic ESP sequence, and well-formed SIP. Yet it validates our
        // ESP on the client SA (`spi-s`) continuously, and TS 33.203 keys all
        // four SAs from one `IK`, so there is no cryptographic difference
        // between the packets it accepts and the ones it drops. Replying on
        // the SA it demonstrably trusts is the pairing that works there.
        //
        // It contradicts RFC 3261 §18.2.2, so it is off unless a line's
        // config asks for it — Airtel and Vi answer correctly on the arrival
        // socket today.
        let received = match inbound.rx.recv_timeout(poll) {
            Ok((msg, sink)) if respond_on_client && matches!(msg, SipMessage::Request(_)) => {
                match session.transport().and_then(|t| t.sink()) {
                    Ok(client_sink) => {
                        tracing::debug!(
                            "respond_on_client: answering on the client leg, not the arrival socket"
                        );
                        Ok((msg, client_sink))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "respond_on_client is set but there is no client sink; using the arrival socket");
                        Ok((msg, sink))
                    }
                }
            }
            other => other,
        };
        match received {
            Ok((SipMessage::Request(req), sink)) if req.method == "INVITE" => {
                let _phase = p.progress.phase_guard(watchdog::Phase::InboundCall);
                st.handle_inbound_invite(session, inbound, p, &req, &sink);
            }
            Ok((SipMessage::Request(req), sink)) if req.method == "BYE" => {
                st.handle_carrier_bye(p, &req, &sink);
            }
            Ok((SipMessage::Request(req), _)) if req.method == "ACK" => {
                tracing::debug!("received ACK, dialog confirmed");
            }
            Ok((SipMessage::Request(req), sink)) if req.method == "NOTIFY" => {
                handle_notify(&sink, &req);
            }
            Ok((SipMessage::Request(req), sink)) if req.method == "MESSAGE" => {
                handle_message(session, p, &req, &sink);
            }
            Ok((SipMessage::Request(req), sink)) if req.method == "OPTIONS" => {
                tracing::debug!("received OPTIONS keepalive; answering 200 OK");
                respond(
                    &sink,
                    "200 OK (OPTIONS)",
                    &options_response(&req, &random_hex(4)),
                );
            }
            Ok((SipMessage::Request(req), sink)) => {
                tracing::info!(
                    method = %req.method,
                    "inbound request this UAS does not serve; refusing it explicitly"
                );
                respond(
                    &sink,
                    "response to an unserved method",
                    &unserved_method_response(&req, &random_hex(4)),
                );
            }
            Ok((SipMessage::Response(resp), _)) => {
                st.handle_carrier_response(session, p, &resp);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(BridgeError::Ims(
                    "every Gm connection reader has stopped; the registration is unreachable"
                        .into(),
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                st.on_idle_tick(session, inbound, p)?;
            }
        }
    }
}

impl LoopState {
    /// Keep the shared health inputs the status listener reads current — the
    /// busy flag and any deferred maintenance — so a `volte-status` query is
    /// answered from the same state the loop is acting on. Cheap: one lock
    /// per poll, and the values are eventually consistent within a poll
    /// interval regardless.
    fn sync_shared_status(&self, p: &DispatchParams) {
        let mut guard = p.status.lock().unwrap_or_else(|e| e.into_inner());
        guard.busy = self.busy();
        guard.deferred_maintenance = self.maintenance.deferred();
        // Reflect the telephone-side half's PBX registration. Absent (Wi-Fi
        // path), the PBX leg is not tracked here, so treat it as available
        // rather than falsely reporting the line unable to answer.
        guard.pbx_registered = p
            .pbx_registered
            .is_none_or(|f| f.load(std::sync::atomic::Ordering::SeqCst));
        // Reported Gm connection health (specs/028), kept current for a
        // `vowifi-status`/`volte-status` query the same way `busy` is.
        guard.gm_connection = self.gm_conn;
    }

    /// A hangup can start on *either* side. The carrier's arrives as a BYE;
    /// the PBX's arrives here, as a `CallEnded` from Agent B — and must be
    /// turned into a BYE toward the carrier, or hanging up the SIP extension
    /// would leave the caller listening to a call that is already over.
    ///
    /// Returns `true` when the call ended and the loop should restart.
    fn handle_pbx_hangup(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        inbound: &Inbound,
        p: &DispatchParams,
    ) -> bool {
        let Some(call) = &mut self.active_call else {
            return false;
        };
        let end_reason = match call.ctrl_rx.try_recv() {
            Ok(ControlMessage::CallEnded { reason, .. }) => reason,
            Ok(other) => {
                tracing::debug!(message = ?other, "ignoring control message during an active call");
                return false;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                // Agent B is gone; we can't keep a half-bridged call up.
                tracing::warn!(call_id = %call.call_id, "Agent B's control connection dropped mid-call");
                reason::TRANSPORT_ERROR.to_string()
            }
            Err(mpsc::TryRecvError::Empty) => return false,
        };

        let mut call = self.active_call.take().expect("just matched Some");
        // The telephone side hung up first (or reported its leg failed).
        // Attribute it before reporting; Agent B's own reason string still
        // drives the BYE for the finer detail.
        call.lifecycle.end(EndedBy::Pbx);
        report_answered_call_ended(p.obs, &call);
        hangup_carrier(session, inbound, call, &end_reason);
        // The call is over; any maintenance held for it may now run.
        self.maintenance.release();
        true
    }

    /// FR-011: end a call whose attachment genuinely died mid-call, stated as
    /// such rather than as a caller hangup. Cheap on a healthy call — the modem
    /// is only touched once the carrier leg has gone fully silent, and even then
    /// only to tell a dead attachment from a quiet caller.
    fn handle_attachment_loss(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        p: &DispatchParams,
    ) -> bool {
        let (Some(call), Some(check)) = (&self.active_call, p.attachment_check) else {
            return false;
        };
        if !self.watch.attachment_lost(call.meter.carrier_rx(), check) {
            return false;
        }
        let mut call = self.active_call.take().expect("just matched Some");
        tracing::warn!(
            call_id = %call.call_id,
            "ending call: the network attachment was lost mid-call \
             (not a caller hangup) — FR-011"
        );
        call.lifecycle.end(EndedBy::AttachmentLost);
        report_answered_call_ended(p.obs, &call);
        call::end_call_attachment_lost(session, call);
        self.maintenance.release();
        self.watch = AttachmentWatch::default();
        true
    }

    /// Advance an outbound origination that is mid-flight (specs/029): watch
    /// Agent B's control channel for a caller hangup, enforce the current
    /// deadline, and pick up Agent B's veth leg once the carrier answers.
    ///
    /// Deliberately does *not* short-circuit the iteration — the loop must fall
    /// through to `inbound.rx` so an inbound INVITE arriving during the attempt
    /// still gets its prompt `486` (FR-011), and carrier responses (which arrive
    /// on `inbound.rx`, no longer read here directly) still reach the response
    /// arm.
    fn advance_origination(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        p: &DispatchParams,
    ) {
        if self.origination.is_none() {
            return;
        }
        if let Some(call) = tick_pending_origination(&mut self.origination, session) {
            // The outbound call is now bridged — treat it like any other
            // active call (fresh media baseline so the last call's counts
            // can't read as a stall on this one).
            p.obs.set_active_calls(1);
            self.watch = AttachmentWatch::default();
            self.active_call = Some(call);
        } else if self.origination.is_none() {
            // Resolved as a failure/abandonment (not bridged, not still
            // pending): a renewal held for it may now run.
            self.maintenance.release();
        }
    }

    /// Outbound calling (specs/025-outbound-calling) — the same
    /// one-call-at-a-time rule `Admission::RejectBusy` already applies to a
    /// *carrier*-originated INVITE, for the other direction. A request arriving
    /// while the line is occupied gets an immediate `busy` `CallFailed` — never
    /// left queued in the channel for whenever the current call happens to end,
    /// which could be a long, silent wait from Agent B's side.
    /// `contains("busy")` is what `run_outbound_listener` (`vowifi/mod.rs`)
    /// checks to decide whether to try a different line rather than giving up
    /// outright.
    fn handle_place_call_request(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        p: &DispatchParams,
        place_call_rx: &mpsc::Receiver<PendingPlaceCall>,
    ) -> bool {
        let Ok(mut pending) = place_call_rx.try_recv() else {
            return false;
        };
        if self.busy() {
            tracing::debug!(
                call_id = %pending.call_id,
                "outbound: busy with another call, refusing"
            );
            let _ = write_msg(
                &mut pending.control,
                &ControlMessage::CallFailed {
                    call_id: pending.call_id,
                    reason: "busy".to_string(),
                },
            );
            return true;
        }
        // Ack receipt *before* touching the carrier transport, so Agent B can
        // tell "committed, now genuinely placing the call" apart from "busy"
        // and switch to a much longer wait for the real outcome
        // (`vowifi::mod`'s `CallAttempting` handling). Best-effort like
        // `fail()`'s writes: a dead connection here will fail again,
        // harmlessly, at the next write.
        let _ = write_msg(
            &mut pending.control,
            &ControlMessage::CallAttempting {
                call_id: pending.call_id.clone(),
            },
        );
        // Send the INVITE and record the attempt as in-flight, rather than
        // blocking here until it resolves (specs/029). The wait — for the
        // carrier's response, then for Agent B's veth leg — is driven by
        // `tick_pending_origination` and the response arm on the loop's own
        // thread, so an inbound INVITE, a caller hangup, and the Gm keepalive
        // all keep being serviced while the carrier is still ringing. Carrier
        // responses arrive on `inbound.rx` (via the client-reader thread) like
        // everything else — this no longer reads the carrier socket directly,
        // which also removes the two-readers-on-one-socket race (research R2).
        //
        // Watched (specs/039-at-stall-watchdog): this is the one part of an
        // origination that blocks the dispatch loop, and its INVITE write has no
        // timeout of its own — a P-CSCF whose receive window never opens parks
        // this thread indefinitely, exactly as the modem read did. The *waiting*
        // afterwards is tick-driven and needs no phase; `Phase::Origination`
        // therefore covers precisely this call and nothing else.
        self.origination = {
            let _phase = p.progress.phase_guard(watchdog::Phase::Origination);
            begin_origination(
                session,
                pending.control,
                pending.call_id,
                &pending.destination,
                &p.origination_setup(),
            )
        };
        true
    }

    fn handle_inbound_invite(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        inbound: &Inbound,
        p: &DispatchParams,
        req: &SipRequest,
        sink: &SipSink,
    ) {
        // An in-flight outbound origination occupies the line too (specs/029,
        // FR-011): consult whichever lifecycle exists so an inbound INVITE
        // during an attempt is refused `486` at once, through this same path,
        // rather than waiting out the attempt.
        let occupant = self
            .active_call
            .as_ref()
            .map(|c| &c.lifecycle)
            .or(self.origination.as_ref().map(|o| &o.lifecycle));
        if Admission::for_current(occupant) == Admission::RejectBusy {
            tracing::info!("declining inbound call: another VoWiFi call is already active");
            let _ = sink.send(&build_486_busy_here(req, &random_hex(4)));
            p.obs.report_call_not_answered(
                CallStatus::Failed,
                BridgeFailureReason::BridgeSetupFailed,
                &extract_caller(req),
                Utc::now(),
            );
            return;
        }
        // If the telephone-side half has no PBX registration, the outbound leg
        // cannot be placed — decline immediately with `480` rather than
        // dialling into the void and making the caller wait out a ~32s
        // transaction timeout. `480`, not `486`: the line is not busy, the
        // bridge is temporarily unavailable.
        if p.pbx_registered
            .is_some_and(|f| !f.load(std::sync::atomic::Ordering::SeqCst))
        {
            tracing::warn!(
                caller = %extract_caller(req),
                "declining inbound call: the PBX registration is down, so the \
                 outbound bridge leg cannot be placed"
            );
            let _ = sink.send(&build_uas_response(
                480,
                "Temporarily Unavailable",
                req,
                Some(&random_hex(4)),
                None,
                None,
            ));
            p.obs.report_call_not_answered(
                CallStatus::Failed,
                BridgeFailureReason::AgentUnreachable,
                &extract_caller(req),
                Utc::now(),
            );
            return;
        }

        match inbound::handle_invite(session, req, sink, inbound, &p.invite_ctx()) {
            Ok(call) => {
                if call.is_some() {
                    p.obs.set_active_calls(1);
                    // Fresh call, fresh media baseline (the meter starts at
                    // zero) — so a previous call's counts cannot read as a
                    // stall on this one.
                    self.watch = AttachmentWatch::default();
                }
                self.active_call = call;
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to handle inbound INVITE");
                // Tell the caller. Without this the carrier never gets a final
                // response, so the caller keeps hearing the ringback our
                // earlier `180` started and waits out the network's own timer —
                // a call that rings forever and never connects, with no
                // indication anything failed (FR-005, observed live:
                // specs/017 R17).
                //
                // `480 Temporarily Unavailable` rather than `486 Busy`: the
                // line is not busy, the bridge could not be built. Saying which
                // is the difference between a caller redialling now and one
                // redialling later.
                if let Err(send_err) = sink.send(&build_uas_response(
                    480,
                    "Temporarily Unavailable",
                    req,
                    Some(&random_hex(4)),
                    None,
                    None,
                )) {
                    tracing::warn!(
                        error = %send_err,
                        "could not tell the caller the bridge failed"
                    );
                }
                p.obs.report_call_not_answered(
                    CallStatus::Failed,
                    BridgeFailureReason::AgentUnreachable,
                    &extract_caller(req),
                    Utc::now(),
                );
            }
        }
    }

    fn handle_carrier_bye(&mut self, p: &DispatchParams, req: &SipRequest, sink: &SipSink) {
        match self.active_call.take() {
            Some(mut call) => {
                // The carrier's BYE is the caller hanging up.
                call.lifecycle.end(EndedBy::Caller);
                report_answered_call_ended(p.obs, &call);
                handle_bye(sink, req, call);
                self.maintenance.release();
            }
            None => {
                let _ = sink.send(&build_200_ok_bye(req, &random_hex(4)));
            }
        }
    }

    fn handle_carrier_response(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        p: &DispatchParams,
        resp: &SipResponse,
    ) {
        // Is this a response to an outbound INVITE we are placing (specs/029)?
        // These arrive here, on `inbound.rx`, instead of being read from the
        // carrier socket directly inside a blocking helper — matched by
        // `Call-ID`, which never collides with the Gm keepalive's `OPTIONS`
        // (correlated by `CSeq` below). A `200 OK` moves the attempt to
        // awaiting the veth leg; a failure clears it.
        if self
            .origination
            .as_ref()
            .is_some_and(|o| o.matches_response(resp))
        {
            if let OriginationStatus::Ended = self
                .origination
                .as_mut()
                .expect("just matched Some")
                .on_carrier_response(resp, session)
            {
                self.origination = None;
                // A renewal held for the attempt may now run.
                self.maintenance.release();
            }
            return;
        }
        // Is this the answer to our Gm keepalive? Any final response counts as
        // proof the client connection carries signaling — even a 4xx/5xx: the
        // question is liveness, not whether the carrier liked the request
        // (specs/028 R1). A matching answer is also what confirms a *reconnect*
        // actually worked, before we report the line healthy (R7).
        let matched = resp
            .header("CSeq")
            .and_then(parse_cseq_number)
            .is_some_and(|n| self.ping.on_response(n));
        if matched {
            if !self.gm_conn.is_up() {
                tracing::info!("Gm connection liveness confirmed; connection is up");
            }
            self.gm_conn = crate::ims::GmConnectionState::Up;
            self.reconnect_attempts = 0;
            p.obs.set_gm_connection_up(true);
            return;
        }
        // Outside a call the only requests we originate are reg-event
        // SUBSCRIBEs, so their outcome is worth surfacing rather than burying
        // at debug.
        tracing::info!(
            status = resp.status,
            reason = %resp.reason,
            "received response outside an active transaction"
        );
    }

    /// Idle wake-up: nothing arrived within the poll interval. Runs Gm
    /// liveness and, if it is due, the registration renewal.
    fn on_idle_tick(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        inbound: &mut Inbound,
        p: &DispatchParams,
    ) -> BridgeResult<()> {
        // Gm connection liveness (specs/028) runs here, on the idle path, only
        // when no call is in progress — a live call proves the connection by
        // itself and its own signaling must not be disturbed (FR-006). An
        // in-flight origination counts as a call in progress (specs/029): its
        // INVITE transaction is live on this transport, so a keepalive OPTIONS
        // or a reconnect must not cut across it. Repeated repair failure sets
        // `force_renewal`, which the renewal gate below honours.
        if !self.busy() {
            let _phase = p.progress.phase_guard(watchdog::Phase::GmProbe);
            probe_gm_connection(
                session,
                inbound,
                p.obs,
                &mut self.ping,
                &mut self.gm_conn,
                &mut self.reconnect_attempts,
                &mut self.force_renewal,
            );
        }

        // Never renew mid-call — that would tear down the transport a call's
        // own signaling (e.g. the eventual BYE) still needs; renewal is
        // deferred until the call ends.
        let (expires_at, registered_at) = {
            let guard = p.status.lock().unwrap_or_else(|e| e.into_inner());
            (guard.expires_at, guard.registered_at)
        };
        // The headroom scales to the lifetime the registrar actually granted
        // (FR-024). The granted value needs no separate field: it is exactly
        // the span the status already records. Without the scaling, a grant
        // shorter than twice the fixed headroom would make `renewal_due`
        // permanently true and re-register on every idle poll, once a second,
        // forever.
        let headroom = match (expires_at, registered_at) {
            (Some(exp), Some(reg)) => crate::ims::renewal_headroom_for(
                exp.duration_since(reg).unwrap_or(RENEWAL_HEADROOM),
                RENEWAL_HEADROOM,
            ),
            _ => RENEWAL_HEADROOM,
        };
        let due =
            expires_at.is_some_and(|e| crate::ims::renewal_due(SystemTime::now(), e, headroom));
        // `force_renewal` is the Gm-liveness escalation: re-register now even
        // though expiry is far off, because only a re-registration can
        // renegotiate a Gm SA that has gone dead underneath the connection
        // (R6). A scheduled renewal (`due`) proceeds as before.
        if !due && !self.force_renewal {
            return Ok(());
        }
        // Renewal is genuinely due. Hold it if a call is in progress, or an
        // outbound origination is still being placed (specs/029) —
        // re-registering mid-attempt would replace the transport and the
        // session the pending INVITE's dialog lives on. Recorded by the policy
        // so the deferral is visible in status, and so the model, not an inline
        // `is_some()`, owns the rule.
        if self.maintenance.decide(Maintenance::Renewal, self.busy()) == MaintenanceDecision::Defer
        {
            return Ok(());
        }
        // A previous attempt failed and its backoff hasn't elapsed yet —
        // `renewal_due` alone would otherwise fire again on every idle wake-up
        // regardless of backoff, hammering a still-failing renewal every poll
        // interval.
        if let Some(next_attempt) = self.next_renewal_attempt {
            if Instant::now() < next_attempt {
                return Ok(());
            }
        }
        // Everything from here to the end of this function is the renewal, and
        // all of it can block: acquiring the modem lock (which the SMS sweep
        // holds while it does its own AT work), the re-attach hook, the SIM
        // APDUs, the REGISTER round trips, the SA install. The phase is entered
        // *before* the lock acquisition deliberately — a sweep that wedges
        // while holding the lock would otherwise stall this loop outside any
        // budget, which is precisely the second route to the 2026-08-16 outage.
        let _phase = p.progress.phase_guard(watchdog::Phase::Renewal);
        p.status.lock().unwrap_or_else(|e| e.into_inner()).state =
            crate::ims::RegistrationState::Renewing;
        // Hold the modem lock across the whole renewal: the hook re-attaches
        // (drives the modem) and `attempt_renewal` re-reads the IMEI over the
        // AT port. Serialises with the cellular SMS reader that shares that
        // port (research R6); `None`, so a no-op, on the Wi-Fi path. Released
        // when this returns.
        //
        // Bounded (specs/039-at-stall-watchdog, FR-005): the SMS sweep holds
        // this lock every 20s while it does its own AT work, so an unbounded
        // wait here is how a wedged sweep used to take the registration down
        // with it. Failing the renewal instead lands on the existing backoff,
        // and the watchdog is still watching in case nothing recovers.
        let _modem_guard = match p.modem_lock {
            Some(l) => match l.lock() {
                Some(g) => Some(g),
                None => {
                    tracing::warn!(
                        retry_in_secs = self.backoff.as_secs(),
                        "cannot renew: another user of the modem has held it beyond the timeout"
                    );
                    let mut guard = p.status.lock().unwrap_or_else(|e| e.into_inner());
                    guard.state = crate::ims::RegistrationState::Failed;
                    guard.last_failure = Some((
                        SystemTime::now(),
                        "the modem was held by another user beyond the timeout".to_string(),
                    ));
                    drop(guard);
                    p.obs.set_registered(false);
                    self.next_renewal_attempt = Some(Instant::now() + self.backoff);
                    self.backoff = next_backoff(self.backoff, RETRY_MAX_BACKOFF);
                    return Ok(());
                }
            },
            None => None,
        };
        // Rebuild the layer underneath before spending a REGISTER on it.
        // Reaching here already means no call is in progress (the maintenance
        // policy deferred it above otherwise), which is precisely how
        // re-attachment inherits renewal's deferral instead of needing its own
        // — see `PreRenewalHook`.
        if let Some(hook) = p.pre_renewal {
            if let Err(reason) = hook() {
                tracing::warn!(
                    error = %reason,
                    retry_in_secs = self.backoff.as_secs(),
                    "cannot renew: the network attachment is down"
                );
                let mut guard = p.status.lock().unwrap_or_else(|e| e.into_inner());
                guard.state = crate::ims::RegistrationState::Failed;
                guard.last_failure = Some((SystemTime::now(), reason));
                // The re-attach hook is what just failed, so the attachment
                // underneath is down — health must say so.
                guard.attached = false;
                drop(guard);
                p.obs.set_registered(false);
                // If this renewal was the Gm-liveness escalation, its failure
                // means the connection is still down and the heavy remedy
                // didn't take — report Failed, but keep retrying on backoff
                // (FR-010b: Failed is not terminal).
                if self.force_renewal {
                    self.gm_conn = crate::ims::GmConnectionState::Failed {
                        since: ping::gm_episode_since(self.gm_conn),
                    };
                    p.obs.set_gm_connection_up(false);
                }
                self.next_renewal_attempt = Some(Instant::now() + self.backoff);
                self.backoff = next_backoff(self.backoff, RETRY_MAX_BACKOFF);
                return Ok(());
            }
        }
        match attempt_renewal(p.reg_cfg) {
            Ok(new_session) => {
                session.cleanup();
                *session = new_session;
                // A renewal negotiates a fresh Gm SA on fresh ports, so the old
                // listeners are now bound to dead ones.
                *inbound = start_inbound(session)?;
                // Re-read the granted lifetime on every renewal: a registrar is free to
                // grant a different one each time, and carrying the first
                // response's value forever would eventually mis-time renewal.
                let granted = session.granted_expires(crate::ims::DEFAULT_EXPIRES);
                let mut guard = p.status.lock().unwrap_or_else(|e| e.into_inner());
                guard.state = crate::ims::RegistrationState::Registered;
                guard.registered_at = Some(SystemTime::now());
                guard.expires_at = Some(SystemTime::now() + Duration::from_secs(granted as u64));
                // A renewal only reaches here through a successful re-attach
                // (the hook above), so the attachment is up.
                guard.attached = true;
                drop(guard);
                self.backoff = RETRY_INITIAL_BACKOFF;
                self.next_renewal_attempt = None;
                tracing::info!(granted_expires_secs = granted, "registration renewed");
                p.obs
                    .report_registration_attempt(RegistrationStatus::Success);
                p.obs.set_registered(true);
                p.obs.set_tunnel_up(true);
                p.obs.set_registration_expiry(
                    SystemTime::now() + Duration::from_secs(granted as u64),
                );
                // The renewal replaced `session` and `inbound` wholesale — a
                // fresh Gm SA, transport, and both readers. Any in-flight ping
                // referenced the old socket and can never be answered on the
                // new one, so it must be dropped (R11); the Gm connection is up
                // again by construction, and the failure episode (if any) is
                // over.
                self.ping.reset();
                self.reconnect_attempts = 0;
                self.force_renewal = false;
                self.gm_conn = crate::ims::GmConnectionState::Up;
                p.obs.set_gm_connection_up(true);
                subscribe_reg_event(session);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    retry_in_secs = self.backoff.as_secs(),
                    "registration renewal failed, retrying with backoff"
                );
                p.obs
                    .report_registration_attempt(map_registration_error(&e));
                p.obs.set_registered(false);
                p.obs.set_tunnel_up(false);
                let mut guard = p.status.lock().unwrap_or_else(|e| e.into_inner());
                guard.state = crate::ims::RegistrationState::Failed;
                guard.last_failure = Some((SystemTime::now(), e.to_string()));
                drop(guard);
                // A failed Gm-liveness escalation: still down, keep retrying on
                // backoff (FR-010b).
                if self.force_renewal {
                    self.gm_conn = crate::ims::GmConnectionState::Failed {
                        since: ping::gm_episode_since(self.gm_conn),
                    };
                    p.obs.set_gm_connection_up(false);
                }
                // Not a blocking sleep: the loop keeps dispatching inbound SIP
                // every iteration in the meantime (see `next_renewal_attempt`'s
                // doc comment).
                self.next_renewal_attempt = Some(Instant::now() + self.backoff);
                self.backoff = next_backoff(self.backoff, RETRY_MAX_BACKOFF);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // These moved to `ims::session` in the FR-019 extraction; the tests that
    // cover them stay here, exercising the same implementation.
    use crate::ims::session::{build_subscribe, SubscribeParts};

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
    fn build_subscribe_formats_a_reg_event_subscription() {
        let msg = build_subscribe(&SubscribeParts {
            impu: "sip:+919000000010@ims.mnc094.mcc404.3gppnetwork.org",
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
            .starts_with("SUBSCRIBE sip:+919000000010@ims.mnc094.mcc404.3gppnetwork.org SIP/2.0"));
        assert!(msg.contains("Route: <sip:pcscf.example:6000;lr>\r\n"));
        assert!(msg
            .contains("From: <sip:+919000000010@ims.mnc094.mcc404.3gppnetwork.org>;tag=tag1\r\n"));
        assert!(msg.contains("To: <sip:+919000000010@ims.mnc094.mcc404.3gppnetwork.org>\r\n"));
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
                    From: <sip:+919000000000@ims.mnc094.mcc404.3gppnetwork.org>;tag=abc\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert_eq!(extract_caller(&req), "+919000000000");
    }

    #[test]
    fn extract_caller_falls_back_to_unknown_when_from_is_unparseable() {
        let raw = "INVITE sip:x SIP/2.0\r\nFrom: garbage\r\nCall-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert_eq!(extract_caller(&req), "unknown");
    }

    fn message_with_headers(headers: &str) -> SipRequest {
        let raw = format!(
            "MESSAGE sip:x SIP/2.0\r\n{headers}Call-ID: c\r\nCSeq: 1 MESSAGE\r\n\
             Content-Length: 0\r\n\r\n"
        );
        SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
    }

    /// The delivery report is addressed to the IP-SM-GW named in the
    /// delivered message's `P-Asserted-Identity` (TS 24.341 §5.3.2.4 NOTE 1),
    /// so the *whole* URI has to survive — parameters included, since the
    /// carrier form measured on Jio carries `;transport=udp` inside the
    /// brackets, where it belongs to the URI rather than to the header.
    #[test]
    fn header_uri_keeps_the_whole_uri_from_a_bracketed_header() {
        let req = message_with_headers(
            "P-Asserted-Identity: <sip:A2P@203.0.113.7;transport=udp>\r\n\
             From: <sip:gateway.ims.example>;tag=abc\r\n",
        );
        assert_eq!(
            header_uri(&req, "P-Asserted-Identity").as_deref(),
            Some("sip:A2P@203.0.113.7;transport=udp")
        );
        // The `;tag=abc` here is a *header* parameter, outside the brackets,
        // and must not end up in a Request-URI.
        assert_eq!(
            header_uri(&req, "From").as_deref(),
            Some("sip:gateway.ims.example")
        );
    }

    /// Unbracketed (`addr-spec`) form: with no brackets to mark the end of the
    /// URI, a `;` is the URI's own parameter separator and must be kept.
    #[test]
    fn header_uri_keeps_parameters_of_an_unbracketed_uri() {
        let req = message_with_headers("P-Asserted-Identity: sip:ipsmgw.example;lr\r\n");
        assert_eq!(
            header_uri(&req, "P-Asserted-Identity").as_deref(),
            Some("sip:ipsmgw.example;lr")
        );
    }

    /// Nothing to address a report to: a missing header, or one carrying only
    /// a display name. `acknowledge` logs and skips rather than sending a
    /// request to a URI it invented.
    #[test]
    fn header_uri_is_none_without_a_uri() {
        let req = message_with_headers("P-Asserted-Identity: \"Anonymous\"\r\n");
        assert_eq!(header_uri(&req, "P-Asserted-Identity"), None);
        assert_eq!(header_uri(&req, "X-Absent"), None);
    }

    /// A minimal RP-DATA + SMS-DELIVER TPDU for "Hi" from +919000000001 — same
    /// construction as `sms_pdu`'s own tests, kept independent so this test
    /// exercises the header-gated call site, not just the decoder itself.
    fn a_3gpp_sms_pdu() -> Vec<u8> {
        let sender = "919000000001";
        let bcd: Vec<u8> = sender
            .as_bytes()
            .chunks(2)
            .map(|p| match p {
                [a, b] => (b - b'0') << 4 | (a - b'0'),
                [a] => 0xF0 | (a - b'0'),
                _ => unreachable!(),
            })
            .collect();
        // "Hi" in the GSM 7-bit default alphabet happens to equal its ASCII
        // codepoints, so no separate encode table is needed here.
        let septets = [b'H' & 0x7F, b'i' & 0x7F];
        let mut packed = 0u16;
        packed |= u16::from(septets[0]);
        packed |= u16::from(septets[1]) << 7;
        let packed_bytes = packed.to_le_bytes();

        let mut tpdu = vec![0x00, sender.len() as u8, 0x91];
        tpdu.extend_from_slice(&bcd);
        tpdu.extend_from_slice(&[0x00, 0x00]); // TP-PID, TP-DCS (GSM7)
        tpdu.extend_from_slice(&[0u8; 7]); // TP-SCTS
        tpdu.push(2); // TP-UDL: 2 septets
        tpdu.extend_from_slice(&packed_bytes);

        let mut rp = vec![0x01, 0x00, 0, 0];
        rp.push(tpdu.len() as u8);
        rp.extend_from_slice(&tpdu);
        rp
    }

    /// The header-gated path `handle_message` actually depends on: a
    /// `MESSAGE` naming the 3GPP content type gets its sender and body
    /// replaced with what the TPDU says, not the SIP `From` (a network
    /// element's own hostname on a real carrier) or the raw undecoded bytes.
    #[test]
    fn decode_pdu_body_decodes_a_3gpp_sms_message() {
        let pdu = a_3gpp_sms_pdu();
        let mut raw = b"MESSAGE sip:x SIP/2.0\r\n\
             From: <sip:gateway.ims.example>\r\n\
             Call-ID: c\r\nCSeq: 1 MESSAGE\r\n\
             Content-Type: application/vnd.3gpp.sms\r\n"
            .to_vec();
        raw.extend_from_slice(format!("Content-Length: {}\r\n\r\n", pdu.len()).as_bytes());
        raw.extend_from_slice(&pdu);

        let (req, _) = SipRequest::try_parse(&raw).unwrap().unwrap();
        let decoded = decode_pdu_body(&req).expect("a 3GPP SMS body must decode");
        assert_eq!(decoded.sender, "+919000000001");
        assert_eq!(decoded.text, "Hi");
    }

    /// An ordinary text `MESSAGE` (no 3GPP content type) must be left alone —
    /// this is the shape `sample_message()` elsewhere in this crate uses, and
    /// it must keep working exactly as before.
    #[test]
    fn decode_pdu_body_ignores_a_plain_text_message() {
        let raw = "MESSAGE sip:x SIP/2.0\r\n\
                    Call-ID: c\r\nCSeq: 1 MESSAGE\r\n\
                    Content-Type: text/plain\r\n\
                    Content-Length: 5\r\n\r\nhello";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert!(decode_pdu_body(&req).is_none());
    }

    // ---- modem SMS sweep spawn decision (specs/038-reliable-sms-delivery) ---

    #[test]
    fn wants_modem_sms_reader_for_a_real_modem_line() {
        assert!(wants_modem_sms_reader(false));
    }

    #[test]
    fn does_not_want_modem_sms_reader_for_a_pcsc_reader_line() {
        // A pcsc_reader line's SIM sits in a PC/SC reader with no modem/cellular
        // attach at all — there is no legacy bearer to poll and no modem_port
        // to open.
        assert!(!wants_modem_sms_reader(true));
    }

    // ---- what this UAS serves, and what it refuses -------------------------

    fn request(method: &str) -> SipRequest {
        let raw = format!(
            "{method} sip:me@10.0.0.9:5060 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.1.1.1:5067;branch=z9hG4bKone\r\n\
             From: <sip:caller@example.net>;tag=abc\r\n\
             To: <sip:me@example.net>\r\n\
             Call-ID: c1\r\nCSeq: 3 {method}\r\nContent-Length: 0\r\n\r\n"
        );
        SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
    }

    /// The `Allow` we state and the methods the dispatch loop actually serves
    /// are the same list, and it is the one place either is written down.
    #[test]
    fn allow_lists_exactly_what_the_dispatch_loop_serves() {
        let served: Vec<&str> = ALLOW.split(", ").collect();
        assert_eq!(
            served,
            vec!["INVITE", "ACK", "CANCEL", "BYE", "OPTIONS", "MESSAGE", "NOTIFY"],
            "changing this means changing `dispatch_loop`'s arms to match"
        );
    }

    /// A P-CSCF's keepalive must be answered, and answered with the same
    /// capability list a session response carries — silence reads as a UE that
    /// is no longer there.
    #[test]
    fn an_options_keepalive_is_answered_with_our_capabilities() {
        let resp = options_response(&request("OPTIONS"), "totag1");
        assert!(resp.starts_with("SIP/2.0 200 OK\r\n"), "{resp}");
        assert!(resp.contains(&format!("\r\nAllow: {ALLOW}\r\n")), "{resp}");
        assert!(resp.contains("\r\nCSeq: 3 OPTIONS\r\n"), "{resp}");
    }

    /// A method we do not serve gets a refusal that names what we do serve
    /// (RFC 3261 §21.4.6), not silence for the network to time out on.
    #[test]
    fn a_method_we_do_not_serve_is_refused_with_the_list_that_we_do() {
        for method in ["UPDATE", "PRACK", "INFO", "REFER", "SUBSCRIBE"] {
            let resp = unserved_method_response(&request(method), "totag1");
            assert!(
                resp.starts_with("SIP/2.0 405 Method Not Allowed\r\n"),
                "{method}: {resp}"
            );
            assert!(
                resp.contains(&format!("\r\nAllow: {ALLOW}\r\n")),
                "a 405 must state what is allowed — {method}: {resp}"
            );
        }
    }

    /// A `CANCEL` that reaches the dispatch loop is one whose INVITE is no
    /// longer being rung — the transaction is gone, not the method unknown
    /// (RFC 3261 §9.2). Answering `405` here would say we cannot cancel calls.
    #[test]
    fn a_stray_cancel_is_answered_481_not_405() {
        let resp = unserved_method_response(&request("CANCEL"), "totag1");
        assert!(
            resp.starts_with("SIP/2.0 481 Call/Transaction Does Not Exist\r\n"),
            "{resp}"
        );
    }
}
