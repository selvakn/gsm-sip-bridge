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
    attempt_renewal, extract_caller, header_uri, map_registration_error,
    map_registration_status_code, next_backoff, respond, send_sms_delivery_report, start_inbound,
    subscribe_reg_event, to_unix, Inbound,
};
use crate::ims::sip_client::{
    build_200_ok_message, build_415_unsupported_media, build_486_busy_here,
    build_488_not_acceptable, build_uas_response, build_uas_response_with_headers, format_sip_addr,
    random_hex, SipMessage, SipRequest, SipResponse, SipSink,
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

#[cfg(test)]
use call::test_active_call;
use call::{
    classify_in_dialog_invite, handle_bye, hangup_carrier, report_answered_call_ended, ActiveCall,
    AttachmentWatch, InDialogInvite,
};
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
    // specs/047-offerless-invite-sms-reassembly (SMS-05): shared with
    // `handle_message` (via `InboundParams` below) the same way `dedupe`
    // already is — `admit_part` there is what actually populates this.
    // Expiry is flushed from `LoopState::on_idle_tick`, not the modem sweep
    // below — that thread only exists when `wants_modem_sms_reader` is true
    // (never on a `pcsc_reader` line), while every line runs the idle tick
    // (code review finding, 2026-08-28: the original design assumed the
    // sweep "runs unconditionally, once per line," which is false for that
    // line type).
    let reassembly = Arc::new(Mutex::new(crate::volte::sms::Reassembly::default()));
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
        reassembly,
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
///
/// Re-exported from [`crate::ims::UAS_ALLOW`] rather than defined here: the
/// REGISTER built in `sip_client::build_register` used to state a longer,
/// separately-hardcoded list (`..., UPDATE, PRACK, INFO, ..., REFER`) than
/// this one, so the network's picture of what we serve and what we actually
/// serve could disagree before either side ever sent a request (specs/041
/// conformance review, MT-10). One constant now backs both.
const ALLOW: &str = crate::ims::UAS_ALLOW;

/// Option-tags this UAS implements enough of to honour if a peer `Require`s
/// them (RFC 3261 §8.2.2.3) and to state in a `Supported` header.
/// `timer`, `100rel`, `replaces`, `path` and `gruu` stay absent: they were
/// previously claimed in `inbound::UAS_EXTRA_HEADERS`'s `Supported` line
/// with no behaviour behind any of them — `timer` with no session-refresh
/// timer, `100rel` with no UAS-side reliable-provisional handling,
/// `replaces`/`gruu` with none of their machinery at all, and `path`
/// naming a REGISTER mechanism this is not even a REGISTER response
/// (specs/041 conformance review, MT-10). `precondition` is real, if
/// bounded: this UAS honours RFC 3312 QoS preconditions on its own
/// segment (no real reservation delay to wait on) but still declines what
/// it cannot honestly confirm without a synchronization mechanism it
/// doesn't implement (specs/048 MT-06) — advertising it is no longer a
/// promise this bridge can't keep, the same bar MT-10 set for every other
/// tag here. One list feeds both [`unsupported_required_extensions`] and
/// whatever `Supported` header a caller gets — grown only by growing the
/// UAS, the same rule [`ALLOW`] already states for methods.
const SUPPORTED_EXTENSIONS: &[&str] = &["precondition"];

/// Every option-tag a request's `Require` demands that is not in
/// [`SUPPORTED_EXTENSIONS`] — RFC 3261 §8.2.2.3: a UAS that does not support
/// one must refuse the whole request `420 Bad Extension`, listing exactly
/// which in `Unsupported`, rather than silently proceeding as if it had. Reads
/// every `Require` header line, not just the first — RFC 3261 doesn't forbid
/// more than one, the same reason `SipRequest::headers_all` exists for `Via`.
fn unsupported_required_extensions(req: &SipRequest) -> Vec<String> {
    req.headers_all("Require")
        .iter()
        .flat_map(|v| v.split(','))
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty() && !SUPPORTED_EXTENSIONS.contains(&tag.as_str()))
        .collect()
}

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
/// Whether `req` names the one call we have up, by Call-ID alone. Good
/// enough for diagnostics that take no action either way (`log_ack`) and
/// for `CANCEL` (which, per RFC 3261 §9.1, mirrors the original INVITE's
/// still-untagged `To` and so never carries a tag to check) — but NOT for
/// anything that acts on the match, where a colliding or malformed Call-ID
/// could satisfy it without the request actually belonging to this dialog.
/// See `matches_caller_tag`/`names_active_dialog` for those (PR review,
/// 2026-08-26).
fn names_active_call(req: &SipRequest, active_call_id: Option<&str>) -> bool {
    active_call_id.is_some_and(|id| req.header("Call-ID") == Some(id))
}

/// The `tag=` parameter of a header value (RFC 3261 §19.3) — e.g. `"abc"`
/// from `<sip:x@y>;tag=abc`. `None` if the header is absent or carries none.
fn header_tag(value: &str) -> Option<&str> {
    value.split(';').skip(1).find_map(|p| {
        let p = p.trim_start();
        let (name, val) = p.split_once('=')?;
        // RFC 3261 §7.3.1: parameter names are case-insensitive (unlike the
        // tag *value* itself, an opaque token compared byte-for-byte, so
        // this stops at the `=` and leaves `val` untouched).
        name.eq_ignore_ascii_case("tag").then_some(val)
    })
}

/// Whether `req`'s `From` tag matches the caller's own tag, given the
/// verbatim `From` header they sent on the INVITE that started the dialog
/// (`ActiveCall::dialog`'s `to` field — see its doc comment). This is the
/// half of RFC 3261 §12.2.2's dialog identity present on *every* request in
/// the dialog, including the very first — still untagged-`To` — INVITE and
/// any exact retransmission of it, unlike our own tag, which only exists
/// once we've answered.
fn matches_caller_tag(req: &SipRequest, caller_from: &str) -> bool {
    req.header("From").and_then(header_tag) == header_tag(caller_from)
}

/// Whether `req` names the exact dialog identified by `call_id`/`our_to_tag`/
/// `caller_from`, using the full RFC 3261 §12.2.2 identity — Call-ID plus
/// *both* tags — rather than Call-ID alone, which a colliding or malformed
/// Call-ID could satisfy without `req` actually belonging to this dialog
/// (PR review, 2026-08-26: a `BYE` reusing the active call's Call-ID with
/// different tags would otherwise still end it). Only valid once the dialog
/// carries our tag too (i.e. after we've answered) — everything that calls
/// this (`BYE`) only makes sense post-answer anyway; see `matches_caller_tag`
/// for the pre-answer retransmission case, which never has our tag to check.
fn names_active_dialog(
    req: &SipRequest,
    call_id: &str,
    our_to_tag: &str,
    caller_from: &str,
) -> bool {
    req.header("Call-ID") == Some(call_id)
        && req.header("To").and_then(header_tag) == Some(our_to_tag)
        && matches_caller_tag(req, caller_from)
}

/// A `CANCEL` naming the one call we have up gets `200 OK` on that call's
/// own `To` tag (RFC 3261 §9.2); anything else falls to
/// `unserved_method_response`'s general "no such transaction" `481`.
fn cancel_response(
    req: &SipRequest,
    active: Option<(&str, &str)>,
    fallback_to_tag: &str,
) -> String {
    match active {
        Some((call_id, to_tag)) if Some(call_id) == req.header("Call-ID") => {
            build_uas_response(200, "OK", req, Some(to_tag), None, None)
        }
        _ => unserved_method_response(req, fallback_to_tag),
    }
}

/// `None` when `req` names the one call we have up (the caller goes on to
/// actually end it); `Some` with the `481` to send when it does not — a
/// request naming a dialog we don't recognise (or arriving with no active
/// call at all) is refused, not silently treated as ending whatever call
/// happens to be active (RFC 3261 §12.2.2; specs/042-dialog-transaction-identity, MT-08).
fn bye_response_if_unmatched(req: &SipRequest, active_call: Option<&ActiveCall>) -> Option<String> {
    let matched = active_call
        .is_some_and(|call| names_active_dialog(req, &call.call_id, &call.to_tag, &call.dialog.to));
    if matched {
        None
    } else {
        Some(build_uas_response(
            481,
            "Call/Transaction Does Not Exist",
            req,
            Some(&random_hex(4)),
            None,
            None,
        ))
    }
}

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
    /// specs/047-offerless-invite-sms-reassembly (SMS-05): buffers a
    /// multi-part message's parts until every part has arrived or it times
    /// out. Shared the same way `dedupe` is, for the same reason — see
    /// `Reassembly`'s own docs.
    pub reassembly: Arc<Mutex<crate::volte::sms::Reassembly>>,
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
        reassembly,
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
    subscribe_reg_event(&mut session, &reg_cfg.access_network_info);

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
            reassembly: &reassembly,
            // Read from `[vowifi]` on both paths deliberately: SMS-over-IP is
            // the same TS 24.341 procedure over LTE as over Wi-Fi, so one
            // switch governs both rather than two that could disagree.
            sms_delivery_report: app_config.vowifi.sms_delivery_report,
            // Read from `[vowifi]` on both paths for the same reason as
            // `sms_delivery_report` above: one switch, not two that disagree.
            originating_headers: app_config.vowifi.originating_headers,
            respect_caller_privacy: app_config.vowifi.respect_caller_privacy,
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
///
/// Returns the envelope's actual [`DecodedRp`] rather than unwrapping straight
/// to a message — an RP-ACK or RP-ERROR is not a decode failure (the bytes
/// parse fine), but it is not a deliverable short message either, and the
/// caller must not treat it as one.
/// The exhaustive set of `Content-Type`s this UAS can interpret on an
/// inbound `MESSAGE`: the 3GPP SMS-over-IP TPDU wrapper, and plain text.
const SUPPORTED_MESSAGE_CONTENT_TYPES: &[&str] = &["application/vnd.3gpp.sms", "text/plain"];

/// Whether this UAS can interpret an inbound `MESSAGE`'s body: no
/// `Content-Type` at all (the long-standing shape for a plain-text SMS
/// gateway — unchanged, still treated as text), or one of
/// [`SUPPORTED_MESSAGE_CONTENT_TYPES`] compared with any `;`-delimited
/// parameters ignored (RFC 3261 §7.2.1: the `;` starts the parameter list,
/// not a different media type). RFC 3428 §7 / RFC 3261 §21.4.13: a UAS that
/// cannot render a body must refuse it `415 Unsupported Media Type`, not
/// accept it and forward whatever a lossy text conversion of, say, a JPEG
/// produces (specs/041 conformance review, SMS-06).
fn message_content_type_supported(req: &SipRequest) -> bool {
    match req.header("Content-Type") {
        None => true,
        Some(ct) => {
            let media_type = ct.split(';').next().unwrap_or("").trim();
            SUPPORTED_MESSAGE_CONTENT_TYPES
                .iter()
                .any(|t| t.eq_ignore_ascii_case(media_type))
        }
    }
}

fn decode_pdu_body(req: &SipRequest) -> Option<crate::ims::sms_pdu::DecodedRp> {
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
    if !message_content_type_supported(req) {
        let content_type = req.header("Content-Type").unwrap_or("(none)").to_string();
        tracing::info!(
            content_type = %content_type,
            "declining a MESSAGE body we cannot interpret"
        );
        respond(
            sink,
            "415 (MESSAGE)",
            &build_415_unsupported_media(
                req,
                &random_hex(4),
                &SUPPORTED_MESSAGE_CONTENT_TYPES.join(", "),
            ),
        );
        return;
    }

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
    //
    // An RP-ACK or RP-ERROR is not a decode failure and not a message either
    // — see `sms_pdu::DecodedRp`'s docs. There is nothing to relay or record
    // for either: this bridge never submits an RP-DATA over IMS itself, so a
    // well-behaved peer never sends them, and treating one as a message would
    // put a "message" nobody sent in front of the operator (specs/041's
    // sibling review, SMS-01). Acknowledge the SIP transaction and stop.
    let decoded = match decode_pdu_body(req) {
        Some(crate::ims::sms_pdu::DecodedRp::Message(sms)) => Some(sms),
        Some(crate::ims::sms_pdu::DecodedRp::Ack { rp_mr }) => {
            tracing::info!(
                sender = %sender,
                rp_mr,
                "received an RP-ACK on the SMS transport; not a deliverable message"
            );
            respond(
                sink,
                "200 OK (MESSAGE, RP-ACK)",
                &build_200_ok_message(req, &random_hex(4)),
            );
            return;
        }
        Some(crate::ims::sms_pdu::DecodedRp::Error { rp_mr, cause }) => {
            tracing::warn!(
                sender = %sender,
                rp_mr,
                cause = ?cause,
                "received an RP-ERROR on the SMS transport; not a deliverable message"
            );
            respond(
                sink,
                "200 OK (MESSAGE, RP-ERROR)",
                &build_200_ok_message(req, &random_hex(4)),
            );
            return;
        }
        // specs/045 SMS-02: the RP-DATA was fine, but its TPDU isn't
        // SMS-DELIVER (an SMS-SUBMIT-REPORT or SMS-STATUS-REPORT) —
        // recognized, not garbled, and still owed an RP-ACK (the RP-DATA
        // itself was received), never relayed as text.
        Some(crate::ims::sms_pdu::DecodedRp::UnsupportedTpdu { rp_mr, kind }) => {
            tracing::info!(
                sender = %sender,
                rp_mr,
                kind = ?kind,
                "received a non-SMS-DELIVER TPDU on the SMS transport; not a deliverable message"
            );
            respond(
                sink,
                "200 OK (MESSAGE, non-deliver TPDU)",
                &build_200_ok_message(req, &random_hex(4)),
            );
            // The RP-DATA envelope itself was still received (just not a
            // deliverable message) — it still owes the network an RP-ACK,
            // the same as a genuinely decoded message would (PR review,
            // 2026-08-27). Without this the network never sees the RP layer
            // acknowledged and retains or retries the RP-DATA.
            if let Some(ipsmgw) =
                header_uri(req, "P-Asserted-Identity").or_else(|| header_uri(req, "From"))
            {
                send_sms_delivery_report(
                    session,
                    &ipsmgw,
                    &crate::ims::sms_pdu::build_rp_ack(rp_mr),
                );
            } else {
                tracing::warn!(
                    "no P-Asserted-Identity or From URI on an SMS MESSAGE; cannot address an RP-ACK"
                );
            }
            return;
        }
        // specs/045 SMS-03: the TPDU claimed to be SMS-DELIVER but couldn't
        // be decoded — a genuine failure, so an RP-ERROR is owed instead of
        // an RP-ACK, and req.body must never be relayed as if it were text.
        Some(crate::ims::sms_pdu::DecodedRp::Undecodable { rp_mr }) => {
            tracing::warn!(
                sender = %sender,
                rp_mr,
                "could not decode a TPDU claiming to be SMS-DELIVER; reporting RP-ERROR"
            );
            respond(
                sink,
                "200 OK (MESSAGE, undecodable TPDU)",
                &build_200_ok_message(req, &random_hex(4)),
            );
            if let Some(ipsmgw) =
                header_uri(req, "P-Asserted-Identity").or_else(|| header_uri(req, "From"))
            {
                send_sms_delivery_report(
                    session,
                    &ipsmgw,
                    &crate::ims::sms_pdu::build_rp_error(rp_mr, None),
                );
            } else {
                tracing::warn!(
                    "no P-Asserted-Identity or From URI on an SMS MESSAGE; cannot address an RP-ERROR"
                );
            }
            return;
        }
        None => None,
    };
    if let Some(decoded) = &decoded {
        sender = decoded.sender.clone();
        body = match decoded.part {
            Some(part) => format!("[{}/{}] {}", part.sequence, part.total, decoded.text),
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

    // specs/047-offerless-invite-sms-reassembly (SMS-05): a multi-part
    // message's individual part is buffered here rather than relayed
    // immediately, and only forwarded — combined, in order — once every
    // part has arrived. `decoded.part` is `None` for an ordinary
    // single-part message (or a plain-text MESSAGE with no RP layer at
    // all), which falls straight through to the pre-existing send below,
    // unchanged (SC-005/FR-015). `key` above was computed from this part's
    // own labelled `body` before this block runs, so `Dedupe` keeps
    // deduplicating at the per-part level regardless of what reassembly
    // does with it (data-model.md's explicit non-goal note).
    let mut completed_part: Option<crate::ims::sms_pdu::ConcatPart> = None;
    if let Some(part) = decoded.as_ref().and_then(|d| d.part) {
        let text = decoded.as_ref().map(|d| d.text.clone()).unwrap_or_default();
        // If admitting this part forces an *other*, unrelated buffer out
        // (capacity eviction, or a detected reference reuse —
        // `Reassembly::admit_part`'s own docs), that buffer's content is
        // queued inside `Reassembly` itself for retried delivery — see
        // `flush_expired_reassembly`/`LoopState::on_idle_tick`, which
        // drains the same queue this could have just added to. Nothing to
        // do with it here.
        let outcome = p
            .reassembly
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .admit_part(&sender, &part, &text);
        match outcome {
            crate::volte::sms::PartOutcome::Pending => {
                // Still missing at least one part. FR-012: acknowledge this
                // part to the network now, exactly as promptly as a
                // single-part message is today — only forwarding waits.
                // Deliberately **not** `Dedupe::confirm`ed here (code
                // review finding, 2026-08-28): this part has not been
                // durably delivered anywhere yet, only buffered in this
                // process's memory — confirming it would tell the
                // modem-storage route it's safe to discard its own backup
                // copy of a part that could still be lost (a crash, or a
                // later failed expiry-flush). `confirm` happens only once
                // an actual delivery succeeds — see `Complete`'s branch
                // below and `deliver_flushed_part`.
                tracing::info!(
                    sender = %sender,
                    sequence = part.sequence,
                    total = part.total,
                    "buffered one part of a multi-part message; not yet complete"
                );
                acknowledge(session, sink, req, decoded.as_ref(), p.sms_delivery_report);
                return;
            }
            crate::volte::sms::PartOutcome::Complete(joined) => {
                body = joined;
                completed_part = Some(part);
            }
            crate::volte::sms::PartOutcome::Malformed => {
                // Fall back to today's existing per-part labelled delivery
                // (FR-016) — `body` already holds the `[seq/total] text`
                // shape computed above.
            }
        }
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
        if let Some(part) = completed_part {
            // Only now — mirrors `Dedupe::confirm` immediately above, and
            // for the identical reason: nothing may treat this multi-part
            // message as safe to discard until its actual delivery
            // succeeded, not merely completed reassembly.
            p.reassembly
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mark_delivered(&sender, &part);
        }
        acknowledge(session, sink, req, decoded.as_ref(), p.sms_delivery_report);
    } else {
        // Release the admission above so the retransmission this triggers is
        // treated as fresh, not as a duplicate of a delivery that never
        // happened.
        p.dedupe
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .forget(&key);
        // `completed_part`'s `Reassembly` entry is deliberately left in
        // place on this failure path (no `mark_delivered` call) — see that
        // method's own docs: the network's retransmission of just the
        // triggering part (the one `Dedupe::forget` above just
        // un-suppressed) will re-admit into a buffer that still holds every
        // other part, and correctly reach `Complete` again without needing
        // the whole message resent.
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

/// Attempts one already-acknowledged, individually-labelled delivery for a
/// part sitting in `p.reassembly`'s retry queue (`Reassembly::
/// ready_for_delivery`'s shape) — whether it's there from capacity
/// eviction, a detected reference reuse, or ordinary expiry, all queued the
/// same way. Returns whether it actually succeeded, so the caller
/// (`flush_expired_reassembly`) knows whether to dequeue it or leave it for
/// the next retry — this attempt is deliberately **not** the only one: a
/// transient control-channel failure here used to lose the content outright
/// (code review finding, 2026-08-28); now it just stays queued and tries
/// again next `on_idle_tick` (~1s later).
///
/// Also settles that part's own `Dedupe` claim on success: `confirm`,
/// mirroring the ordinary single-part success path, so the modem-storage
/// route's cross-route coordination (`wait_for_resolution`) can finally
/// treat it as safe to discard its own backup — which, before this part
/// was flushed, it correctly could not (a separate, earlier code-review
/// finding, 2026-08-28: see [`crate::volte::sms::PartOutcome::Pending`]'s
/// docs). No `Dedupe` action on failure — the claim stays exactly as it
/// was, since this attempt changed nothing durable; the next retry decides
/// its fate, not this one. The label is reconstructed identically to how
/// it was first computed, so the dedupe key matches.
fn deliver_flushed_part(
    p: &DispatchParams,
    sender: &str,
    sequence: u8,
    total: u8,
    text: &str,
) -> bool {
    let body = format!("[{sequence}/{total}] {text}");
    let key = crate::volte::sms::InboundMessage {
        route: crate::volte::sms::MessageRoute::OverRegistration,
        sender: sender.to_string(),
        body: body.clone(),
        modem_index: None,
    }
    .dedupe_key();
    let msg = ControlMessage::SmsReceived {
        sender: sender.to_string(),
        body,
        received_at: chrono::Utc::now().to_rfc3339(),
    };
    match TcpStream::connect_timeout(&p.control_addr, CONTROL_TIMEOUT) {
        Ok(mut control) => match write_msg(&mut control, &msg) {
            Ok(()) => {
                p.dedupe
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .confirm(&key);
                tracing::info!(
                    sender = %sender,
                    sequence,
                    total,
                    "delivered an already-buffered multi-part message's held part individually"
                );
                true
            }
            Err(e) => {
                tracing::warn!(
                    sender = %sender,
                    sequence,
                    total,
                    error = %e,
                    "failed to deliver a buffered multi-part message's held part; still queued, \
                     will retry next idle tick"
                );
                false
            }
        },
        Err(e) => {
            tracing::warn!(
                sender = %sender,
                sequence,
                total,
                error = %e,
                "failed to reach the control channel to deliver a buffered multi-part message's \
                 held part; still queued, will retry next idle tick"
            );
            false
        }
    }
}

/// Advances `p.reassembly`'s retry queue by one `LoopState::on_idle_tick`
/// step: moves anything newly past [`REASSEMBLY_TIMEOUT`] into the queue
/// (FR-013/SC-004), then attempts delivery for everything currently queued
/// — new and previously-retried alike — dequeuing only what actually
/// succeeds via [`deliver_flushed_part`].
///
/// Called from `LoopState::on_idle_tick`, which — unlike the modem-storage
/// sweep thread this was originally (and wrongly) placed in — runs for
/// every line regardless of whether it has a modem to sweep (code review
/// finding, 2026-08-28).
fn flush_expired_reassembly(p: &DispatchParams) {
    p.reassembly
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .expire_due(std::time::Instant::now());
    let ready = p
        .reassembly
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .ready_for_delivery();
    for (id, sender, sequence, total, text) in ready {
        if deliver_flushed_part(p, &sender, sequence, total, &text) {
            p.reassembly
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mark_flush_delivered(id);
        }
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
    /// specs/047-offerless-invite-sms-reassembly (SMS-05) — see
    /// `InboundParams::reassembly`.
    reassembly: &'a Arc<Mutex<crate::volte::sms::Reassembly>>,
    /// See `config::VowifiConfig::sms_delivery_report`; consumed by
    /// [`acknowledge`].
    sms_delivery_report: bool,
    /// See `config::VowifiConfig::originating_headers`; consumed by
    /// [`origination::begin_origination`].
    originating_headers: crate::config::OriginatingHeaders,
    /// See `config::VowifiConfig::respect_caller_privacy`; consumed by
    /// [`inbound::caller_name_for_onward_signaling`].
    respect_caller_privacy: bool,
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
            access_network_info: &self.reg_cfg.access_network_info,
            respect_caller_privacy: self.respect_caller_privacy,
        }
    }

    fn origination_setup(&self) -> origination::OriginationSetup {
        origination::OriginationSetup {
            veth_local_ip: self.veth_local_ip,
            veth_sip_port: self.veth_sip_port,
            wideband: self.wideband,
            originating_headers: self.originating_headers,
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
            Ok((SipMessage::Request(req), sink)) if req.method == "CANCEL" => {
                st.handle_carrier_cancel(&req, &sink);
            }
            Ok((SipMessage::Request(req), _)) if req.method == "ACK" => {
                st.log_ack(&req);
            }
            Ok((SipMessage::Request(req), sink)) if req.method == "NOTIFY" => {
                st.handle_reg_notify(session, p, &req, &sink);
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
        // Signal every relay/RTCP thread to stop *before* reading the
        // figures they publish — `hangup_carrier` below sets this too, but
        // only after the report already ran, which meant every call's
        // "final" quality snapshot was taken while those threads were
        // still live and could still be updating it (Greptile review,
        // PR #66). Idempotent: setting it twice is harmless.
        call.stop.store(true, Ordering::Relaxed);
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
        // See the matching comment in `handle_pbx_hangup` — stop the media/
        // RTCP threads before reading what they published, not after.
        call.stop.store(true, Ordering::Relaxed);
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
        // An INVITE naming the call already active on this line is never a
        // brand-new second call — it's either a retransmission of the answer
        // we already gave, or a genuine re-INVITE (specs/042-dialog-transaction-identity,
        // MT-01/MT-02). Either way it must not fall into the busy check below.
        //
        // Checked by Call-ID plus the caller's own tag, not Call-ID alone
        // (PR review, 2026-08-26) — a retransmission of the still-unanswered
        // original INVITE never carries *our* tag yet, so the fuller
        // `names_active_dialog` check (which requires it) would wrongly
        // reject exactly the retransmission case this exists to catch; the
        // caller's tag is present on every request in the dialog regardless.
        if let Some(call) = self.active_call.as_ref() {
            if req.header("Call-ID") == Some(call.call_id.as_str())
                && matches_caller_tag(req, &call.dialog.to)
            {
                if let InDialogInvite::RetransmittedOriginal =
                    classify_in_dialog_invite(req, call.answered_invite.as_ref())
                {
                    let cached = call
                        .answered_invite
                        .as_ref()
                        .expect("RetransmittedOriginal only returned when Some");
                    tracing::info!(call_id = %call.call_id, "retransmitted INVITE for an already-answered call; resending the cached 200 OK");
                    let _ = sink.send(&build_uas_response_with_headers(
                        200,
                        "OK",
                        req,
                        Some(&call.to_tag),
                        Some(&cached.contact),
                        Some(&cached.answer_sdp),
                        &[("Allow", ALLOW)],
                    ));
                    return;
                }
                // A genuine re-INVITE: we don't support renegotiating a call
                // already in progress, but the line is not busy — say so
                // honestly instead of claiming `486 Busy Here`
                // (specs/042-dialog-transaction-identity, MT-02).
                tracing::info!(call_id = %call.call_id, "declining re-INVITE: in-call session modification is not supported");
                let _ = sink.send(&build_488_not_acceptable(
                    req,
                    &call.to_tag,
                    &format_sip_addr(session.contact_addr),
                ));
                return;
            }
        }
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
        if let Some(response) = bye_response_if_unmatched(req, self.active_call.as_ref()) {
            let _ = sink.send(&response);
            return;
        }
        let mut call = self
            .active_call
            .take()
            .expect("bye_response_if_unmatched returned None above, so it matched");
        // The carrier's BYE is the caller hanging up.
        call.lifecycle.end(EndedBy::Caller);
        // See the matching comment in `handle_pbx_hangup` — stop the media/
        // RTCP threads before reading what they published, not after.
        call.stop.store(true, Ordering::Relaxed);
        report_answered_call_ended(p.obs, &call);
        handle_bye(sink, req, call);
        self.maintenance.release();
    }

    /// A `CANCEL` naming the call already active gets `200 OK` on that
    /// call's own `To` tag (RFC 3261 §9.2 — the CANCEL's own transaction
    /// still needs an explicit final response, even though it can no longer
    /// affect an already-answered INVITE). Anything else falls to
    /// `unserved_method_response`'s general "no such transaction" `481`,
    /// unchanged (specs/042-dialog-transaction-identity, MT-01).
    fn handle_carrier_cancel(&self, req: &SipRequest, sink: &SipSink) {
        let active = self
            .active_call
            .as_ref()
            .map(|c| (c.call_id.as_str(), c.to_tag.as_str()));
        let _ = sink.send(&cancel_response(req, active, &random_hex(4)));
    }

    /// An `ACK` naming the active call confirms its dialog; one naming
    /// anything else (or arriving with no call active) is not — logged, not
    /// silently accepted as confirming whichever call happens to be active
    /// (specs/042-dialog-transaction-identity, MT-01). No SIP response is
    /// ever sent for an ACK either way.
    fn log_ack(&self, req: &SipRequest) {
        let active_call_id = self.active_call.as_ref().map(|c| c.call_id.as_str());
        if names_active_call(req, active_call_id) {
            tracing::debug!(call_id = ?req.header("Call-ID"), "received ACK, dialog confirmed");
        } else {
            tracing::warn!(
                call_id = ?req.header("Call-ID"),
                active_call_id = ?active_call_id,
                "received ACK for a Call-ID that does not match the active call (or no call is active)"
            );
        }
    }

    /// Drives the registration state off a reg-event `NOTIFY` — see
    /// `session::handle_notify`'s docs for what makes it report a
    /// deregistration. Reuses the exact escalation path
    /// `ping::probe_gm_connection` already uses for a dead Gm connection
    /// (`force_renewal`, honoured by `on_idle_tick`'s next pass) rather than
    /// adding a second way to force a re-registration.
    fn handle_reg_notify(
        &mut self,
        session: &crate::ims::RegisteredSession,
        p: &DispatchParams,
        req: &SipRequest,
        sink: &SipSink,
    ) {
        if crate::ims::session::handle_notify(session, sink, req) {
            self.force_renewal = true;
            let mut guard = p.status.lock().unwrap_or_else(|e| e.into_inner());
            guard.state = crate::ims::RegistrationState::Failed;
            guard.last_failure = Some((
                SystemTime::now(),
                "reg-event NOTIFY reported our own binding deregistered".to_string(),
            ));
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
    /// liveness, the multi-part-SMS reassembly expiry flush, and, if it is
    /// due, the registration renewal.
    fn on_idle_tick(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        inbound: &mut Inbound,
        p: &DispatchParams,
    ) -> BridgeResult<()> {
        // specs/047-offerless-invite-sms-reassembly (SMS-05, FR-013/SC-004):
        // this is the periodic wakeup `Reassembly`'s expiry clock actually
        // needs — every line calls `on_idle_tick` regardless of line type,
        // unlike the modem-storage sweep thread, which is never spawned at
        // all on a `pcsc_reader` line (code review finding, 2026-08-28: the
        // original design put this flush in the sweep thread on the mistaken
        // assumption that it "runs unconditionally, once per line"). Placed
        // first, unconditionally, so no early `return` later in this
        // function (renewal not due, deferred, backed off, ...) can skip it.
        flush_expired_reassembly(p);

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
                subscribe_reg_event(session, &p.reg_cfg.access_network_info);
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
    use crate::ims::session::{
        caller_identity_is_private, escape_display_name, extract_caller_name,
    };
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
            access_network_info: "3GPP-E-UTRAN-FDD;utran-cell-id-3gpp=40494abcdef01",
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
        // specs/045 MT-11: states the real access-network value, not a
        // hardcoded one.
        assert!(msg.contains(
            "P-Access-Network-Info: 3GPP-E-UTRAN-FDD;utran-cell-id-3gpp=40494abcdef01\r\n"
        ));
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

    /// specs/045 MT-12: a trusted network element's `P-Asserted-Identity`
    /// wins over the caller-supplied `From` when both are present — measured
    /// on real carrier traffic where the two legitimately differ (an SMSC
    /// gateway's own hostname in `From`, the real subscriber in P-Asserted-Identity).
    #[test]
    fn extract_caller_prefers_p_asserted_identity_over_from() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: <sip:gateway.ims.example>;tag=abc\r\n\
                    P-Asserted-Identity: <sip:+919000000000@ims.example>\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert_eq!(extract_caller(&req), "+919000000000");
    }

    #[test]
    fn extract_caller_falls_back_to_from_when_no_asserted_identity() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: <sip:+919000000001@ims.example>;tag=abc\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert_eq!(extract_caller(&req), "+919000000001");
    }

    /// Confirmed live 2026-09-03: an Indian carrier's inbound INVITE already
    /// carries CNAP as the P-Asserted-Identity display name, unprompted.
    #[test]
    fn extract_caller_name_reads_the_quoted_display_name_from_p_asserted_identity() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: \"Firstname Lastname\" <sip:+919000000000@ims.example;user=phone>;tag=abc\r\n\
                    P-Asserted-Identity: \"Firstname Lastname\" <tel:+919000000000;cpc=ordinary>\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert_eq!(
            extract_caller_name(&req),
            Some("Firstname Lastname".to_string())
        );
    }

    #[test]
    fn extract_caller_name_falls_back_to_from_when_no_asserted_identity() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: \"Firstname Lastname\" <sip:+919000000000@ims.example>;tag=abc\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert_eq!(
            extract_caller_name(&req),
            Some("Firstname Lastname".to_string())
        );
    }

    /// Confirmed live: the Nokia SBC's `X-P-Asserted-Identity` and a bare
    /// `P-Asserted-Identity`/`From` with no quoted name at all are both real
    /// cases — must be `None`, never a placeholder like `extract_caller`'s
    /// `"unknown"`, since a caller could plausibly be named that.
    #[test]
    fn extract_caller_name_is_none_when_neither_header_has_a_display_name() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: <sip:+919000000000@ims.example>;tag=abc\r\n\
                    P-Asserted-Identity: <sip:+919000000000@ims.example>\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert_eq!(extract_caller_name(&req), None);
    }

    #[test]
    fn extract_caller_name_is_none_for_an_empty_quoted_name() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: \"\" <sip:+919000000000@ims.example>;tag=abc\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert_eq!(extract_caller_name(&req), None);
    }

    /// Regression: `P-Asserted-Identity` names one party (no display name of
    /// its own) and `From` names a *different* one (an SMSC gateway relay,
    /// e.g., with a display name). `extract_caller` sources the number from
    /// PAI; `extract_caller_name` must stick to that same header rather than
    /// independently falling through to `From`'s name — otherwise the onward
    /// leg would present a name that does not belong to the asserted number.
    #[test]
    fn extract_caller_name_does_not_mix_identity_sources() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: \"Gateway Relay\" <sip:gateway.ims.example>;tag=abc\r\n\
                    P-Asserted-Identity: <sip:+919000000000@ims.example>\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert_eq!(extract_caller(&req), "+919000000000");
        assert_eq!(extract_caller_name(&req), None);
    }

    #[test]
    fn extract_caller_name_rejects_an_embedded_crlf() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: \"Evil\r\nX-Injected: yes\" <sip:+919000000000@ims.example>;tag=abc\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        // The malformed header likely fails to parse as a single header at
        // all; either way, no display name containing a raw CR/LF may ever
        // come out the other end.
        if let Some((req, _)) = SipRequest::try_parse(raw.as_bytes()).unwrap() {
            if let Some(name) = extract_caller_name(&req) {
                assert!(!name.contains(['\r', '\n']));
            }
        }
    }

    #[test]
    fn escape_display_name_escapes_backslash_and_quote() {
        assert_eq!(
            escape_display_name(r#"Firstname "Nickname" Lastname"#),
            r#"Firstname \"Nickname\" Lastname"#
        );
        assert_eq!(escape_display_name(r"a\b"), r"a\\b");
    }

    #[test]
    fn escape_display_name_leaves_an_ordinary_name_unchanged() {
        assert_eq!(
            escape_display_name("Firstname Lastname"),
            "Firstname Lastname"
        );
    }

    #[test]
    fn caller_identity_is_private_matches_case_insensitively() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: \"Firstname Lastname\" <sip:+919000000000@ims.example>;tag=abc\r\n\
                    Privacy: ID\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert!(caller_identity_is_private(&req));
    }

    #[test]
    fn caller_identity_is_private_matches_privacy_id() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: \"Firstname Lastname\" <sip:+919000000000@ims.example>;tag=abc\r\n\
                    Privacy: id\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert!(caller_identity_is_private(&req));
    }

    #[test]
    fn caller_identity_is_private_matches_privacy_user_among_multiple_values() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: \"Firstname Lastname\" <sip:+919000000000@ims.example>;tag=abc\r\n\
                    Privacy: header, user\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert!(caller_identity_is_private(&req));
    }

    #[test]
    fn caller_identity_is_private_is_false_when_privacy_header_absent() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: \"Firstname Lastname\" <sip:+919000000000@ims.example>;tag=abc\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert!(!caller_identity_is_private(&req));
    }

    #[test]
    fn caller_identity_is_private_is_false_for_privacy_none() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: \"Firstname Lastname\" <sip:+919000000000@ims.example>;tag=abc\r\n\
                    Privacy: none\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert!(!caller_identity_is_private(&req));
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
        let crate::ims::sms_pdu::DecodedRp::Message(decoded) = decoded else {
            panic!("RP-DATA must decode to DecodedRp::Message");
        };
        assert_eq!(decoded.sender, "+919000000001");
        assert_eq!(decoded.text, "Hi");
    }

    /// An RP-ACK is well-formed and decodes without error, but it is not a
    /// deliverable message — `handle_message` must recognise the variant and
    /// route it away from the operator feed, never treat it as text.
    #[test]
    fn decode_pdu_body_recognizes_an_rp_ack_as_not_a_message() {
        let rp_ack = vec![0x03, 0x42]; // RP-ACK, network->MS, MR=0x42
        let mut raw = b"MESSAGE sip:x SIP/2.0\r\n\
             From: <sip:gateway.ims.example>\r\n\
             Call-ID: c\r\nCSeq: 1 MESSAGE\r\n\
             Content-Type: application/vnd.3gpp.sms\r\n"
            .to_vec();
        raw.extend_from_slice(format!("Content-Length: {}\r\n\r\n", rp_ack.len()).as_bytes());
        raw.extend_from_slice(&rp_ack);

        let (req, _) = SipRequest::try_parse(&raw).unwrap().unwrap();
        assert_eq!(
            decode_pdu_body(&req),
            Some(crate::ims::sms_pdu::DecodedRp::Ack { rp_mr: 0x42 })
        );
    }

    /// specs/045 SMS-02: a TPDU that isn't SMS-DELIVER must be recognized as
    /// such, never fall through to `handle_message`'s raw-bytes-as-text path.
    #[test]
    fn decode_pdu_body_recognizes_a_non_deliver_tpdu_as_not_a_message() {
        // RP-DATA (MTI=1), MR=0x11, no addresses, one-byte TPDU with
        // TP-MTI=10 (SMS-STATUS-REPORT).
        let rp = vec![0x01, 0x11, 0x00, 0x00, 0x01, 0b10];
        let mut raw = b"MESSAGE sip:x SIP/2.0\r\n\
             From: <sip:gateway.ims.example>\r\n\
             Call-ID: c\r\nCSeq: 1 MESSAGE\r\n\
             Content-Type: application/vnd.3gpp.sms\r\n"
            .to_vec();
        raw.extend_from_slice(format!("Content-Length: {}\r\n\r\n", rp.len()).as_bytes());
        raw.extend_from_slice(&rp);

        let (req, _) = SipRequest::try_parse(&raw).unwrap().unwrap();
        assert_eq!(
            decode_pdu_body(&req),
            Some(crate::ims::sms_pdu::DecodedRp::UnsupportedTpdu {
                rp_mr: 0x11,
                kind: crate::ims::sms_pdu::TpduMessageType::StatusReport,
            })
        );
    }

    /// specs/045 SMS-03: a TPDU claiming SMS-DELIVER but too short to parse
    /// must be recognized as undecodable, never fall through to
    /// `handle_message`'s raw-bytes-as-text path.
    #[test]
    fn decode_pdu_body_recognizes_a_truncated_deliver_tpdu_as_undecodable() {
        // RP-DATA (MTI=1), MR=0x13, no addresses, one-byte TPDU with
        // TP-MTI=00 (SMS-DELIVER) and nothing else.
        let rp = vec![0x01, 0x13, 0x00, 0x00, 0x01, 0x00];
        let mut raw = b"MESSAGE sip:x SIP/2.0\r\n\
             From: <sip:gateway.ims.example>\r\n\
             Call-ID: c\r\nCSeq: 1 MESSAGE\r\n\
             Content-Type: application/vnd.3gpp.sms\r\n"
            .to_vec();
        raw.extend_from_slice(format!("Content-Length: {}\r\n\r\n", rp.len()).as_bytes());
        raw.extend_from_slice(&rp);

        let (req, _) = SipRequest::try_parse(&raw).unwrap().unwrap();
        assert_eq!(
            decode_pdu_body(&req),
            Some(crate::ims::sms_pdu::DecodedRp::Undecodable { rp_mr: 0x13 })
        );
    }

    fn invite_requiring(extensions: &str) -> SipRequest {
        let raw = format!(
            "INVITE sip:me@10.0.0.9:5060 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.1.1.1:5067;branch=z9hG4bKone\r\n\
             From: <sip:caller@example.net>;tag=abc\r\n\
             To: <sip:me@example.net>\r\n\
             Call-ID: c1\r\nCSeq: 1 INVITE\r\n\
             Require: {extensions}\r\nContent-Length: 0\r\n\r\n"
        );
        SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
    }

    /// MT-03: `SUPPORTED_EXTENSIONS` covers only `precondition` today, so
    /// any `Require` naming something else must still come back — every
    /// unrecognized tag, not just the first.
    #[test]
    fn unsupported_required_extensions_lists_every_tag() {
        let req = invite_requiring("100rel, gruu");
        assert_eq!(
            unsupported_required_extensions(&req),
            vec!["100rel".to_string(), "gruu".to_string()]
        );
    }

    /// specs/048 MT-06: `precondition` is genuinely supported (bounded —
    /// see `SUPPORTED_EXTENSIONS`'s doc comment), so `Require:
    /// precondition` alone must not be listed as unsupported — while an
    /// unrelated tag combined with it still is.
    #[test]
    fn precondition_is_no_longer_listed_as_unsupported() {
        assert!(unsupported_required_extensions(&invite_requiring("precondition")).is_empty());
        assert_eq!(
            unsupported_required_extensions(&invite_requiring("100rel, precondition")),
            vec!["100rel".to_string()]
        );
    }

    /// A request with no `Require` at all has nothing to refuse.
    #[test]
    fn no_require_header_means_nothing_unsupported() {
        assert!(unsupported_required_extensions(&request("INVITE")).is_empty());
    }

    /// RFC 3261 doesn't forbid more than one `Require` header line; both
    /// must be read, the same reason `headers_all` exists for `Via`.
    #[test]
    fn unsupported_required_extensions_reads_every_require_line() {
        let raw = "INVITE sip:me@10.0.0.9:5060 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.1.1.1:5067;branch=z9hG4bKone\r\n\
             From: <sip:caller@example.net>;tag=abc\r\n\
             To: <sip:me@example.net>\r\n\
             Call-ID: c1\r\nCSeq: 1 INVITE\r\n\
             Require: 100rel\r\nRequire: timer\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert_eq!(
            unsupported_required_extensions(&req),
            vec!["100rel".to_string(), "timer".to_string()]
        );
    }

    fn message_with_content_type(content_type: Option<&str>) -> SipRequest {
        let ct_line = content_type
            .map(|ct| format!("Content-Type: {ct}\r\n"))
            .unwrap_or_default();
        let raw = format!(
            "MESSAGE sip:x SIP/2.0\r\nCall-ID: c\r\nCSeq: 1 MESSAGE\r\n{ct_line}Content-Length: 5\r\n\r\nhello"
        );
        SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
    }

    /// SMS-06: the two content types this UAS actually decodes, and the
    /// long-standing plain-text default (no header at all), are accepted.
    #[test]
    fn supported_message_content_types_are_accepted() {
        assert!(message_content_type_supported(&message_with_content_type(
            Some("application/vnd.3gpp.sms")
        )));
        assert!(message_content_type_supported(&message_with_content_type(
            Some("text/plain")
        )));
        assert!(message_content_type_supported(&message_with_content_type(
            Some("TEXT/PLAIN")
        )));
        assert!(message_content_type_supported(&message_with_content_type(
            None
        )));
    }

    /// A body this UAS cannot render (a JPEG, say) must be refused, not
    /// accepted and forwarded as whatever a lossy text conversion produces.
    #[test]
    fn an_unrecognised_message_content_type_is_not_supported() {
        assert!(!message_content_type_supported(&message_with_content_type(
            Some("image/jpeg")
        )));
    }

    /// RFC 3261 §7.2.1: the `;` starts the parameter list, not a different
    /// media type — a charset parameter must not make an otherwise-supported
    /// type look unsupported.
    #[test]
    fn message_content_type_parameters_are_ignored() {
        assert!(message_content_type_supported(&message_with_content_type(
            Some("text/plain; charset=utf-8")
        )));
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

    // ---- which call a request names (specs/042-dialog-transaction-identity) ----

    #[test]
    fn names_active_call_matches_the_same_call_id() {
        assert!(names_active_call(&request("BYE"), Some("c1")));
    }

    #[test]
    fn names_active_call_rejects_a_different_call_id() {
        assert!(!names_active_call(
            &request("BYE"),
            Some("some-other-call-id")
        ));
    }

    #[test]
    fn names_active_call_is_false_with_no_active_call() {
        assert!(!names_active_call(&request("BYE"), None));
    }

    /// `request`'s hardcoded `From: <sip:caller@example.net>;tag=abc`, as a
    /// standalone `dialog.to` value — what `test_active_call`'s third
    /// argument needs to describe a call that `request(..)` itself belongs
    /// to.
    const REQUEST_HELPER_CALLER_FROM: &str = "<sip:caller@example.net>;tag=abc";

    /// Same shape as `request`, but with a `To` tag — what every in-dialog
    /// request (a real `BYE`, a genuine re-INVITE) carries once we've
    /// answered, unlike `request`'s bare `To` (which models an *initial*
    /// INVITE, or an exact retransmission of one).
    fn in_dialog_request(method: &str, our_to_tag: &str) -> SipRequest {
        let raw = format!(
            "{method} sip:me@10.0.0.9:5060 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.1.1.1:5067;branch=z9hG4bKone\r\n\
             From: <sip:caller@example.net>;tag=abc\r\n\
             To: <sip:me@example.net>;tag={our_to_tag}\r\n\
             Call-ID: c1\r\nCSeq: 3 {method}\r\nContent-Length: 0\r\n\r\n"
        );
        SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
    }

    #[test]
    fn matches_caller_tag_recognizes_the_same_from_tag() {
        assert!(matches_caller_tag(
            &request("BYE"),
            REQUEST_HELPER_CALLER_FROM
        ));
    }

    #[test]
    fn matches_caller_tag_rejects_a_different_from_tag() {
        assert!(!matches_caller_tag(
            &request("BYE"),
            "<sip:someone-else@example.net>;tag=zzz"
        ));
    }

    /// RFC 3261 §7.3.1: parameter *names* are case-insensitive — only the
    /// tag *value* is compared byte-for-byte (PR review, 2026-08-26).
    #[test]
    fn header_tag_recognizes_the_parameter_name_regardless_of_case() {
        assert_eq!(header_tag("<sip:x@y>;TAG=abc"), Some("abc"));
        assert_eq!(header_tag("<sip:x@y>;Tag=abc"), Some("abc"));
        assert_eq!(header_tag("<sip:x@y>;tag=abc"), Some("abc"));
    }

    #[test]
    fn names_active_dialog_requires_our_to_tag_not_just_call_id() {
        // The exact PR-review finding: a request naming the active call's
        // Call-ID but carrying different dialog tags must not match.
        assert!(!names_active_dialog(
            &request("BYE"), // bare To, no tag at all
            "c1",
            "our-to-tag",
            REQUEST_HELPER_CALLER_FROM,
        ));
        assert!(!names_active_dialog(
            &in_dialog_request("BYE", "some-other-to-tag"),
            "c1",
            "our-to-tag",
            REQUEST_HELPER_CALLER_FROM,
        ));
    }

    #[test]
    fn names_active_dialog_matches_call_id_and_both_tags() {
        assert!(names_active_dialog(
            &in_dialog_request("BYE", "our-to-tag"),
            "c1",
            "our-to-tag",
            REQUEST_HELPER_CALLER_FROM,
        ));
    }

    #[test]
    fn bye_response_if_unmatched_is_none_for_the_active_calls_own_dialog() {
        let call = test_active_call("c1", "our-to-tag", REQUEST_HELPER_CALLER_FROM);
        assert!(
            bye_response_if_unmatched(&in_dialog_request("BYE", "our-to-tag"), Some(&call))
                .is_none()
        );
    }

    #[test]
    fn bye_response_if_unmatched_refuses_481_for_a_different_call_id() {
        let call = test_active_call(
            "some-other-call-id",
            "our-to-tag",
            REQUEST_HELPER_CALLER_FROM,
        );
        let resp = bye_response_if_unmatched(&in_dialog_request("BYE", "our-to-tag"), Some(&call))
            .expect("a mismatched Call-ID must be refused");
        assert!(
            resp.starts_with("SIP/2.0 481 Call/Transaction Does Not Exist\r\n"),
            "{resp}"
        );
    }

    /// The PR-review regression test: the same Call-ID as the active call,
    /// but a `BYE` naming a different dialog (different tags) must not end
    /// it — a colliding or malformed Call-ID alone must not be enough.
    #[test]
    fn bye_response_if_unmatched_refuses_481_for_a_matching_call_id_but_different_tags() {
        let call = test_active_call("c1", "our-to-tag", REQUEST_HELPER_CALLER_FROM);
        let resp =
            bye_response_if_unmatched(&in_dialog_request("BYE", "not-our-to-tag"), Some(&call))
                .expect("a Call-ID match with mismatched tags must still be refused");
        assert!(
            resp.starts_with("SIP/2.0 481 Call/Transaction Does Not Exist\r\n"),
            "{resp}"
        );
    }

    #[test]
    fn cancel_response_answers_200_ok_on_the_calls_own_to_tag_when_it_names_the_active_call() {
        let resp = cancel_response(&request("CANCEL"), Some(("c1", "our-to-tag")), "totag1");
        assert!(resp.starts_with("SIP/2.0 200 OK\r\n"), "{resp}");
        assert!(resp.contains(";tag=our-to-tag"), "{resp}");
    }

    #[test]
    fn cancel_response_falls_back_to_481_for_an_unrelated_call_id() {
        let resp = cancel_response(
            &request("CANCEL"),
            Some(("some-other-call-id", "our-to-tag")),
            "totag1",
        );
        assert!(
            resp.starts_with("SIP/2.0 481 Call/Transaction Does Not Exist\r\n"),
            "{resp}"
        );
    }

    #[test]
    fn cancel_response_falls_back_to_481_with_no_active_call() {
        let resp = cancel_response(&request("CANCEL"), None, "totag1");
        assert!(
            resp.starts_with("SIP/2.0 481 Call/Transaction Does Not Exist\r\n"),
            "{resp}"
        );
    }

    #[test]
    fn bye_response_if_unmatched_refuses_481_with_no_active_call() {
        let resp = bye_response_if_unmatched(&request("BYE"), None)
            .expect("a BYE with no call active must be refused, not answered as if one existed");
        assert!(
            resp.starts_with("SIP/2.0 481 Call/Transaction Does Not Exist\r\n"),
            "{resp}"
        );
    }
}
