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
//! | The reg-event subscription's transaction and dialog | [`reg_subscription`] |
//! | Which advertised P-CSCF to register through, and which to avoid | [`pcscf_pool`] |
//! | RFC 4028 session-timer refresh on a placed call | [`session_refresh`] |
//! | The veth link to Agent B, and the RTP relay | [`veth`] |
//! | The `probe-inbound` diagnostic | [`probe`] |

mod call;
mod inbound;
mod origination;
pub(crate) mod pcscf_pool;
mod ping;
mod probe;
mod reg_subscription;
mod session_refresh;
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
    attempt_renewal, evaluate_reg_notify, extract_caller, header_uri, log_superseded_notify,
    map_registration_error, map_registration_status_code, next_backoff, register_checked, respond,
    respond_notify, send_reg_subscribe, send_sms_delivery_report, start_inbound, to_unix,
    unresolved_caller_diagnostic, DeregEvent, Deregistration, Inbound, RegisterFailure, SubState,
    SubscribeRefresh, SubscribeRequest,
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
use pcscf_pool::{cause_of, FailureCause, PcscfPool, SelectReason};
use ping::{cseq_method, parse_cseq_number, probe_gm_connection, PingState};
use reg_subscription::{EndpointFailureCause, NotifyMatch, RegSubscription, SubAction, SubKind};
use session_refresh::{RefreshVerdict, Refresher};

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
/// checks plenty timely (a refresh is due at least 60s apart) while bounding that
/// worst case to something `PLACE_CALL_TIMEOUT` safely covers. `pub(crate)`
/// so `vowifi::mod`'s tests can assert `PLACE_CALL_TIMEOUT` stays larger
/// than this, the same cross-check `CALL_ATTEMPT_TIMEOUT` does against
/// `OUTBOUND_INVITE_TIMEOUT`.
pub(crate) const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const RETRY_INITIAL_BACKOFF: Duration = Duration::from_secs(5);
const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(120);
/// How many consecutive renewal failures this agent absorbs in-process before
/// exiting and letting the supervisor recover the line.
///
/// Spans the whole backoff ladder — 5 + 10 + 20 + 40 + 80 = 155s of retrying —
/// so an ordinary carrier or attachment blip is ridden out in-process, while a
/// fault that is still there after two and a half minutes reaches the only
/// component that can do anything about it. See
/// [`LoopState::schedule_renewal_retry`] for why exiting is the recovery.
const MAX_CONSECUTIVE_RENEWAL_FAILURES: u32 = 6;
/// How long a fresh registration must last before the network dropping it
/// reads as a new event rather than as that registration having failed.
///
/// Issue #117: a carrier answered REGISTER `200 OK` and then, 8s later,
/// reported the binding `terminated/rejected`. Re-registering on every such
/// NOTIFY with a clean slate would loop REGISTER → 200 → rejected forever, the
/// success resetting the backoff each time. Inside this window the rejection
/// counts against the renewal that produced the binding instead — see
/// [`LoopState::on_network_deregistration`].
const REGISTRATION_SETTLE: Duration = Duration::from_secs(60);

/// `last_failure` when an unanswered reg-event SUBSCRIBE and its confirming
/// Gm probe both fail (specs/090 FR-011a).
const QUALIFYING_FAILURE_REASON: &str =
    "reg-event SUBSCRIBE unanswered and the Gm keepalive probe failed; re-registering";

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
    // Before anything that can fail: an agent killed mid-call leaves a `1`
    // behind, and its successor must clear it even if it never registers, or
    // the supervisor would hold a stuck-rekey rebuild for a call that is gone
    // (specs/093 research R5). Best-effort, like the count itself.
    let _ = crate::ims::pcscf_files::write_active_calls(
        &crate::ims::pcscf_files::calls_path(&config.pcscf_source_path),
        0,
    );

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
    let signal_slot: crate::volte::sms::SignalSlot =
        Arc::new(crate::volte::sms::SignalShare::default());
    let signal_poll = crate::volte::sms::SignalPoll::for_interval(
        app_config.metrics.signal_poll_interval_seconds,
        Arc::clone(&signal_slot),
    );
    let modem_lock = if wants_modem_sms_reader(config.pcsc_reader) {
        let lock = Arc::new(crate::modules::modem_lock::ModemLock::new());
        let modem_port = PathBuf::from(&config.modem_port);
        let sweep_lock = lock.clone();
        let sweep_dedupe = dedupe.clone();
        let sweep_signal = signal_poll.clone();
        if let Err(e) = std::thread::Builder::new()
            .name(format!("vowifi-sms-{card_id}"))
            .spawn(move || {
                crate::volte::sms::run_modem_reader(
                    modem_port,
                    control_addr,
                    sweep_lock,
                    sweep_dedupe,
                    sweep_signal,
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
        sms_delivery_report: config.sms_delivery_report,
        sms_report_pcscf_route: config.sms_report_pcscf_route,
        respect_caller_privacy: config.respect_caller_privacy,
        amr_passthrough: config.amr_passthrough,
        gm_options_keepalive: config.gm_options_keepalive,
        amr_mode_set: config.amr_mode_set,
        // Already this line's value: discovery applied any `[[vowifi.line]]`
        // override to the per-line config.
        reg_rejected_cooldown: Duration::from_secs(config.reg_rejected_cooldown_sec),
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
        pcscf_pool: PcscfPool::new(
            pcscf_addr,
            transport_handle.pool.clone(),
            Some(crate::ims::pcscf_files::state_path(
                &config.pcscf_source_path,
            )),
        ),
        pcscf_source: Some(config.pcscf_source_path.clone()),
        // A `pcsc_reader` line has no sweep thread, so nothing would ever
        // fill the slot.
        signal_slot: (signal_poll.is_some() && wants_modem_sms_reader(config.pcsc_reader))
            .then_some(signal_slot),
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
/// them (RFC 3261 §8.2.2.3) — feeds only [`unsupported_required_extensions`],
/// not the literal `Supported` header text on the 2xx-to-INVITE response
/// (that's `inbound.rs`'s own hardcoded string; see the note there,
/// specs/050). `precondition` is real, if bounded: this UAS honours RFC
/// 3312 QoS preconditions on its own segment (no real reservation delay to
/// wait on) but still declines what it cannot honestly confirm without a
/// synchronization mechanism it doesn't implement (specs/048 MT-06).
/// `timer`/`100rel`/`replaces`/`path`/`gruu` stay absent from *this* list —
/// nothing here implements Require-time behaviour for any of them — even
/// though `inbound.rs` states them in `Supported` anyway as carrier-interop
/// boilerplate Jio's IMS core requires (specs/050); a caller `Require`ing
/// any of them is still declined `420` by this list unchanged.
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
/// §11.2 wants the same `Allow` a 200 to an INVITE carries, and an `Accept`
/// naming the bodies an INVITE may carry (specs/088: multipart too).
fn options_response(req: &SipRequest, to_tag: &str) -> String {
    build_uas_response_with_headers(
        200,
        "OK",
        req,
        Some(to_tag),
        None,
        None,
        &[("Allow", ALLOW), ("Accept", inbound::INVITE_BODY_ACCEPT)],
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

/// How long a declined INVITE is remembered for its ACK: Timer H, 64·T1
/// (RFC 3261 §17.2.1) — past that the carrier has given up retransmitting
/// the final response, so no ACK for it can still be on its way.
const DECLINE_ACK_WINDOW: Duration = Duration::from_secs(32);
/// Bound on remembered declines, so a burst of refused calls cannot grow the
/// list without limit; the oldest is forgotten first.
const DECLINE_ACK_CAPACITY: usize = 64;

/// Call-IDs of inbound INVITEs this loop answered with a failure, kept just
/// long enough to recognise their ACK (RFC 3261 §17.1.1.3: the ACK to a
/// non-2xx carries the INVITE's Call-ID). Used only to label that ACK in the
/// log — issue #108's live trace showed a perfectly correct ACK after `486`
/// logged as unmatched. Never a gate on admitting a new INVITE.
#[derive(Default)]
struct RecentDeclines {
    entries: std::collections::VecDeque<(String, Instant)>,
}

impl RecentDeclines {
    fn record(&mut self, invite: &SipRequest, now: Instant) {
        let Some(call_id) = invite.header("Call-ID") else {
            return;
        };
        self.prune(now);
        if self.entries.len() >= DECLINE_ACK_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back((call_id.to_string(), now));
    }

    fn contains(&self, call_id: &str, now: Instant) -> bool {
        self.entries.iter().any(|(id, at)| {
            id == call_id && now.saturating_duration_since(*at) < DECLINE_ACK_WINDOW
        })
    }

    fn prune(&mut self, now: Instant) {
        while self
            .entries
            .front()
            .is_some_and(|(_, at)| now.saturating_duration_since(*at) >= DECLINE_ACK_WINDOW)
        {
            self.entries.pop_front();
        }
    }
}

/// Which call an inbound ACK belongs to, for `LoopState::log_ack`.
#[derive(Debug, PartialEq, Eq)]
enum AckDisposition {
    ActiveCall,
    DeclinedCall,
    Unmatched,
}

fn ack_disposition(
    req: &SipRequest,
    active_call_id: Option<&str>,
    declines: &RecentDeclines,
    now: Instant,
) -> AckDisposition {
    if names_active_call(req, active_call_id) {
        AckDisposition::ActiveCall
    } else if req
        .header("Call-ID")
        .is_some_and(|id| declines.contains(id, now))
    {
        AckDisposition::DeclinedCall
    } else {
        AckDisposition::Unmatched
    }
}

/// The fallback final response when handling an inbound INVITE fails
/// outright. `480`, not `486`: the line is not busy, the bridge could not be
/// built. On `to_tag` — the tag `inbound::handle_invite` used for its `180`,
/// if it got that far — since a final response on a fresh tag after a `180`
/// names a dialog the carrier has never seen (specs/084 FR-005).
fn bridge_error_response(req: &SipRequest, to_tag: &str) -> String {
    build_uas_response(
        480,
        "Temporarily Unavailable",
        req,
        Some(to_tag),
        None,
        None,
    )
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
    /// See `config::VowifiConfig::sms_delivery_report` /
    /// `config::VolteConfig::sms_delivery_report` — each caller supplies its
    /// own section's copy, same as `respond_on_client` above.
    pub sms_delivery_report: bool,
    /// See `config::VowifiConfig::sms_report_pcscf_route` /
    /// `config::VolteConfig::sms_report_pcscf_route`.
    pub sms_report_pcscf_route: bool,
    /// See `config::VowifiConfig::respect_caller_privacy` /
    /// `config::VolteConfig::respect_caller_privacy` — each caller supplies
    /// its own section's copy, same as `respond_on_client` above.
    pub respect_caller_privacy: bool,
    /// See `config::VowifiConfig::amr_passthrough`. Both transports read the
    /// `[vowifi]` copy: it is one deployment-wide setting (FR-015).
    pub amr_passthrough: bool,
    /// See `config::VowifiConfig::gm_options_keepalive`. Both transports read
    /// the `[vowifi]` copy: it is one deployment-wide setting.
    pub gm_options_keepalive: bool,
    /// See `config::VowifiConfig::amr_mode_set`.
    pub amr_mode_set: crate::amr_forward::ModeSetPolicy,
    /// This line's resolved `reg_rejected_cooldown_sec` (specs/089) — each
    /// caller resolves its own section and per-line override.
    pub reg_rejected_cooldown: Duration,
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
    /// The P-CSCFs this line may register through, with any marks a previous
    /// run left (specs/091). Built by the caller: VoWiFi from the published
    /// pool, VoLTE from its single configured P-CSCF.
    pub pcscf_pool: PcscfPool,
    /// The supervisor's selection file (`/tmp/pcscf-N`), whose pool is
    /// re-read before every renewal (FR-014). `None` on VoLTE, where nothing
    /// republishes the P-CSCF.
    pub pcscf_source: Option<String>,
    /// Where this line's modem sweep leaves its latest signal reading, for the
    /// reporter to carry (specs/122-management-webui). `None` for a line with
    /// no modem sweep (`pcsc_reader`) or with polling off.
    pub signal_slot: Option<crate::volte::sms::SignalSlot>,
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
        sms_delivery_report,
        sms_report_pcscf_route,
        respect_caller_privacy,
        amr_passthrough,
        gm_options_keepalive,
        amr_mode_set,
        reg_rejected_cooldown,
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
        pcscf_pool,
        pcscf_source,
        signal_slot,
        agent_label,
        agent_kind,
    } = p;
    let mut pcscf_pool = pcscf_pool;
    tracing::info!(
        gm_options_keepalive,
        "{}",
        if gm_options_keepalive {
            "periodic Gm OPTIONS keepalive on ([vowifi].gm_options_keepalive = true)"
        } else {
            "periodic Gm OPTIONS keepalive off ([vowifi].gm_options_keepalive = false); \
             a dead Gm connection is found by the reg-event probe, a failed send or the renewal"
        }
    );

    // Best-effort: a store that fails to open must not stop the agent from
    // registering and carrying calls (FR-018) — call history is simply
    // unavailable for this run, logged once here rather than on every insert
    // attempt.
    let history_store = match StoreHandle::open(std::path::Path::new(&app_config.sms.db_path)) {
        Ok(s) => {
            crate::store::events::install_sink(s.sender());
            Some(s)
        }
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
        AgentKind::Ims | AgentKind::Sip | AgentKind::Supervisor => crate::store::Transport::Vowifi,
    };
    let obs = observability::AgentObservability::new(
        reporter,
        card_id.to_string(),
        history_store,
        app_config.bridge.sip_destination.clone(),
        transport,
    );
    obs.set_plmn(&reg_cfg.mcc, &reg_cfg.mnc);
    if let Some(slot) = signal_slot {
        obs.attach_signal_slot(slot);
    }
    // VoWiFi only: its tunnel supervisor holds a stuck-rekey rebuild for a
    // call (specs/093). VoLTE has no `pcscf_source` and no such supervisor.
    let obs = match &pcscf_source {
        Some(src) => obs.with_calls_file(crate::ims::pcscf_files::calls_path(src)),
        None => obs,
    };

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
        let startup_wait = |d: Duration| {
            // Asleep by design, so `Dormant`; the attempt after it is startup
            // work again and gets a fresh budget.
            progress.enter(watchdog::Phase::Dormant);
            std::thread::sleep(d);
            progress.enter(watchdog::Phase::Startup);
        };
        match register_via_pool(
            &mut pcscf_pool,
            reg_cfg,
            |cfg| register_checked(cfg, "IMS registration failed"),
            |addr| crate::ims::route_to(addr, reg_cfg.pcscf_port).is_ok(),
            || {
                pcscf_source
                    .as_deref()
                    .and_then(crate::ims::pcscf_files::read_last_outage)
            },
            startup_wait,
        ) {
            Ok(s) => s,
            Err(f) => {
                drop(_guard);
                obs.report_registration_attempt(match f.status {
                    Some(status) => map_registration_status_code(status),
                    None => map_registration_error(&f.error),
                });
                obs.set_registered(false);
                obs.set_tunnel_up(false);
                return Err(f.error);
            }
        }
    };
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
    // the NOTIFY the network sends straight back on a new connection. The
    // SUBSCRIBE itself is the dispatch loop's first act, so it is tracked like
    // every later one (specs/090 R2).
    let mut inbound = start_inbound(&session)?;

    // What the registrar actually granted, not what we asked for: renewing on
    // the requested value would leave a window where the binding has lapsed
    // while we still believe it is live (FR-023).
    let granted_expires = session.granted_expires(crate::ims::DEFAULT_EXPIRES);
    crate::ims::warn_if_short_grant(granted_expires);
    let status = Arc::new(Mutex::new(crate::ims::RegistrationStatus {
        pcscf: pcscf_pool.snapshot(SystemTime::now()),
        state: crate::ims::RegistrationState::Registered,
        registered_at: Some(SystemTime::now()),
        expires_at: Some(SystemTime::now() + Duration::from_secs(granted_expires as u64)),
        last_failure: None,
        // Health starts able-to-answer: we reach here only after a successful
        // registration, and the attachment underneath it is up (the Wi-Fi path
        // has none and leaves this at its default).
        ..Default::default()
    }));

    obs.attach_registration_status(Arc::clone(&status));

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
            sms_delivery_report,
            sms_report_pcscf_route,
            // Still read from `[vowifi]` on both paths deliberately, unlike
            // `sms_delivery_report`/`respect_caller_privacy`: no carrier has
            // ever needed a non-default header set on the VoLTE path, so
            // giving it its own copy would be a config knob with no evidence
            // behind it.
            originating_headers: app_config.vowifi.originating_headers,
            respect_caller_privacy,
            amr_passthrough,
            gm_options_keepalive,
            amr_mode_set,
            pbx_registered: pbx_registered.as_ref(),
            obs: &obs,
            progress: &progress,
            reg_rejected_cooldown,
            pcscf_source: pcscf_source.as_deref(),
        },
        pcscf_pool,
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
                    rejection_cooldown_until: snapshot.rejected_until.and_then(to_unix),
                    reg_subscription: snapshot.reg_subscription.render(),
                    reg_subscription_last_failure: snapshot
                        .reg_subscription_last_failure
                        .map(|(t, msg)| (to_unix(t).unwrap_or(0), msg)),
                    pcscf: Some(Box::new(crate::vowifi::control::PcscfStatusReply {
                        active: snapshot
                            .pcscf
                            .active
                            .map(|a| a.to_string())
                            .unwrap_or_default(),
                        pool: snapshot.pcscf.pool.iter().map(|a| a.to_string()).collect(),
                        unavailable: snapshot
                            .pcscf
                            .unavailable
                            .iter()
                            .map(|(a, t)| (a.to_string(), to_unix(*t).unwrap_or(0)))
                            .collect(),
                    })),
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
        Some(ct) => crate::ims::body::MediaType::parse(ct)
            .is_some_and(|mt| SUPPORTED_MESSAGE_CONTENT_TYPES.iter().any(|t| mt.is(t))),
    }
}

fn decode_pdu_body(req: &SipRequest) -> Option<crate::ims::sms_pdu::DecodedRp> {
    // Parameters ignored, the same comparison `message_content_type_supported`
    // makes (specs/088 FR-019): a parameterised SMS type is still binary SMS.
    let is_3gpp_sms = req
        .header("Content-Type")
        .and_then(crate::ims::body::MediaType::parse)
        .is_some_and(|mt| mt.is("application/vnd.3gpp.sms"));
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
    reports: &mut ReportsInFlight,
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
                reports.record(send_sms_delivery_report(
                    session,
                    &ipsmgw,
                    &crate::ims::sms_pdu::build_rp_ack(rp_mr),
                    inbound_call_id(req),
                    p.sms_report_pcscf_route,
                ));
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
                reports.record(send_sms_delivery_report(
                    session,
                    &ipsmgw,
                    &crate::ims::sms_pdu::build_rp_error(rp_mr, None),
                    inbound_call_id(req),
                    p.sms_report_pcscf_route,
                ));
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
        acknowledge(
            session,
            sink,
            reports,
            req,
            decoded.as_ref(),
            p.sms_delivery_report,
            p.sms_report_pcscf_route,
        );
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
            reports,
            req,
            decoded.as_ref(),
            p.sms_delivery_report && claim_confirmed,
            p.sms_report_pcscf_route,
        );
        return;
    }

    // The sender came from the SIP headers (no decoded TPDU supplied one) and
    // they named nobody: record why (issue #104, FR-015). Placed after the
    // duplicate check above, so a retransmission of a message already handled
    // — same sender and body — returns before it and does not log again.
    if decoded.is_none() {
        if let Some(raw) = unresolved_caller_diagnostic(req) {
            tracing::warn!(headers = %raw, "SMS has no readable sender identity");
        }
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
                acknowledge(
                    session,
                    sink,
                    reports,
                    req,
                    decoded.as_ref(),
                    p.sms_delivery_report,
                    p.sms_report_pcscf_route,
                );
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
        acknowledge(
            session,
            sink,
            reports,
            req,
            decoded.as_ref(),
            p.sms_delivery_report,
            p.sms_report_pcscf_route,
        );
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
    reports: &mut ReportsInFlight,
    req: &SipRequest,
    decoded: Option<&crate::ims::sms_pdu::DecodedSms>,
    send_delivery_report: bool,
    pcscf_route: bool,
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
    reports.record(send_sms_delivery_report(
        session,
        &ipsmgw,
        &crate::ims::sms_pdu::build_rp_ack(decoded.rp_mr),
        inbound_call_id(req),
        pcscf_route,
    ));
}

/// The inbound `MESSAGE`'s `Call-ID`, which the delivery report names in
/// `In-Reply-To` (TS 24.341 §5.3.2.4 (d)). Sending without one is the
/// defensive fallback for a request that had none.
fn inbound_call_id(req: &SipRequest) -> Option<&str> {
    let id = req.header("Call-ID");
    if id.is_none() {
        tracing::warn!(
            "an SMS MESSAGE carried no Call-ID; sending its delivery report without In-Reply-To"
        );
    }
    id
}

/// How many unanswered delivery reports are remembered; the oldest is
/// forgotten beyond this.
const REPORTS_IN_FLIGHT_CAP: usize = 32;

/// What the network's answer to a delivery report amounts to.
#[derive(Debug, PartialEq, Eq)]
enum ReportAnswer {
    /// Not a response to a report we are tracking.
    NotOurs,
    /// A provisional response; the final one is still to come.
    Pending,
    /// A 2xx: the report was taken.
    Accepted,
    /// A final non-2xx: the report was refused.
    Rejected {
        call_id: String,
        status: u16,
        reason: String,
        warning: Option<String>,
    },
}

/// `Call-ID`s of SMS delivery reports sent and not yet finally answered, so a
/// refusal is visible rather than falling into the generic unmatched-response
/// line. Bounded; never drives a resend.
#[derive(Default)]
struct ReportsInFlight(std::collections::VecDeque<String>);

impl ReportsInFlight {
    /// Remember a report's `Call-ID` (`None`: the send failed, nothing to track).
    fn record(&mut self, call_id: Option<String>) {
        let Some(call_id) = call_id else { return };
        if self.0.len() >= REPORTS_IN_FLIGHT_CAP {
            self.0.pop_front();
        }
        self.0.push_back(call_id);
    }

    /// Classify `resp` against the tracked reports, dropping the entry on a
    /// final answer.
    fn on_response(&mut self, resp: &SipResponse) -> ReportAnswer {
        let is_message = resp
            .header("CSeq")
            .and_then(cseq_method)
            .is_some_and(|m| m.eq_ignore_ascii_case("MESSAGE"));
        let Some(call_id) = resp.header("Call-ID").filter(|_| is_message) else {
            return ReportAnswer::NotOurs;
        };
        let Some(pos) = self.0.iter().position(|c| c == call_id) else {
            return ReportAnswer::NotOurs;
        };
        if resp.status < 200 {
            return ReportAnswer::Pending;
        }
        let call_id = self.0.remove(pos).expect("position just found");
        if resp.status < 300 {
            ReportAnswer::Accepted
        } else {
            ReportAnswer::Rejected {
                call_id,
                status: resp.status,
                reason: resp.reason.clone(),
                warning: resp.header("Warning").map(str::to_string),
            }
        }
    }
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
    /// See `config::VowifiConfig::sms_report_pcscf_route`; consumed by the
    /// delivery-report call sites.
    sms_report_pcscf_route: bool,
    /// See `config::VowifiConfig::originating_headers`; consumed by
    /// [`origination::begin_origination`].
    originating_headers: crate::config::OriginatingHeaders,
    /// See `config::VowifiConfig::respect_caller_privacy`; consumed by
    /// [`inbound::caller_name_for_onward_signaling`].
    respect_caller_privacy: bool,
    /// See `config::VowifiConfig::amr_passthrough`; consumed by
    /// [`inbound::handle_invite`].
    amr_passthrough: bool,
    /// See `config::VowifiConfig::gm_options_keepalive`; consumed by
    /// [`ping::probe_gm_connection`].
    gm_options_keepalive: bool,
    amr_mode_set: crate::amr_forward::ModeSetPolicy,
    pbx_registered: Option<&'a Arc<AtomicBool>>,
    obs: &'a observability::AgentObservability,
    /// Publishes which phase the loop is in, so the watchdog can tell a line
    /// that is working from one that has stopped (specs/039-at-stall-watchdog).
    progress: &'a Arc<watchdog::Progress>,
    /// How long a reg-event `rejected` holds registration off — see
    /// `config::VowifiConfig::reg_rejected_cooldown_sec`; consumed by
    /// [`LoopState::on_network_deregistration`].
    reg_rejected_cooldown: Duration,
    /// See `InboundParams::pcscf_source`.
    pcscf_source: Option<&'a str>,
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
            amr_passthrough: self.amr_passthrough,
            amr_mode_set: self.amr_mode_set,
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

/// A reg-event `rejected` cooldown: no registration is attempted before
/// `until`. `until_wall` is the same moment, for status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RejectionCooldown {
    until: Instant,
    until_wall: SystemTime,
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
    /// Consecutive failed renewal attempts, reset by any renewal that
    /// succeeds. Bounds how long this agent keeps a dead registration to
    /// itself instead of exiting into `supervise::sim_recovery` — see
    /// [`Self::schedule_renewal_retry`].
    consecutive_renewal_failures: u32,
    /// When the registration this loop is serving was last confirmed — the
    /// loop starts right after one, and every successful renewal moves it.
    last_registered: Instant,
    /// `(backoff, consecutive_renewal_failures)` as they stood just before the
    /// last successful renewal cleared them, so a binding the network rejects
    /// within [`REGISTRATION_SETTLE`] can be counted as that renewal having
    /// failed after all.
    ladder_before_success: (Duration, u32),
    /// Whether the current registration's early loss has already been counted
    /// as a failure — a registrar can report the same loss in more than one
    /// NOTIFY, and only the first may move the ladder.
    early_loss_counted: bool,
    /// Set while a reg-event `rejected` holds registration off
    /// ([`Self::on_network_deregistration`]); cleared when the cooldown's one
    /// attempt starts, and by any successful renewal.
    rejection: Option<RejectionCooldown>,
    /// The renewal budget was spent by a loss reported mid-call, where exiting
    /// would have torn the call down. Owed to the supervisor as soon as the
    /// line is idle — checked before any renewal can run and clear the count.
    budget_exit_owed: Option<BridgeError>,
    /// Call-ID of the last INVITE whose caller identity was unreadable and
    /// already logged, so a retransmission of it (our final response lost)
    /// does not repeat the warning — see [`Self::unreadable_caller_to_log`].
    unreadable_caller_warned: Option<String>,
    /// INVITEs this loop recently declined, so their ACK reads as the
    /// expected end of a decline rather than as an unmatched ACK
    /// (specs/084 FR-006). Diagnostics only — never consulted for admission.
    recent_declines: RecentDeclines,
    /// The reg-event subscription: its transaction, dialog and timers
    /// (specs/090). The loop performs the sends it asks for.
    reg_sub: RegSubscription,
    /// Which P-CSCF each renewal goes to, and which are marked unavailable
    /// (specs/091). Handed over from the startup registration.
    pool: PcscfPool,
    /// A spec 090 qualifying endpoint failure raised by the last subscription
    /// action, waiting for `perform_sub_action` to mark the P-CSCF.
    endpoint_failure_seen: Option<String>,
    /// Delivery reports awaiting the network's answer.
    reports: ReportsInFlight,
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
            consecutive_renewal_failures: 0,
            last_registered: Instant::now(),
            ladder_before_success: (RETRY_INITIAL_BACKOFF, 0),
            early_loss_counted: false,
            rejection: None,
            budget_exit_owed: None,
            unreadable_caller_warned: None,
            recent_declines: RecentDeclines::default(),
            reg_sub: RegSubscription::new(),
            // Replaced by `dispatch_loop` with the pool the startup
            // registration chose from; this placeholder is only ever seen by
            // tests that build a bare `LoopState`.
            pool: PcscfPool::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), Vec::new(), None),
            endpoint_failure_seen: None,
            reports: ReportsInFlight::default(),
        }
    }

    /// The FR-015 diagnostic for this INVITE, at most once per Call-ID: a
    /// retransmission is the same request, and the busy / PBX-down rejection
    /// paths keep no record of it, so without this a lost `486` would log the
    /// same raw headers again with every retry. Only the most recent Call-ID is
    /// remembered — retransmissions arrive back to back, and a handful of
    /// distinct unreadable callers in between is already a different problem.
    fn unreadable_caller_to_log(&mut self, req: &SipRequest) -> Option<String> {
        let raw = unresolved_caller_diagnostic(req)?;
        let call_id = req.header("Call-ID").unwrap_or_default();
        if self.unreadable_caller_warned.as_deref() == Some(call_id) {
            return None;
        }
        self.unreadable_caller_warned = Some(call_id.to_string());
        Some(raw)
    }

    /// Is this line occupied — by a bridged call or an attempt still being
    /// placed? An in-flight origination counts: its INVITE transaction is live
    /// on this transport (specs/029).
    fn busy(&self) -> bool {
        self.active_call.is_some() || self.origination.is_some()
    }

    /// Records one failed renewal attempt: puts the next one on the backoff
    /// ladder, and reports whether the in-process retry budget is now spent.
    /// `Err` here ends `dispatch_loop`, and with it the process.
    ///
    /// # Why a failing renewal must eventually exit
    ///
    /// [`supervise::sim_recovery`](crate::supervise::sim_recovery) owns the
    /// only remedy for a USIM that has dropped off the modem bus
    /// (`AT+CFUN=0` → `AT+CFUN=1`), and it is driven entirely by *agent
    /// exits*: it classifies each finished run from that run's log and
    /// power-cycles the SIM after `CSIM_FAIL_THRESHOLD` consecutive
    /// `AT+CSIM failed` exits. An agent that retries in-process forever is
    /// therefore invisible to it, and the remedy is never applied however
    /// long the fault lasts.
    ///
    /// The startup path has always exited on exactly this failure
    /// (`run_inner`'s `register_session` returns `Err`), so recovery worked
    /// there and only there. Renewal retried instead, which left the two
    /// paths silently asymmetric: a SIM present at startup and lost later was
    /// unrecoverable without a human.
    ///
    /// That is not hypothetical. On a live line on 2026-09-11 the SIM dropped
    /// off the bus at 16:59; every renewal from then on failed with
    /// `AT+CSIM failed: 0`; this loop retried 309 times over 10h12m, pinned at
    /// `RETRY_MAX_BACKOFF`, until the container was restarted by hand. The
    /// remedy was three minutes away the whole time.
    ///
    /// Exiting is safe here by construction: `on_idle_tick` has already
    /// deferred this renewal through [`MaintenancePolicy`] if a call is in
    /// progress, so no live call is ever cut short by it.
    /// A renewal succeeded: the registration is live again, so the backoff
    /// ladder and the retry budget both start over. The counterpart to
    /// [`Self::schedule_renewal_retry`] — kept next to it so the two can't
    /// drift, since a budget that was spent but never cleared would exit the
    /// agent on the first blip after an hour of healthy renewals.
    fn on_renewal_success(&mut self) {
        self.ladder_before_success = (self.backoff, self.consecutive_renewal_failures);
        self.last_registered = Instant::now();
        self.early_loss_counted = false;
        self.rejection = None;
        self.backoff = RETRY_INITIAL_BACKOFF;
        self.consecutive_renewal_failures = 0;
        self.next_renewal_attempt = None;
    }

    /// A reg-event NOTIFY reported our current binding deregistered: apply
    /// TS 24.229 §5.1.1.7's response for its event (§5.4.1.5 on the network
    /// side). `cooldown` is this line's `reg_rejected_cooldown_sec`.
    ///
    /// | Event | Response |
    /// |---|---|
    /// | `deactivated` | re-register at once |
    /// | `unregistered` / `probation` | re-register on the backoff ladder, never sooner than the network's `retry-after` |
    /// | `rejected` | hold off for `max(cooldown, retry-after)`, then try once |
    ///
    /// A binding that had been live for [`REGISTRATION_SETTLE`] or longer was
    /// dropped by something new on the network's side. One dropped sooner
    /// means the registration that created it did not really succeed: the
    /// ladder it cleared is put back and this counts as one more failure on
    /// it, so a network that keeps dropping fresh bindings gets the same
    /// backoff, and eventually the same exit to the supervisor, as one that
    /// refuses the REGISTER outright (issue #117). A `deactivated` early loss
    /// on a clean ladder still goes out at once; it is counted all the same.
    ///
    /// `rejected` never counts against the budget: the network has said not
    /// to come back for a while, and exiting to the supervisor would only
    /// come back sooner. The cooldown is a far-off `next_renewal_attempt` that
    /// [`Self::schedule_renewal_retry`] never touches (FR-012).
    ///
    /// Counted once per registration: a repeat NOTIFY about the same loss
    /// leaves the pending retry, and any renewal failure counted since, alone.
    ///
    /// Never `Err` while `busy`. Exiting tears the session down, and the
    /// renewal path only gets to exit because the maintenance policy has
    /// already held it for any call in progress; a NOTIFY can arrive mid-call.
    /// A loss that spends the budget then is still counted, and the exit is
    /// owed until the call ends — see [`Self::take_owed_budget_exit`].
    fn on_network_deregistration(
        &mut self,
        now: Instant,
        busy: bool,
        deregistration: &Deregistration,
        cooldown: Duration,
    ) -> BridgeResult<()> {
        self.force_renewal = true;
        let age = now.saturating_duration_since(self.last_registered);
        let early = age < REGISTRATION_SETTLE;
        match deregistration.event {
            DeregEvent::Rejected => {
                let hold = cooldown.max(deregistration.retry_after.unwrap_or_default());
                let until = now + hold;
                let until_wall = SystemTime::now() + hold;
                // A failure of the one attempt after the cooldown starts the
                // normal ladder clean (FR-011).
                self.backoff = RETRY_INITIAL_BACKOFF;
                self.consecutive_renewal_failures = 0;
                self.early_loss_counted = true;
                self.rejection = Some(RejectionCooldown { until, until_wall });
                self.next_renewal_attempt = Some(until);
                tracing::warn!(
                    cooldown_secs = hold.as_secs(),
                    retry_after_secs = deregistration.retry_after.map(|r| r.as_secs()),
                    next_attempt_at = %chrono::DateTime::<Utc>::from(until_wall).to_rfc3339(),
                    "the network rejected our registration; holding off"
                );
                Ok(())
            }
            DeregEvent::Deactivated => {
                if !early || self.early_loss_counted {
                    tracing::warn!(
                        registered_secs_ago = age.as_secs(),
                        counted = false,
                        "re-registering now: the network deactivated our binding"
                    );
                    return Ok(());
                }
                self.early_loss_counted = true;
                (self.backoff, self.consecutive_renewal_failures) = self.ladder_before_success;
                let clean = self.consecutive_renewal_failures == 0;
                let result = self.count_loss(busy);
                if clean {
                    self.next_renewal_attempt = None;
                    tracing::warn!(
                        registered_secs_ago = age.as_secs(),
                        counted = true,
                        "re-registering now: the network deactivated our binding"
                    );
                } else {
                    self.log_backoff_retry(deregistration.event, age, now);
                }
                result
            }
            DeregEvent::Unregistered | DeregEvent::Probation => {
                // A repeat notice of the same loss is not counted again, but
                // a longer `retry-after` it carries still has to be honoured.
                let result = if self.early_loss_counted {
                    Ok(())
                } else {
                    self.early_loss_counted = true;
                    if early {
                        (self.backoff, self.consecutive_renewal_failures) =
                            self.ladder_before_success;
                    }
                    self.count_loss(busy)
                };
                if let Some(floor) = deregistration.retry_after.map(|r| now + r) {
                    self.next_renewal_attempt = self.next_renewal_attempt.max(Some(floor));
                    // A rejection cooldown already running is held longer
                    // too; status reports its deadline, so move that with it.
                    if let Some(r) = &mut self.rejection {
                        if floor > r.until {
                            r.until_wall += floor - r.until;
                            r.until = floor;
                        }
                    }
                }
                self.log_backoff_retry(deregistration.event, age, now);
                result
            }
        }
    }

    fn log_backoff_retry(&self, event: DeregEvent, age: Duration, now: Instant) {
        tracing::warn!(
            event = %event,
            registered_secs_ago = age.as_secs(),
            retry_in_secs = self
                .next_renewal_attempt
                .map_or(0, |t| t.saturating_duration_since(now).as_secs()),
            "the network dropped our binding; retrying on backoff"
        );
    }

    /// One registration loss counted against the renewal budget, on the
    /// ladder as it stands. Never `Err` while `busy` — the exit is owed.
    fn count_loss(&mut self, busy: bool) -> BridgeResult<()> {
        match self.schedule_renewal_retry() {
            Err(e) if busy => {
                tracing::warn!(
                    error = %e,
                    "renewal budget spent mid-call; exiting once the call ends"
                );
                self.budget_exit_owed = Some(e);
                Ok(())
            }
            result => result,
        }
    }

    /// The exit [`Self::on_network_deregistration`] held back for a call, once
    /// no call is in progress. Taken before the renewal gate in `on_idle_tick`,
    /// so a renewal's success can never clear the count that spent it.
    fn take_owed_budget_exit(&mut self) -> BridgeResult<()> {
        if self.busy() {
            return Ok(());
        }
        match self.budget_exit_owed.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn schedule_renewal_retry(&mut self) -> BridgeResult<()> {
        self.next_renewal_attempt = Some(Instant::now() + self.backoff);
        self.backoff = next_backoff(self.backoff, RETRY_MAX_BACKOFF);
        self.count_renewal_failure()
    }

    /// One more failed renewal on the budget; `Err` once it is spent.
    fn count_renewal_failure(&mut self) -> BridgeResult<()> {
        self.consecutive_renewal_failures += 1;
        if self.consecutive_renewal_failures >= MAX_CONSECUTIVE_RENEWAL_FAILURES {
            return Err(BridgeError::Ims(format!(
                "registration renewal failed {} times in a row; exiting so the supervisor can \
                 recover this line",
                self.consecutive_renewal_failures
            )));
        }
        Ok(())
    }

    /// A renewal through `addr` failed (specs/091 research R7).
    ///
    /// A failure of the P-CSCF itself, seen over a live route, marks it. If
    /// another candidate is still eligible the next attempt goes there at once
    /// — the failure is counted on the budget, but the backoff does not move
    /// (clarification Q1). Only with every candidate marked does the existing
    /// ladder apply (FR-013), each step aimed at the soonest-expiring mark.
    /// Anything else takes exactly the path it always did.
    fn on_renewal_failure(
        &mut self,
        addr: IpAddr,
        failure: &RegisterFailure,
        routed: bool,
        now: SystemTime,
    ) -> BridgeResult<()> {
        let Some(cause) = cause_of(failure) else {
            return self.schedule_renewal_retry();
        };
        if !routed {
            tracing::info!(
                pcscf = %addr,
                cause = %cause,
                why = "no-route",
                "failure not attributed to the P-CSCF; not marking"
            );
            return self.schedule_renewal_retry();
        }
        let marked = self.pool.on_failure(addr, cause.clone(), now).is_some();
        match self.pool.next_eligible(now) {
            Some(next) if marked => {
                tracing::warn!(
                    from = %addr,
                    to = %next,
                    cause = %cause,
                    "failing over to the next P-CSCF now"
                );
                self.next_renewal_attempt = None;
                self.force_renewal = true;
                self.count_renewal_failure()
            }
            None => {
                let result = self.schedule_renewal_retry();
                let soonest = self
                    .pool
                    .next_eligible(now + self.pool.soonest_expiry(now))
                    .map(|a| a.to_string())
                    .unwrap_or_default();
                tracing::warn!(
                    pcscf = %soonest,
                    pool = %self
                        .pool
                        .candidates(now)
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                    retry_in_secs = self
                        .next_renewal_attempt
                        .map_or(0, |t| t.saturating_duration_since(Instant::now()).as_secs()),
                    "every P-CSCF is unavailable; retrying on backoff"
                );
                result
            }
            Some(_) => self.schedule_renewal_retry(),
        }
    }

    /// Marks `addr` — the P-CSCF of the session the failure was seen on — for
    /// a failure of the live registration: spec 090's qualifying endpoint
    /// failure, or the Gm keepalive escalation (FR-007). Not again while it is
    /// already marked: one episode can raise both signals, and it must count
    /// once. The re-registration they trigger then selects the next candidate.
    ///
    /// The session's own address, not the pool's active one: after a renewal
    /// through B fails without marking it, the loop is still on A's session
    /// while the pool already names B, and A's next failure must not land on
    /// B (`PcscfPool::on_failure` then ignores it as not active, FR-017).
    fn mark_session_pcscf(&mut self, addr: IpAddr, cause: FailureCause, port: u16) {
        let now = SystemTime::now();
        if self.pool.is_marked(addr, now) {
            return;
        }
        if crate::ims::route_to(addr, port).is_err() {
            tracing::info!(
                pcscf = %addr,
                cause = %cause,
                why = "no-route",
                "failure not attributed to the P-CSCF; not marking"
            );
            return;
        }
        self.pool.on_failure(addr, cause, now);
    }
}

/// The `"registration through P-CSCF"` outcome line for a failed attempt
/// (FR-019).
fn log_attempt_failure(addr: IpAddr, f: &RegisterFailure) {
    let class = match f.status {
        Some(_) => "rejected",
        None => crate::ims::classify_register_failure(&f.error).as_str(),
    };
    tracing::warn!(
        pcscf = %addr,
        outcome = "failed",
        class,
        status = f.status.map(|s| s.to_string()).unwrap_or_default(),
        error = %f.error,
        "registration through P-CSCF"
    );
}

/// The startup registration, through the pool (specs/091 research R8).
///
/// A failure of the P-CSCF itself, over a live route, marks it and the next
/// eligible candidate is tried straight away, in this process — at most once
/// per candidate. Any other failure, or running out of candidates, returns it
/// exactly as a single attempt always did, so the supervisor's exit-driven
/// recovery (`sim_recovery`) sees the same exits.
///
/// If every candidate is already marked when this starts (a restart inside the
/// marks' window), `wait` sleeps until the soonest mark ends, at most
/// [`RETRY_MAX_BACKOFF`], before the one attempt: otherwise the supervisor's
/// quick respawn would retry a dead pool faster than the renewal ladder ever
/// does (FR-013, SC-003).
///
/// Marks set while the path to the ePDG was down are dropped first, and again
/// every [`OUTAGE_RECHECK`] of that wait (specs/100 FR-011): an agent restarted
/// during an outage marks the P-CSCF it could not reach, and the ePDG often
/// hands the same one back once the path returns. `last_outage` reads the
/// supervisor's record of the last outage the path came back from.
///
/// `register`, `routed` and `last_outage` are parameters so the selection
/// logic is testable without a modem; production passes `register_checked`,
/// `route_to` and the `.uplink` file.
/// How often a startup wait on marked P-CSCFs looks again for an outage they
/// were set in.
const OUTAGE_RECHECK: Duration = Duration::from_secs(5);

fn register_via_pool(
    pool: &mut PcscfPool,
    reg_cfg: &ImsRegisterConfig,
    mut register: impl FnMut(
        &ImsRegisterConfig,
    ) -> Result<crate::ims::RegisteredSession, RegisterFailure>,
    routed: impl Fn(IpAddr) -> bool,
    last_outage: impl Fn() -> Option<crate::ims::pcscf_files::Outage>,
    mut wait: impl FnMut(Duration),
) -> Result<crate::ims::RegisteredSession, RegisterFailure> {
    pool.forget_outage(last_outage());
    if !pool.has_unmarked(SystemTime::now()) {
        let wait_for = pool
            .soonest_expiry(SystemTime::now())
            .min(RETRY_MAX_BACKOFF);
        tracing::warn!(
            pcscf = %pool.next_eligible(SystemTime::now() + wait_for).map(|a| a.to_string()).unwrap_or_default(),
            wait_secs = wait_for.as_secs(),
            "every P-CSCF is unavailable; waiting before the startup attempt"
        );
        // In steps, so a path that comes back meanwhile ends the wait.
        let mut left = wait_for;
        while !left.is_zero() {
            let step = left.min(OUTAGE_RECHECK);
            wait(step);
            left -= step;
            if pool.forget_outage(last_outage()) > 0 && pool.has_unmarked(SystemTime::now()) {
                break;
            }
        }
    }
    loop {
        let selection = pool.select(SystemTime::now());
        match register(&reg_cfg.with_pcscf(selection.addr)) {
            Ok(session) => {
                pool.on_registered(selection.addr);
                tracing::info!(pcscf = %selection.addr, outcome = "registered", "registration through P-CSCF");
                return Ok(session);
            }
            Err(failure) => {
                log_attempt_failure(selection.addr, &failure);
                let Some(cause) = cause_of(&failure) else {
                    return Err(failure);
                };
                if !routed(selection.addr) {
                    tracing::info!(
                        pcscf = %selection.addr,
                        cause = %cause,
                        why = "no-route",
                        "failure not attributed to the P-CSCF; not marking"
                    );
                    return Err(failure);
                }
                let now = SystemTime::now();
                pool.on_failure(selection.addr, cause.clone(), now);
                match pool.next_eligible(now) {
                    Some(next) if selection.reason != SelectReason::AllUnavailable => {
                        tracing::warn!(
                            from = %selection.addr,
                            to = %next,
                            cause = %cause,
                            "failing over to the next P-CSCF now"
                        );
                    }
                    _ => return Err(failure),
                }
            }
        }
    }
}

fn dispatch_loop(
    session: &mut crate::ims::RegisteredSession,
    inbound: &mut Inbound,
    p: &DispatchParams,
    pool: PcscfPool,
    place_call_rx: mpsc::Receiver<PendingPlaceCall>,
) -> BridgeResult<()> {
    let mut st = LoopState::new();
    st.pool = pool;
    st.ping.periodic = p.gm_options_keepalive;
    let respond_on_client = p.respond_on_client;
    if respond_on_client {
        tracing::warn!(
            "respond_on_client is set — answering network-initiated requests on the client leg, not the socket they arrived on"
        );
    }
    let first = st.reg_sub.on_new_registration(Instant::now());
    st.perform_sub_action(session, p, first);
    loop {
        st.sync_shared_status(p);
        // An outbound call being placed occupies the line for the tunnel
        // supervisor before it is bridged (specs/093). Reconciled here, once
        // per pass, rather than at each of the places an attempt can end.
        p.obs.set_outbound_setup(st.origination.is_some());

        if st.handle_pbx_hangup(session, inbound, p) {
            continue;
        }
        if st.handle_attachment_loss(session, p) {
            continue;
        }
        if st.handle_session_refresh(session, inbound, p) {
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
                st.handle_reg_notify(session, p, &req, &sink)?;
            }
            Ok((SipMessage::Request(req), sink)) if req.method == "MESSAGE" => {
                handle_message(session, p, &mut st.reports, &req, &sink);
            }
            Ok((SipMessage::Request(req), sink)) if req.method == "UPDATE" => {
                st.handle_carrier_update(&req, &sink);
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
        guard.rejected_until = self.rejection.map(|r| r.until_wall);
        guard.reg_subscription = self.reg_sub.health(Instant::now(), SystemTime::now());
        guard.reg_subscription_last_failure = self.reg_sub.last_failure().cloned();
        guard.pcscf = self.pool.snapshot(SystemTime::now());
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
        call::end_call_best_effort(session, call, reason::ATTACHMENT_LOST);
        self.maintenance.release();
        self.watch = AttachmentWatch::default();
        true
    }

    /// specs/049: drive an active call's RFC 4028 session-timer refresh
    /// forward one tick — send this bridge's own refresh once due
    /// (`refresher == Uac`, resent a bounded number of times if
    /// unanswered — `SessionRefreshState`'s own `verdict`/`on_sent` decide
    /// when, this method just reacts to `SendNow` either way), and end the
    /// call if a refresh cycle fails in either direction (every resend
    /// exhausted with no valid response, or the carrier's own refresh
    /// never arriving in time). A call with no `session_refresh` at all (no
    /// `Session-Expires` on its `200 OK`, or an inbound-answered call) is
    /// untouched — the common case today.
    fn handle_session_refresh(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        inbound: &Inbound,
        p: &DispatchParams,
    ) -> bool {
        let Some(call) = &mut self.active_call else {
            return false;
        };
        let Some(refresh) = &mut call.session_refresh else {
            return false;
        };
        match refresh.verdict(Instant::now()) {
            RefreshVerdict::Idle => false,
            RefreshVerdict::SendNow => {
                let session_expires = refresh.refresh_header_value();
                let req = call
                    .dialog
                    .build_update_for(&call.call_id, &session_expires);
                match session.transport_mut().and_then(|t| t.send(&req)) {
                    Ok(()) => refresh.on_sent(call.dialog.cseq, Instant::now()),
                    Err(e) => {
                        tracing::warn!(call_id = %call.call_id, error = %e, "outbound: failed to send session-timer refresh");
                        refresh.on_send_failed();
                    }
                }
                false
            }
            RefreshVerdict::Overdue => {
                let mut call = self.active_call.take().expect("just matched Some");
                tracing::warn!(
                    call_id = %call.call_id,
                    "ending call: RFC 4028 session-timer refresh failed or was never \
                     received in time (specs/049)"
                );
                call.lifecycle.end(EndedBy::SessionTimerExpired);
                call.stop.store(true, Ordering::Relaxed);
                report_answered_call_ended(p.obs, &call);
                hangup_carrier(session, inbound, call, reason::SESSION_TIMER_EXPIRED);
                self.maintenance.release();
                true
            }
        }
    }

    /// specs/049, User Story 2: accept the carrier's own in-dialog session
    /// refresh instead of rejecting it — the only case `UPDATE` is served
    /// at all today. Anything else (no active call, a different dialog, a
    /// body carrying an SDP offer, or arriving while this bridge itself
    /// holds the refresher role) falls through to the same decline this
    /// bridge has always sent an `UPDATE` (FR-007: this must never weaken
    /// the separate, pre-existing re-INVITE decline in
    /// `handle_inbound_invite`, which this method does not touch).
    fn handle_carrier_update(&mut self, req: &SipRequest, sink: &SipSink) {
        // Call-ID alone does not establish dialog membership (greptile PR
        // #74 review) — `names_active_dialog` additionally requires both
        // dialog tags, the same check `bye_response_if_unmatched` already
        // applies to a `BYE`, so a stale, cross-dialog, or unrelated
        // body-less `UPDATE` sharing this call's Call-ID cannot reset the
        // refresh deadline.
        let accepts = self.active_call.as_ref().is_some_and(|call| {
            names_active_dialog(req, &call.call_id, &call.to_tag, &call.dialog.to)
                && req.body.trim().is_empty()
                && call
                    .session_refresh
                    .as_ref()
                    .is_some_and(|r| r.refresher() == Refresher::Uas)
        });
        if accepts {
            let call = self
                .active_call
                .as_mut()
                .expect("just matched Some in `accepts`");
            call.session_refresh
                .as_mut()
                .expect("just matched Some in `accepts`")
                .on_peer_refresh(Instant::now());
            let _ = sink.send(&build_uas_response_with_headers(
                200,
                "OK",
                req,
                Some(&call.to_tag),
                None,
                None,
                &[("Supported", "timer")],
            ));
            tracing::info!(call_id = %call.call_id, "accepted the carrier's own session-timer refresh");
            return;
        }
        tracing::debug!("declining UPDATE: not a matching in-dialog session refresh");
        let _ = sink.send(&unserved_method_response(req, &random_hex(4)));
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
            p.obs.begin_live_call(call.live_call());
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
    /// Why this line must refuse a `PlaceCall` outright, if it must. Refused
    /// before `CallAttempting`, Agent B treats the line as unavailable and
    /// tries the next one. During a reg-event `rejected` cooldown the network
    /// has released this binding (TS 24.229 §5.4.1.5), so an INVITE on it
    /// would only fail at the carrier, too late for Agent B to move on.
    fn outbound_refusal(&self) -> Option<&'static str> {
        if self.busy() {
            Some("busy")
        } else if self.rejection.is_some() {
            Some(reason::REGISTRATION_REJECTED)
        } else {
            None
        }
    }

    fn handle_place_call_request(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        p: &DispatchParams,
        place_call_rx: &mpsc::Receiver<PendingPlaceCall>,
    ) -> bool {
        let Ok(mut pending) = place_call_rx.try_recv() else {
            return false;
        };
        if let Some(reason) = self.outbound_refusal() {
            tracing::debug!(
                call_id = %pending.call_id,
                reason,
                "outbound: refusing before touching the carrier"
            );
            let _ = write_msg(
                &mut pending.control,
                &ControlMessage::CallFailed {
                    call_id: pending.call_id,
                    reason: reason.to_string(),
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
        // Occupied from before the INVITE goes out, which can block: waiting
        // for the next dispatch pass to publish it would leave the attempt
        // unprotected from a stuck-rekey rebuild meanwhile (specs/093). The
        // loop-top reconcile clears it again if the attempt never started.
        p.obs.set_outbound_setup(true);
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
                        // The same shared header set the original answer was
                        // built with (`inbound::uas_answer_headers`) — not a
                        // hand-copied subset of it. A resend that dropped or
                        // fell out of step with even one of these headers
                        // would re-open exactly the teardown they exist to
                        // avoid (Greptile review, PR #85).
                        &inbound::uas_answer_headers(&p.reg_cfg.access_network_info),
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
        // A genuinely new call (the retransmission and re-INVITE cases returned
        // above): say once, with the raw values, when neither identity header
        // names a caller — every later `caller=unknown` then has a cause on
        // record (issue #104, FR-015).
        if let Some(raw) = self.unreadable_caller_to_log(req) {
            tracing::warn!(headers = %raw, "inbound call has no readable caller identity");
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
            self.recent_declines.record(req, Instant::now());
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
            self.recent_declines.record(req, Instant::now());
            p.obs.report_call_not_answered(
                CallStatus::Failed,
                BridgeFailureReason::AgentUnreachable,
                &extract_caller(req),
                Utc::now(),
            );
            return;
        }

        // One tag for every response to this INVITE, owned here so the `Err`
        // fallback below lands on the same early dialog as the `180` that
        // `handle_invite` may already have sent (specs/084 FR-005).
        let to_tag = random_hex(4);
        // Rings inside `handle_invite`; the line is occupied from here, not
        // only once the call connects (specs/093).
        p.obs.set_inbound_setup(true);
        match inbound::handle_invite(session, req, sink, inbound, &p.invite_ctx(), &to_tag) {
            Ok(call) => {
                if call.is_none() {
                    self.recent_declines.record(req, Instant::now());
                }
                if let Some(answered) = &call {
                    p.obs.set_active_calls(1);
                    p.obs.begin_live_call(answered.live_call());
                    // Fresh call, fresh media baseline (the meter starts at
                    // zero) — so a previous call's counts cannot read as a
                    // stall on this one.
                    self.watch = AttachmentWatch::default();
                }
                self.active_call = call;
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to handle inbound INVITE");
                self.recent_declines.record(req, Instant::now());
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
                if let Err(send_err) = sink.send(&bridge_error_response(req, &to_tag)) {
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
        // Setup is over either way. A connected call already counts through
        // `set_active_calls(1)` above, so the line never reads free between.
        p.obs.set_inbound_setup(false);
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
        match ack_disposition(req, active_call_id, &self.recent_declines, Instant::now()) {
            AckDisposition::ActiveCall => {
                tracing::debug!(call_id = ?req.header("Call-ID"), "received ACK, dialog confirmed");
            }
            AckDisposition::DeclinedCall => {
                tracing::debug!(call_id = ?req.header("Call-ID"), "received ACK for an INVITE we declined");
            }
            AckDisposition::Unmatched => {
                tracing::warn!(
                    call_id = ?req.header("Call-ID"),
                    active_call_id = ?active_call_id,
                    "received ACK for a Call-ID that does not match the active call (or no call is active)"
                );
            }
        }
    }

    /// Drives the registration state off a reg-event `NOTIFY` — see
    /// `session::handle_notify` for what counts as our current binding being
    /// deregistered, and [`Self::on_network_deregistration`] for the policy
    /// each event gets. Reuses the escalation path
    /// `ping::probe_gm_connection` already uses for a dead Gm connection
    /// (`force_renewal`, honoured by `on_idle_tick`'s next pass) rather than
    /// adding a second way to force a re-registration. `Err` only when the
    /// renewal budget is spent.
    ///
    /// On `rejected` the network has already released every session bound to
    /// the contact (TS 24.229 §5.4.1.5), so a call in progress, or one being
    /// placed, is ended here rather than left to discover that on its own.
    fn handle_reg_notify(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        p: &DispatchParams,
        req: &SipRequest,
        sink: &SipSink,
    ) -> BridgeResult<()> {
        let now = Instant::now();
        let Some((deregistration, state)) = self.answer_reg_notify(session, req, sink, now) else {
            return Ok(());
        };
        // Spec 089's evaluation of the body first, then the subscription's own
        // state (research R7): a terminated NOTIFY that also reports our
        // binding gone forces a renewal, which replaces the subscription.
        let result = self.apply_reg_notify(session, p, deregistration);
        self.reg_sub.apply_subscription_state(&state, now);
        result
    }

    /// Matches a NOTIFY to a subscription and answers it accordingly (specs/090
    /// R6, RFC 6665 §4.4.1): `200` and an evaluation for the current
    /// subscription, `200` and a log line for one this line replaced, `481` for
    /// anything else. Only the first is returned for the caller to act on.
    fn answer_reg_notify(
        &mut self,
        session: &crate::ims::RegisteredSession,
        req: &SipRequest,
        sink: &SipSink,
        now: Instant,
    ) -> Option<(Option<Deregistration>, SubState)> {
        match self.reg_sub.on_notify(req, now) {
            NotifyMatch::Current { state } => {
                respond_notify(sink, req, 200);
                Some((evaluate_reg_notify(session, req), state))
            }
            NotifyMatch::Superseded => {
                respond_notify(sink, req, 200);
                log_superseded_notify(session, req);
                None
            }
            NotifyMatch::Unknown => {
                respond_notify(sink, req, 481);
                let (current_call_id, current_local_tag) =
                    self.reg_sub.current_ids().unwrap_or_default();
                tracing::info!(
                    call_id = %req.header("Call-ID").unwrap_or_default(),
                    to_tag = %req.header("To").and_then(header_tag).unwrap_or_default(),
                    event = %req.header("Event").unwrap_or_default(),
                    subscription_state = %req.header("Subscription-State").unwrap_or_default(),
                    current_call_id = %current_call_id,
                    current_local_tag = %current_local_tag,
                    "NOTIFY for no subscription of ours; refused 481"
                );
                None
            }
        }
    }

    /// Applies spec 089's policy to a current NOTIFY's evaluation.
    fn apply_reg_notify(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        p: &DispatchParams,
        deregistration: Option<Deregistration>,
    ) -> BridgeResult<()> {
        if deregistration.is_some_and(|d| d.event == DeregEvent::Rejected) {
            self.end_calls_on_rejection(session, &|call| report_answered_call_ended(p.obs, call));
        }
        let busy = self.busy();
        if !self.on_reg_notify(
            Instant::now(),
            busy,
            deregistration.as_ref(),
            p.reg_rejected_cooldown,
        )? {
            return Ok(());
        }
        let reason = match (&deregistration, &self.rejection) {
            (Some(d), Some(r)) if d.event == DeregEvent::Rejected => format!(
                "the network rejected our registration (reg-event rejected); next attempt at {}",
                chrono::DateTime::<Utc>::from(r.until_wall).to_rfc3339()
            ),
            (Some(d), _) => format!(
                "the network deregistered our binding (reg-event {})",
                d.event
            ),
            (None, _) => unreachable!("on_reg_notify applied nothing without a deregistration"),
        };
        {
            let mut guard = p.status.lock().unwrap_or_else(|e| e.into_inner());
            guard.state = crate::ims::RegistrationState::Failed;
            guard.last_failure = Some((SystemTime::now(), reason));
            guard.rejected_until = self.rejection.map(|r| r.until_wall);
        }
        p.obs.set_registered(false);
        Ok(())
    }

    /// Performs what the subscription tracker asked for (specs/090 R1), and
    /// reports a qualifying endpoint failure to observability.
    fn perform_sub_action(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        p: &DispatchParams,
        action: SubAction,
    ) {
        if self.apply_sub_action(session, &p.reg_cfg.access_network_info, p.status, action) {
            p.obs.set_registered(false);
            p.obs.set_gm_connection_up(false);
        }
        if let Some(cause) = self.endpoint_failure_seen.take() {
            self.mark_session_pcscf(
                session.pcscf_addr.ip(),
                FailureCause::EndpointFailure(cause),
                p.reg_cfg.pcscf_port,
            );
        }
    }

    /// Performs a [`SubAction`] and feeds each result back to the tracker,
    /// until it has nothing more to ask: a failed send asks for a probe, and
    /// an unsendable probe for the escalation. `true` when a qualifying
    /// endpoint failure was applied.
    fn apply_sub_action(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        access_network_info: &str,
        status: &Mutex<crate::ims::RegistrationStatus>,
        action: SubAction,
    ) -> bool {
        let mut escalated = false;
        let mut next = Some(action);
        while let Some(action) = next.take() {
            let now = Instant::now();
            next = match action {
                SubAction::SendInitial { expires } => self.send_subscribe(
                    session,
                    access_network_info,
                    expires,
                    SubKind::Initial,
                    now,
                ),
                SubAction::SendRefresh { expires } => self.send_subscribe(
                    session,
                    access_network_info,
                    expires,
                    SubKind::Refresh,
                    now,
                ),
                // Sent straight away, even mid-call, and not through the
                // keepalive: its verdict would be suppressed during a call, and
                // a dead one starts the rebuild ladder (clarifications Q1, Q3).
                SubAction::Probe { cause, call_id } => {
                    tracing::warn!(
                        call_id = %call_id.as_deref().unwrap_or(""),
                        cause = %cause,
                        in_call = self.busy(),
                        "reg-event SUBSCRIBE unanswered; probing the Gm path"
                    );
                    match session.send_gm_ping() {
                        Ok(cseq) => {
                            self.reg_sub.on_probe_sent(cseq, now);
                            None
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "confirming Gm probe could not be sent");
                            self.reg_sub.on_probe_send_failed(now)
                        }
                    }
                }
                SubAction::EndpointFailure { cause, call_id } => {
                    escalated |=
                        self.on_qualifying_endpoint_failure(cause, call_id.as_deref(), status);
                    // Marked by `perform_sub_action`, which knows the port the
                    // route probe needs. Recorded even when the re-registration
                    // was already pending: the P-CSCF is dead either way.
                    self.endpoint_failure_seen = Some(cause.to_string());
                    None
                }
            };
        }
        escalated
    }

    fn send_subscribe(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        access_network_info: &str,
        expires: u32,
        kind: SubKind,
        now: Instant,
    ) -> Option<SubAction> {
        let refresh = match kind {
            SubKind::Initial => None,
            SubKind::Refresh => {
                let d = self.reg_sub.refresh_dialog()?;
                Some(SubscribeRefresh {
                    call_id: d.call_id,
                    local_tag: d.local_tag,
                    remote_tag: d.remote_tag,
                    remote_target: d.remote_target,
                    route_set: d.route_set,
                })
            }
        };
        let sent = send_reg_subscribe(
            session,
            &SubscribeRequest {
                access_network_info,
                expires,
                refresh,
            },
        );
        match sent {
            Ok(sent) => {
                self.reg_sub.on_sent(&sent, kind, now);
                None
            }
            Err(_) => Some(self.reg_sub.on_send_failed(kind, now, self.force_renewal)),
        }
    }

    /// A reg-event SUBSCRIBE went unanswered (or could not be sent) and the
    /// confirming Gm probe failed too: the path to the P-CSCF is dead
    /// (specs/090 FR-011a). Re-register at once, bypassing the transport
    /// rebuild ladder — `probe_gm_connection` returns early while
    /// `force_renewal` is set, so a ladder part-way through is abandoned.
    /// During a call the renewal is deferred by the maintenance policy as
    /// usual, and the call is not ended (FR-022).
    ///
    /// This method and its log line are the signal spec 091 builds P-CSCF
    /// failover on. `true` when it was applied; `false` when a
    /// re-registration was already pending, which is not stacked.
    fn on_qualifying_endpoint_failure(
        &mut self,
        cause: EndpointFailureCause,
        call_id: Option<&str>,
        status: &Mutex<crate::ims::RegistrationStatus>,
    ) -> bool {
        let call_id = call_id.unwrap_or("");
        if self.force_renewal {
            tracing::info!(
                cause = %cause,
                call_id = %call_id,
                "qualifying endpoint failure while a re-registration is already pending"
            );
            return false;
        }
        self.force_renewal = true;
        self.ping.reset();
        self.reconnect_attempts = 0;
        self.gm_conn = crate::ims::GmConnectionState::Failed {
            since: ping::gm_episode_since(self.gm_conn),
        };
        {
            let mut guard = status.lock().unwrap_or_else(|e| e.into_inner());
            guard.state = crate::ims::RegistrationState::Failed;
            guard.last_failure = Some((SystemTime::now(), QUALIFYING_FAILURE_REASON.to_string()));
        }
        tracing::warn!(
            cause = %cause,
            call_id = %call_id,
            in_call = self.busy(),
            "qualifying endpoint failure: re-registering"
        );
        true
    }

    /// The loop's half of a reg-event NOTIFY, once it is classified: nothing
    /// for a NOTIFY that did not deregister our current binding (a retired
    /// previous binding, another device, `expired`), else the deregistration
    /// policy. `Ok(true)` when the registration was lost.
    fn on_reg_notify(
        &mut self,
        now: Instant,
        busy: bool,
        deregistration: Option<&Deregistration>,
        cooldown: Duration,
    ) -> BridgeResult<bool> {
        let Some(d) = deregistration else {
            return Ok(false);
        };
        self.on_network_deregistration(now, busy, d, cooldown)?;
        Ok(true)
    }

    /// FR-009: end the bridged call and fail any origination in progress,
    /// stating why. `report` records the ended call (the observability report
    /// in production).
    fn end_calls_on_rejection(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        report: &dyn Fn(&ActiveCall),
    ) {
        if let Some(mut call) = self.active_call.take() {
            tracing::warn!(
                call_id = %call.call_id,
                "ending call: the network rejected our registration"
            );
            call.lifecycle.end(EndedBy::RegistrationLost);
            // See `handle_pbx_hangup`: stop the media threads before reading
            // what they published.
            call.stop.store(true, Ordering::Relaxed);
            report(&call);
            call::end_call_best_effort(session, call, reason::REGISTRATION_REJECTED);
            self.maintenance.release();
            self.watch = AttachmentWatch::default();
        }
        if let Some(mut o) = self.origination.take() {
            origination::fail_origination(&mut o, session, reason::REGISTRATION_REJECTED);
            // Kept while its CANCEL is outstanding, so a racing answer is still
            // ACKed and BYEd; otherwise it is over.
            if o.awaiting_cancel() {
                self.origination = Some(o);
            } else {
                self.maintenance.release();
            }
        }
    }

    fn handle_carrier_response(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        p: &DispatchParams,
        resp: &SipResponse,
    ) {
        if self.take_reg_sub_response(resp, Instant::now()) {
            return;
        }
        // The network's answer to an SMS delivery report we sent. Before the
        // keepalive, which matches by CSeq number alone.
        match self.reports.on_response(resp) {
            ReportAnswer::NotOurs => {}
            ReportAnswer::Pending => return,
            ReportAnswer::Accepted => {
                tracing::debug!(status = resp.status, "SMS delivery report accepted");
                return;
            }
            ReportAnswer::Rejected {
                call_id,
                status,
                reason,
                warning,
            } => {
                tracing::warn!(
                    report_call_id = %call_id,
                    status,
                    reason = %reason,
                    warning = warning.as_deref().unwrap_or(""),
                    "the network rejected the SMS delivery report"
                );
                return;
            }
        }
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
        // specs/049: is this the response to our own sent session-timer
        // refresh? Matched by Call-ID + CSeq, same discipline as
        // `PingState::on_response` — a response naming a `cseq` outside
        // every attempt this refresh has outstanding (a superseded or not
        // yet sent refresh) is left for `refresh.on_response` itself to
        // ignore. `cseq` can name *any* outstanding attempt, not just the
        // latest resend (second greptile PR #74 review pass) —
        // `is_awaiting_response` covers the whole range.
        if let Some(call) = &mut self.active_call {
            let matches_dialog = resp.header("Call-ID") == Some(call.call_id.as_str());
            let resp_cseq = resp.header("CSeq").and_then(parse_cseq_number);
            if let (true, Some(refresh), Some(cseq)) =
                (matches_dialog, &mut call.session_refresh, resp_cseq)
            {
                if refresh.is_awaiting_response(cseq) {
                    refresh.on_response(cseq, resp.status, Instant::now());
                    return;
                }
            }
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
        // Reg-event SUBSCRIBE responses are matched above, so anything left is
        // worth surfacing rather than burying at debug.
        tracing::info!(
            status = resp.status,
            reason = %resp.reason,
            "received response outside an active transaction"
        );
    }

    /// The responses the reg-event subscription owns, taken before the
    /// keepalive sees them; `true` when consumed.
    ///
    /// - Every SUBSCRIBE response, matched by method (specs/090 R3). The
    ///   keepalive matches by CSeq number alone, so a late SUBSCRIBE response
    ///   from a replaced session could otherwise be scored as its answer.
    /// - The confirming probe's answer (R5): an `OPTIONS` like the keepalive,
    ///   matched by its exact CSeq.
    fn take_reg_sub_response(&mut self, resp: &SipResponse, now: Instant) -> bool {
        let cseq = resp.header("CSeq");
        match cseq.and_then(cseq_method) {
            Some(m) if m.eq_ignore_ascii_case("SUBSCRIBE") => {
                self.reg_sub.on_response(resp, now);
                true
            }
            Some(m) if m.eq_ignore_ascii_case("OPTIONS") => cseq
                .and_then(parse_cseq_number)
                .is_some_and(|n| self.reg_sub.on_probe_response(n, now)),
            _ => false,
        }
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
        self.take_owed_budget_exit()?;

        // specs/090 R4: the subscription's timers, before the busy gate below.
        // The confirming probe must go out mid-call (clarification Q3), and a
        // refresh is ordinary signalling, not maintenance.
        if let Some(action) = self.reg_sub.poll(Instant::now(), self.force_renewal) {
            self.perform_sub_action(session, p, action);
        }

        // Gm connection liveness (specs/028) runs here, on the idle path, only
        // when no call is in progress — a live call proves the connection by
        // itself and its own signaling must not be disturbed (FR-006). An
        // in-flight origination counts as a call in progress (specs/029): its
        // INVITE transaction is live on this transport, so a keepalive OPTIONS
        // or a reconnect must not cut across it. Repeated repair failure sets
        // `force_renewal`, which the renewal gate below honours.
        if !self.busy() {
            let _phase = p.progress.phase_guard(watchdog::Phase::GmProbe);
            if probe_gm_connection(
                session,
                inbound,
                p.obs,
                &mut self.ping,
                &mut self.gm_conn,
                &mut self.reconnect_attempts,
                &mut self.force_renewal,
            ) {
                self.mark_session_pcscf(
                    session.pcscf_addr.ip(),
                    FailureCause::GmKeepalive,
                    p.reg_cfg.pcscf_port,
                );
            }
        }

        // Never renew mid-call — that would tear down the transport a call's
        // own signaling (e.g. the eventual BYE) still needs; renewal is
        // deferred until the call ends.
        let (expires_at, registered_at) = {
            let guard = p.status.lock().unwrap_or_else(|e| e.into_inner());
            (guard.expires_at, guard.registered_at)
        };
        // TS 24.229 5.1.1.4.1: 600s before expiry for a grant over 1200s,
        // half the grant otherwise, never closer than the 60s refresh floor.
        // The granted value needs no separate field: it is exactly the span
        // the status already records (FR-024).
        let due = expires_at.is_some_and(|e| {
            crate::ims::registration_refresh_due(SystemTime::now(), registered_at, e)
        });
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
        // The rejection cooldown's one attempt (FR-011): from here on a
        // failure lands on the ordinary ladder, which the rejection reset.
        if self.rejection.take().is_some() {
            tracing::info!("rejection cooldown over; trying to register once");
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
                    return self.schedule_renewal_retry();
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
                return self.schedule_renewal_retry();
            }
        }
        // The supervisor may have republished the pool since the last attempt
        // (a rebuild, or an address joining): candidates and marks follow it
        // (FR-014). Unreadable files leave the pool as it was.
        if let Some(src) = p.pcscf_source {
            if let Some(selection) = std::fs::read_to_string(src)
                .ok()
                .and_then(|s| s.trim().parse::<IpAddr>().ok())
            {
                let pool =
                    crate::ims::pcscf_files::read_pool(&crate::ims::pcscf_files::pool_path(src));
                self.pool.replace_pool(pool, selection);
            }
            // Nor do marks from an outage the path has since come back from
            // (specs/100 FR-011).
            self.pool
                .forget_outage(crate::ims::pcscf_files::read_last_outage(src));
        }
        let target = self.pool.select(SystemTime::now()).addr;
        match attempt_renewal(&p.reg_cfg.with_pcscf(target)) {
            Ok(new_session) => {
                self.pool.on_registered(target);
                tracing::info!(pcscf = %target, outcome = "registered", "registration through P-CSCF");
                session.cleanup();
                *session = new_session;
                // A renewal negotiates a fresh Gm SA on fresh ports, so the old
                // listeners are now bound to dead ones.
                *inbound = start_inbound(session)?;
                // Re-read the granted lifetime on every renewal: a registrar is free to
                // grant a different one each time, and carrying the first
                // response's value forever would eventually mis-time renewal.
                let granted = session.granted_expires(crate::ims::DEFAULT_EXPIRES);
                crate::ims::warn_if_short_grant(granted);
                let mut guard = p.status.lock().unwrap_or_else(|e| e.into_inner());
                guard.state = crate::ims::RegistrationState::Registered;
                guard.registered_at = Some(SystemTime::now());
                guard.expires_at = Some(SystemTime::now() + Duration::from_secs(granted as u64));
                // A renewal only reaches here through a successful re-attach
                // (the hook above), so the attachment is up.
                guard.attached = true;
                drop(guard);
                self.on_renewal_success();
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
                // A fresh subscription for the fresh registration; the old
                // one's timers go with it (FR-003).
                let action = self.reg_sub.on_new_registration(Instant::now());
                self.perform_sub_action(session, p, action);
            }
            Err(failure) => {
                log_attempt_failure(target, &failure);
                let e = &failure.error;
                tracing::warn!(
                    error = %e,
                    retry_in_secs = self.backoff.as_secs(),
                    "registration renewal failed, retrying with backoff"
                );
                p.obs.report_registration_attempt(map_registration_error(e));
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
                let routed = crate::ims::route_to(target, p.reg_cfg.pcscf_port).is_ok();
                return self.on_renewal_failure(target, &failure, routed, SystemTime::now());
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
    fn build_subscribe_carries_the_full_sec_agree_block_when_negotiated() {
        let echo = "ipsec-3gpp;q=0.5;alg=hmac-sha-1-96;ealg=null, ipsec-3gpp;q=0.3;alg=hmac-md5-96;ealg=null";
        let msg = build_subscribe(&SubscribeParts {
            impu: "sip:+919000000010@ims.example.test",
            route_headers: &[],
            via_transport: "TCP",
            local_addr: "1.2.3.4:48584".parse().unwrap(),
            contact_addr: "1.2.3.4:48586".parse().unwrap(),
            public_user: "9000000010",
            call_id: "cid1",
            from_tag: "tag1",
            cseq: 7,
            expires: 3600,
            access_network_info: "IEEE-802.11",
            security_verify: Some(echo),
            dialog: None,
        });
        assert!(msg.contains("Require: sec-agree\r\n"), "{msg}");
        assert!(msg.contains("Proxy-Require: sec-agree\r\n"), "{msg}");
        assert!(
            msg.contains(&format!("Security-Verify: {echo}\r\n")),
            "{msg}"
        );
        assert!(msg.find("Security-Verify").unwrap() < msg.find("\r\n\r\n").unwrap());
        assert!(msg.ends_with("Content-Length: 0\r\n\r\n"));
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
            security_verify: None,
            dialog: None,
        });
        assert!(!msg.contains("sec-agree") && !msg.contains("Security-Verify"));
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

    /// specs/090 R2: a refresh goes inside the subscription dialog — to the
    /// notifier's Contact, along the dialog's route set, with its tag in `To` —
    /// while keeping our own identifiers.
    #[test]
    fn build_subscribe_formats_an_in_dialog_refresh() {
        let route_set = ["Route: <sip:pcscf@10.0.0.8;lr>".to_string()];
        let msg = build_subscribe(&SubscribeParts {
            impu: "sip:+919000000000@ims.mnc000.mcc000.3gppnetwork.org",
            route_headers: &route_set,
            via_transport: "UDP",
            local_addr: "10.0.0.1:5062".parse().unwrap(),
            contact_addr: "10.0.0.1:5070".parse().unwrap(),
            public_user: "+919000000000",
            call_id: "sub-a",
            from_tag: "ltag-a",
            cseq: 9,
            expires: 3600,
            access_network_info: "3GPP-WLAN",
            security_verify: None,
            dialog: Some(crate::ims::session::SubscribeDialog {
                remote_tag: "rtag-1",
                request_uri: "sip:scscf@10.0.0.9:5060",
            }),
        });
        assert!(
            msg.starts_with("SUBSCRIBE sip:scscf@10.0.0.9:5060 SIP/2.0\r\n"),
            "{msg}"
        );
        let routes: Vec<&str> = msg.lines().filter(|l| l.starts_with("Route:")).collect();
        assert_eq!(routes, ["Route: <sip:pcscf@10.0.0.8;lr>"]);
        assert!(msg
            .contains("To: <sip:+919000000000@ims.mnc000.mcc000.3gppnetwork.org>;tag=rtag-1\r\n"));
        assert!(msg.contains(
            "From: <sip:+919000000000@ims.mnc000.mcc000.3gppnetwork.org>;tag=ltag-a\r\n"
        ));
        assert!(msg.contains("Call-ID: sub-a\r\n"));
        assert!(msg.contains("CSeq: 9 SUBSCRIBE\r\n"));
        assert!(msg.contains("Event: reg\r\n"));
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

    fn invite_with_headers(headers: &str) -> SipRequest {
        let raw = format!(
            "INVITE sip:x SIP/2.0\r\n{headers}Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
        );
        SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
    }

    fn ack_for(call_id: &str) -> SipRequest {
        let raw = format!(
            "ACK sip:x SIP/2.0\r\nCall-ID: {call_id}\r\nCSeq: 1 ACK\r\nContent-Length: 0\r\n\r\n"
        );
        SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
    }

    fn invite_for(call_id: &str) -> SipRequest {
        let raw = format!(
            "INVITE sip:x SIP/2.0\r\nCall-ID: {call_id}\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
        );
        SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
    }

    /// specs/084 FR-006: the ACK after our decline is recognised as such,
    /// within Timer H, and forgotten after it.
    #[test]
    fn the_ack_for_a_declined_invite_is_recognised_until_timer_h() {
        let t0 = Instant::now();
        let mut declines = RecentDeclines::default();
        declines.record(&invite_for("declined-1"), t0);

        assert_eq!(
            ack_disposition(
                &ack_for("declined-1"),
                None,
                &declines,
                t0 + Duration::from_millis(45)
            ),
            AckDisposition::DeclinedCall
        );
        assert_eq!(
            ack_disposition(
                &ack_for("declined-1"),
                None,
                &declines,
                t0 + DECLINE_ACK_WINDOW
            ),
            AckDisposition::Unmatched
        );
        assert_eq!(
            ack_disposition(&ack_for("other"), None, &declines, t0),
            AckDisposition::Unmatched
        );
        assert_eq!(
            ack_disposition(&ack_for("live"), Some("live"), &declines, t0),
            AckDisposition::ActiveCall
        );
    }

    /// A burst of declines cannot grow the list past its bound; the oldest
    /// goes first.
    #[test]
    fn recent_declines_are_bounded_oldest_first() {
        let t0 = Instant::now();
        let mut declines = RecentDeclines::default();
        for i in 0..=DECLINE_ACK_CAPACITY {
            declines.record(&invite_for(&format!("c{i}")), t0);
        }
        assert_eq!(declines.entries.len(), DECLINE_ACK_CAPACITY);
        assert!(!declines.contains("c0", t0));
        assert!(declines.contains(&format!("c{DECLINE_ACK_CAPACITY}"), t0));
    }

    /// specs/084 FR-005: the fallback after a failed INVITE is `480` on the
    /// tag it is handed — the `180`'s — not a fresh one.
    #[test]
    fn the_error_fallback_declines_on_the_given_to_tag() {
        let req = invite_with_headers(
            "Via: SIP/2.0/UDP 10.1.1.1:5067;branch=z9hG4bKone\r\n\
             From: <sip:+919000000000@ims.example>;tag=abc\r\n\
             To: <sip:me@ims.example>\r\n",
        );
        let resp = bridge_error_response(&req, "tagX");
        assert!(
            resp.starts_with("SIP/2.0 480 Temporarily Unavailable\r\n"),
            "{resp}"
        );
        let to = resp
            .lines()
            .find(|l| l.starts_with("To:"))
            .expect("To header");
        assert!(to.ends_with(";tag=tagX"), "{to}");
    }

    /// Issue #104, verbatim from the T2 capture (synthetic number): the
    /// caller is named only by `tel:` URIs, with vendor parameters.
    #[test]
    fn extract_caller_reads_tel_uris_from_both_headers() {
        let req = invite_with_headers(
            "From: <tel:+919000000000;noa=international;srvattri=national>;tag=example\r\n\
             P-Asserted-Identity: <tel:+919000000000>\r\n",
        );
        assert_eq!(extract_caller(&req), "+919000000000");
    }

    #[test]
    fn extract_caller_reads_a_tel_from_without_an_asserted_identity() {
        let req =
            invite_with_headers("From: <tel:+91-900-000-0001;noa=international>;tag=example\r\n");
        assert_eq!(extract_caller(&req), "+919000000001");
    }

    /// Schemes are case-insensitive (RFC 3986 §3.1) and `sips:` is a SIP URI.
    #[test]
    fn extract_caller_accepts_sips_and_any_scheme_case() {
        for from in [
            "<sips:+919000000001@ims.example>;tag=a",
            "<SIP:+919000000001@ims.example>;tag=a",
            "<Tel:+919000000001>;tag=a",
        ] {
            let req = invite_with_headers(&format!("From: {from}\r\n"));
            assert_eq!(extract_caller(&req), "+919000000001", "{from}");
        }
    }

    /// A `sip:` URI with no user part names a host, not a person: no number,
    /// rather than the hostname presented as the caller.
    #[test]
    fn extract_caller_is_unknown_for_a_host_only_uri() {
        let req = invite_with_headers("From: <sip:gateway.ims.example>;tag=a\r\n");
        assert_eq!(extract_caller(&req), "unknown");
    }

    /// The compact form of `From` (RFC 3261 §7.3.3) is canonicalised on parse.
    #[test]
    fn extract_caller_reads_the_compact_from_header() {
        let req = invite_with_headers("f: <tel:+919000000001>;tag=a\r\n");
        assert_eq!(extract_caller(&req), "+919000000001");
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
        // `From` names a *different* party, so this only passes if the name
        // really comes from the `tel:` P-Asserted-Identity — with the same
        // name in both it passed even while the `tel:` identity was ignored.
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: \"Other Name\" <sip:+919000000001@ims.example;user=phone>;tag=abc\r\n\
                    P-Asserted-Identity: \"Firstname Lastname\" <tel:+919000000000;cpc=ordinary>\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert_eq!(extract_caller(&req), "+919000000000");
        assert_eq!(
            extract_caller_name(&req),
            Some("Firstname Lastname".to_string())
        );
    }

    /// RFC 3325 §9.1: two values on two lines, `tel` first and nameless, the
    /// `sip` one second with the name. Number and name both come from the
    /// asserted identity, nothing from `From`.
    #[test]
    fn extract_caller_name_reads_the_second_p_asserted_identity_line() {
        let req = invite_with_headers(
            "From: \"Other Name\" <sip:+919000000001@ims.example>;tag=a\r\n\
             P-Asserted-Identity: <tel:+919000000000>\r\n\
             P-Asserted-Identity: \"Asserted Name\" <sip:+919000000000@ims.example>\r\n",
        );
        assert_eq!(extract_caller(&req), "+919000000000");
        assert_eq!(extract_caller_name(&req), Some("Asserted Name".to_string()));
    }

    /// The same two values comma-joined on one line.
    #[test]
    fn extract_caller_name_reads_a_comma_joined_p_asserted_identity() {
        let req = invite_with_headers(
            "From: \"Other Name\" <sip:+919000000001@ims.example>;tag=a\r\n\
             P-Asserted-Identity: <tel:+919000000000>, \"Asserted Name\" <sip:+919000000000@ims.example>\r\n",
        );
        assert_eq!(extract_caller(&req), "+919000000000");
        assert_eq!(extract_caller_name(&req), Some("Asserted Name".to_string()));
    }

    /// A P-Asserted-Identity naming no user (a host) yields no number, so the
    /// number *and* the name both come from `From` — never one from each.
    #[test]
    fn extract_caller_name_falls_back_to_from_with_its_own_number() {
        let req = invite_with_headers(
            "From: \"Other Name\" <tel:+919000000001>;tag=a\r\n\
             P-Asserted-Identity: \"Gateway\" <sip:gateway.ims.example>\r\n",
        );
        assert_eq!(extract_caller(&req), "+919000000001");
        assert_eq!(extract_caller_name(&req), Some("Other Name".to_string()));
    }

    /// FR-015: one warning per INVITE, not per retransmission. A rejection
    /// whose response was lost is retried with the same Call-ID.
    #[test]
    fn unreadable_caller_is_logged_once_per_call_id() {
        let invite = |call_id: &str| {
            let raw = format!(
                "INVITE sip:x SIP/2.0\r\nFrom: <sip:gateway.ims.example>;tag=a\r\n\
                 Call-ID: {call_id}\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
            );
            SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
        };
        let mut state = LoopState::new();
        assert!(state.unreadable_caller_to_log(&invite("one")).is_some());
        assert!(state.unreadable_caller_to_log(&invite("one")).is_none());
        assert!(state.unreadable_caller_to_log(&invite("one")).is_none());
        assert!(state.unreadable_caller_to_log(&invite("two")).is_some());
    }

    /// A readable caller never produces the diagnostic, so it must not
    /// consume the "already warned" slot either.
    #[test]
    fn a_readable_caller_does_not_disturb_the_warned_call_id() {
        let mut state = LoopState::new();
        let readable = invite_with_headers("From: <tel:+919000000001>;tag=a\r\n");
        assert!(state.unreadable_caller_to_log(&readable).is_none());
        assert_eq!(state.unreadable_caller_warned, None);
    }

    /// A display name that merely looks like a URI must not mislead parsing.
    #[test]
    fn a_display_name_containing_a_uri_scheme_does_not_confuse_parsing() {
        let req = invite_with_headers("From: \"sip:+919000000099\" <tel:+919000000001>;tag=a\r\n");
        assert_eq!(extract_caller(&req), "+919000000001");
        assert_eq!(
            extract_caller_name(&req),
            Some("sip:+919000000099".to_string())
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

    /// Regression: `extract_caller_name` must not double-escape a name that
    /// was already `quoted-pair`-escaped on the wire — the extraction has to
    /// undo the wire-level escaping so `escape_display_name` (applied later,
    /// when this bridge builds its own header around the name) escapes it
    /// exactly once.
    #[test]
    fn extract_caller_name_unescapes_wire_level_quoted_pairs() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: \"Doe \\\"Junior\\\"\" <sip:+919000000000@ims.example>;tag=abc\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        let name = extract_caller_name(&req).expect("a display name");
        assert_eq!(name, "Doe \"Junior\"");
        // Re-escaping the unescaped name for outbound use must produce
        // exactly one level of escaping, matching what was on the wire.
        assert_eq!(escape_display_name(&name), "Doe \\\"Junior\\\"");
    }

    /// Regression: a quoted display name may legally contain a literal `<`
    /// (RFC 3261's `qdtext` excludes only `"` and `\`) — the URI's own `<...>`
    /// must be found by tracking the quoted-string's closing quote, not by
    /// splitting on the first `<` anywhere in the header value.
    #[test]
    fn extract_caller_name_handles_a_literal_angle_bracket_inside_the_quotes() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: \"Doe <Junior>\" <sip:+919000000000@ims.example>;tag=abc\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert_eq!(extract_caller(&req), "+919000000000");
        assert_eq!(extract_caller_name(&req), Some("Doe <Junior>".to_string()));
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

    /// Regression: RFC 3323 §4.2's `Privacy-hdr` grammar separates multiple
    /// `priv-value`s with `;`, not `,` — a comma-only split missed this
    /// spec-conformant form entirely.
    #[test]
    fn caller_identity_is_private_matches_a_semicolon_delimited_value() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: \"Firstname Lastname\" <sip:+919000000000@ims.example>;tag=abc\r\n\
                    Privacy: header;id\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap();
        assert!(caller_identity_is_private(&req));
    }

    /// Regression: RFC 3261 §7.3.1 makes repeated header fields with the
    /// same name equivalent to one comma-separated field — `id`/`user` in a
    /// *second* `Privacy:` line is exactly as binding as one in the first.
    /// `req.header` (singular, first-match-only) would miss this.
    #[test]
    fn caller_identity_is_private_checks_every_repeated_privacy_field() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: \"Firstname Lastname\" <sip:+919000000000@ims.example>;tag=abc\r\n\
                    Privacy: none\r\n\
                    Privacy: id\r\n\
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

    /// RFC 3325 §9.1: a two-value P-Asserted-Identity pairs a `tel` with a
    /// `sip` URI. The delivery report is a SIP request to a network node, so
    /// it goes to the `sip` one — in either order, on one line or two.
    #[test]
    fn header_uri_prefers_the_sip_value_of_a_two_value_asserted_identity() {
        for headers in [
            "P-Asserted-Identity: <tel:+919000000000>, <sip:ipsmgw.example;transport=udp>\r\n",
            "P-Asserted-Identity: <sip:ipsmgw.example;transport=udp>, <tel:+919000000000>\r\n",
            "P-Asserted-Identity: <tel:+919000000000>\r\n\
             P-Asserted-Identity: <sip:ipsmgw.example;transport=udp>\r\n",
        ] {
            let req = message_with_headers(headers);
            assert_eq!(
                header_uri(&req, "P-Asserted-Identity").as_deref(),
                Some("sip:ipsmgw.example;transport=udp"),
                "{headers}"
            );
        }
    }

    /// A tel-only asserted identity is still addressed as it is today.
    #[test]
    fn header_uri_falls_back_to_a_tel_uri_when_that_is_all_there_is() {
        let req = message_with_headers("P-Asserted-Identity: <tel:+919000000000>\r\n");
        assert_eq!(
            header_uri(&req, "P-Asserted-Identity").as_deref(),
            Some("tel:+919000000000")
        );
    }

    /// RFC 3261 §20.10: with no `<>`, a `;param` after a `From` URI is a
    /// *header* parameter — it must not end up in the request-line.
    #[test]
    fn header_uri_drops_header_parameters_of_an_unbracketed_from() {
        let req = message_with_headers("From: sip:ipsmgw.example;tag=abc\r\n");
        assert_eq!(
            header_uri(&req, "From").as_deref(),
            Some("sip:ipsmgw.example")
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
        let decoded = decode_pdu_body(&sms_message_with_content_type("application/vnd.3gpp.sms"))
            .expect("a 3GPP SMS body must decode");
        let crate::ims::sms_pdu::DecodedRp::Message(decoded) = decoded else {
            panic!("RP-DATA must decode to DecodedRp::Message");
        };
        assert_eq!(decoded.sender, "+919000000001");
        assert_eq!(decoded.text, "Hi");
    }

    /// A `MESSAGE` carrying [`a_3gpp_sms_pdu`] under `content_type`.
    fn sms_message_with_content_type(content_type: &str) -> SipRequest {
        let pdu = a_3gpp_sms_pdu();
        let mut raw = format!(
            "MESSAGE sip:x SIP/2.0\r\n\
             From: <sip:gateway.ims.example>\r\n\
             Call-ID: c\r\nCSeq: 1 MESSAGE\r\n\
             Content-Type: {content_type}\r\n\
             Content-Length: {}\r\n\r\n",
            pdu.len()
        )
        .into_bytes();
        raw.extend_from_slice(&pdu);
        SipRequest::try_parse(&raw).unwrap().unwrap().0
    }

    fn sms_message_with_call_id(call_id: &str) -> SipRequest {
        let pdu = a_3gpp_sms_pdu();
        let mut raw = format!(
            "MESSAGE sip:x SIP/2.0\r\n\
             P-Asserted-Identity: <sip:A2P@203.0.113.7;transport=udp>\r\n\
             From: <sip:gateway.ims.example>\r\n\
             Call-ID: {call_id}\r\nCSeq: 1 MESSAGE\r\n\
             Content-Type: application/vnd.3gpp.sms\r\n\
             Content-Length: {}\r\n\r\n",
            pdu.len()
        )
        .into_bytes();
        raw.extend_from_slice(&pdu);
        SipRequest::try_parse(&raw).unwrap().unwrap().0
    }

    /// specs/101 US1: each report names the SMS it acknowledges, and a
    /// redelivery of the same Call-ID names the same one. Returns the
    /// `In-Reply-To` of the report each request produced (the `200 OK` goes
    /// out first, on the same socket).
    #[test]
    fn each_delivery_report_names_its_own_inbound_call_id() {
        let peer = peer();
        let mut session = session_to(&peer);
        let sink = session.transport().unwrap().sink().unwrap();
        let mut reports = ReportsInFlight::default();
        let mut in_reply_to = Vec::new();
        for id in [
            "first@tas.example",
            "second@tas.example",
            "first@tas.example",
        ] {
            let req = sms_message_with_call_id(id);
            let decoded = match decode_pdu_body(&req) {
                Some(crate::ims::sms_pdu::DecodedRp::Message(m)) => m,
                other => panic!("expected a decoded message, got {other:?}"),
            };
            acknowledge(
                &mut session,
                &sink,
                &mut reports,
                &req,
                Some(&decoded),
                true,
                true,
            );
            let mut buf = [0u8; 4096];
            let n = peer.recv(&mut buf).unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).starts_with("SIP/2.0 200"));
            let n = peer.recv(&mut buf).unwrap();
            let report = String::from_utf8_lossy(&buf[..n]).into_owned();
            let line = report
                .lines()
                .find_map(|l| l.strip_prefix("In-Reply-To: "))
                .unwrap_or_else(|| panic!("no In-Reply-To in {report}"));
            in_reply_to.push(line.to_string());
        }
        assert_eq!(
            in_reply_to,
            [
                "first@tas.example",
                "second@tas.example",
                "first@tas.example"
            ]
        );
        assert_eq!(reports.0.len(), 3, "every written report is tracked");
    }

    #[test]
    fn a_message_without_a_call_id_yields_none() {
        let raw = "MESSAGE sip:x SIP/2.0\r\nCSeq: 1 MESSAGE\r\nContent-Length: 0\r\n\r\n";
        let req = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0;
        assert_eq!(inbound_call_id(&req), None);
    }

    fn report_answer(status: u16, call_id: &str, warning: Option<&str>) -> SipResponse {
        let mut headers = vec![
            ("Call-ID".to_string(), call_id.to_string()),
            ("CSeq".to_string(), "4 MESSAGE".to_string()),
        ];
        if let Some(w) = warning {
            headers.push(("Warning".to_string(), w.to_string()));
        }
        SipResponse {
            status,
            reason: "Reason".into(),
            headers,
            body: String::new(),
        }
    }

    /// specs/101 US4: a final non-2xx is surfaced once with its details; a
    /// 2xx is not; a provisional waits for the final one.
    #[test]
    fn a_rejected_report_is_reported_once_and_an_accepted_one_not_at_all() {
        let mut r = ReportsInFlight::default();
        r.record(Some("rep1".into()));
        let w = "399 tas \"Timer T2 expired\"";
        assert_eq!(
            r.on_response(&report_answer(100, "rep1", None)),
            ReportAnswer::Pending
        );
        assert_eq!(
            r.on_response(&report_answer(500, "rep1", Some(w))),
            ReportAnswer::Rejected {
                call_id: "rep1".into(),
                status: 500,
                reason: "Reason".into(),
                warning: Some(w.into()),
            }
        );
        // The duplicate after removal is not announced again.
        assert_eq!(
            r.on_response(&report_answer(500, "rep1", Some(w))),
            ReportAnswer::NotOurs
        );

        r.record(Some("rep2".into()));
        assert_eq!(
            r.on_response(&report_answer(202, "rep2", None)),
            ReportAnswer::Accepted
        );
        assert_eq!(
            r.on_response(&report_answer(202, "rep2", None)),
            ReportAnswer::NotOurs
        );
    }

    #[test]
    fn an_answer_to_something_else_is_not_ours() {
        let mut r = ReportsInFlight::default();
        r.record(Some("rep1".into()));
        assert_eq!(
            r.on_response(&report_answer(500, "other", None)),
            ReportAnswer::NotOurs
        );
        let mut options = report_answer(500, "rep1", None);
        options.headers[1].1 = "4 OPTIONS".into();
        assert_eq!(r.on_response(&options), ReportAnswer::NotOurs);
        r.record(None);
        assert_eq!(r.0.len(), 1);
    }

    #[test]
    fn the_in_flight_set_is_bounded_and_evicts_the_oldest() {
        let mut r = ReportsInFlight::default();
        for i in 0..=REPORTS_IN_FLIGHT_CAP {
            r.record(Some(format!("rep{i}")));
        }
        assert_eq!(r.0.len(), REPORTS_IN_FLIGHT_CAP);
        assert_eq!(
            r.on_response(&report_answer(500, "rep0", None)),
            ReportAnswer::NotOurs
        );
        assert!(matches!(
            r.on_response(&report_answer(500, "rep1", None)),
            ReportAnswer::Rejected { .. }
        ));
    }

    /// specs/088 FR-019: parameters on the media type don't stop a 3GPP SMS
    /// body being decoded as one.
    #[test]
    fn a_parameterised_3gpp_sms_content_type_decodes_the_same() {
        let req = sms_message_with_content_type("Application/Vnd.3gpp.SMS; charset=x");
        assert!(message_content_type_supported(&req));
        let Some(crate::ims::sms_pdu::DecodedRp::Message(decoded)) = decode_pdu_body(&req) else {
            panic!("a parameterised 3GPP SMS body must decode as a message");
        };
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
        assert!(
            resp.contains(
                "\r\nAccept: application/sdp, multipart/mixed, multipart/alternative\r\n"
            ),
            "{resp}"
        );
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

    // ---- RFC 4028 session-timer refresh (specs/049) ----

    /// A real UDP "peer" so `handle_carrier_update`'s response can actually
    /// be sent and read back, mirroring `origination.rs`'s own
    /// `session_with_reachable_transport` fixture pattern.
    fn udp_sink_pair() -> (SipSink, std::net::UdpSocket) {
        let peer = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let transport =
            crate::ims::sip_client::SipTransport::connect(peer.local_addr().unwrap(), false)
                .unwrap();
        (transport.sink().unwrap(), peer)
    }

    fn recv_response(peer: &std::net::UdpSocket) -> String {
        let mut buf = [0u8; 4096];
        let (n, _) = peer.recv_from(&mut buf).expect("expected a response");
        String::from_utf8_lossy(&buf[..n]).to_string()
    }

    fn active_call_with_refresh(refresher: &str) -> ActiveCall {
        let mut call = test_active_call("c1", "our-to-tag", REQUEST_HELPER_CALLER_FROM);
        call.session_refresh = Some(
            session_refresh::SessionRefreshState::from_2xx(
                Some(&format!("300;refresher={refresher}")),
                Instant::now(),
            )
            .expect("Session-Expires present"),
        );
        call
    }

    #[test]
    fn handle_carrier_update_accepts_a_matching_body_less_refresh_when_carrier_is_the_refresher() {
        let mut st = LoopState::new();
        st.active_call = Some(active_call_with_refresh("uas"));
        let (sink, peer) = udp_sink_pair();

        st.handle_carrier_update(&in_dialog_request("UPDATE", "our-to-tag"), &sink);

        let resp = recv_response(&peer);
        assert!(resp.starts_with("SIP/2.0 200 OK\r\n"), "{resp}");
        let refresh = st
            .active_call
            .as_ref()
            .unwrap()
            .session_refresh
            .as_ref()
            .unwrap();
        // Re-armed for another full cycle, not left overdue.
        assert_eq!(refresh.verdict(Instant::now()), RefreshVerdict::Idle);
    }

    #[test]
    fn handle_carrier_update_declines_a_body_carrying_update() {
        // FR-007: an UPDATE attempting real SDP renegotiation must not be
        // accepted just because it names the active dialog.
        let mut st = LoopState::new();
        st.active_call = Some(active_call_with_refresh("uas"));
        let (sink, peer) = udp_sink_pair();
        let body = "v=0\r\nc=IN IP4 127.0.0.1\r\nm=audio 40000 RTP/AVP 0\r\n";
        let raw = format!(
            "UPDATE sip:me@10.0.0.9:5060 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.1.1.1:5067;branch=z9hG4bKone\r\n\
             From: <sip:caller@example.net>;tag=abc\r\n\
             To: <sip:me@example.net>;tag=our-to-tag\r\n\
             Call-ID: c1\r\nCSeq: 3 UPDATE\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let req = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0;

        st.handle_carrier_update(&req, &sink);

        let resp = recv_response(&peer);
        assert!(
            resp.starts_with("SIP/2.0 405 Method Not Allowed\r\n"),
            "{resp}"
        );
    }

    #[test]
    fn handle_carrier_update_declines_a_different_call_id() {
        let mut st = LoopState::new();
        st.active_call = Some(active_call_with_refresh("uas"));
        let (sink, peer) = udp_sink_pair();
        let mut req = in_dialog_request("UPDATE", "our-to-tag");
        req.headers.retain(|(k, _)| k != "Call-ID");
        req.headers
            .push(("Call-ID".to_string(), "some-other-call".to_string()));

        st.handle_carrier_update(&req, &sink);

        let resp = recv_response(&peer);
        assert!(
            resp.starts_with("SIP/2.0 405 Method Not Allowed\r\n"),
            "{resp}"
        );
    }

    #[test]
    fn handle_carrier_update_declines_a_matching_call_id_with_the_wrong_dialog_tags() {
        // greptile PR #74 review: Call-ID alone does not establish dialog
        // membership — a stale/unrelated UPDATE sharing this call's
        // Call-ID but naming different dialog tags must not reset the
        // refresh deadline.
        let mut st = LoopState::new();
        st.active_call = Some(active_call_with_refresh("uas"));
        let original_deadline = st
            .active_call
            .as_ref()
            .unwrap()
            .session_refresh
            .as_ref()
            .unwrap()
            .peer_deadline()
            .unwrap();
        let (sink, peer) = udp_sink_pair();

        st.handle_carrier_update(&in_dialog_request("UPDATE", "some-other-to-tag"), &sink);

        let resp = recv_response(&peer);
        assert!(
            resp.starts_with("SIP/2.0 405 Method Not Allowed\r\n"),
            "{resp}"
        );
        assert_eq!(
            st.active_call
                .as_ref()
                .unwrap()
                .session_refresh
                .as_ref()
                .unwrap()
                .peer_deadline()
                .unwrap(),
            original_deadline,
            "an unmatched dialog must not reset the refresh deadline"
        );
    }

    #[test]
    fn handle_carrier_update_declines_when_this_bridge_holds_the_refresher_role() {
        // The carrier isn't on refresh duty in this case (refresher == Uac)
        // — its own UPDATE here isn't a refresh this bridge is waiting on.
        let mut st = LoopState::new();
        st.active_call = Some(active_call_with_refresh("uac"));
        let (sink, peer) = udp_sink_pair();

        st.handle_carrier_update(&in_dialog_request("UPDATE", "our-to-tag"), &sink);

        let resp = recv_response(&peer);
        assert!(
            resp.starts_with("SIP/2.0 405 Method Not Allowed\r\n"),
            "{resp}"
        );
    }

    #[test]
    fn handle_carrier_update_declines_with_no_active_call() {
        let mut st = LoopState::new();
        let (sink, peer) = udp_sink_pair();

        st.handle_carrier_update(&in_dialog_request("UPDATE", "our-to-tag"), &sink);

        let resp = recv_response(&peer);
        assert!(
            resp.starts_with("SIP/2.0 405 Method Not Allowed\r\n"),
            "{resp}"
        );
    }

    // `handle_session_refresh`'s own send/overdue branches need a real
    // `RegisteredSession` *and* `Inbound` (for `hangup_carrier`'s
    // reconnect-retry path) — `Inbound` has no test constructor available
    // outside `ims::session` (its `tx` field is private there), the same
    // reason `handle_attachment_loss`/`handle_pbx_hangup` (structurally
    // identical "needs session+inbound" methods) have no direct unit test
    // here either. Its actual decision logic is the pure
    // `SessionRefreshState::verdict` state machine, fully covered in
    // `session_refresh.rs`'s own tests — the same split
    // `AttachmentWatch::attachment_lost` already uses for
    // `handle_attachment_loss`.

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

    /// A renewal that keeps failing has to end the process eventually, because
    /// `supervise::sim_recovery`'s SIM power-cycle is driven entirely by agent
    /// *exits*: an agent that retries in-process forever never reaches the one
    /// component that can fix it. On 2026-09-11 a SIM dropped off the modem bus
    /// at 16:59 and this loop retried 309 times across 10h12m — pinned at
    /// `RETRY_MAX_BACKOFF`, the line dead the whole time — with the remedy about
    /// three minutes away.
    #[test]
    fn a_persistently_failing_renewal_exits_instead_of_retrying_forever() {
        // Pinned before anything loops on it. Every assertion below is phrased
        // in terms of `MAX_CONSECUTIVE_RENEWAL_FAILURES`, so they would all
        // hold just as well for a budget of `u32::MAX` — which is precisely
        // the unbounded retry this exists to prevent. A budget that is finite
        // and small is the fix; the rest of the test only checks it is spent
        // correctly.
        assert!(
            (2..=10).contains(&MAX_CONSECUTIVE_RENEWAL_FAILURES),
            "the retry budget must stay finite and small, not {MAX_CONSECUTIVE_RENEWAL_FAILURES}"
        );
        let mut st = LoopState::new();
        // Everything below the budget stays in-process, on the backoff ladder,
        // so an ordinary blip is still ridden out rather than restarted through.
        for n in 1..MAX_CONSECUTIVE_RENEWAL_FAILURES {
            assert!(
                st.schedule_renewal_retry().is_ok(),
                "failure {n} of {MAX_CONSECUTIVE_RENEWAL_FAILURES} should still retry in-process"
            );
            assert!(
                st.next_renewal_attempt.is_some(),
                "retry {n} was not scheduled"
            );
        }
        let err = st
            .schedule_renewal_retry()
            .expect_err("the retry budget is spent; the agent must exit for the supervisor");
        // The reason travels in the message: it is what a human reads in
        // `docker logs` beside the supervisor's restart line.
        assert!(
            err.to_string()
                .contains("exiting so the supervisor can recover"),
            "{err}"
        );
    }

    /// The budget is *consecutive* failures, not lifetime ones. A line that
    /// renews healthily for hours and then hits one blip must not inherit a
    /// spent counter and exit on it.
    #[test]
    fn a_successful_renewal_clears_the_failure_budget() {
        let mut st = LoopState::new();
        for _ in 1..MAX_CONSECUTIVE_RENEWAL_FAILURES {
            st.schedule_renewal_retry().expect("still below the budget");
        }
        st.on_renewal_success();
        assert_eq!(st.consecutive_renewal_failures, 0);
        // The backoff ladder restarts with it, so the next episode gets the
        // full 155s of in-process retrying rather than resuming at the cap.
        assert_eq!(st.backoff, RETRY_INITIAL_BACKOFF);
        assert_eq!(st.next_renewal_attempt, None);
        // And the whole budget is available again.
        for _ in 1..MAX_CONSECUTIVE_RENEWAL_FAILURES {
            st.schedule_renewal_retry()
                .expect("the budget was cleared by the successful renewal");
        }
        assert!(st.schedule_renewal_retry().is_err());
    }

    /// Issue #117: REGISTER `200 OK`, then the binding dropped 8s later.
    /// Counted as that renewal failing, the retry waits on the ladder it would
    /// have been on, instead of going out at once. (#117's event was
    /// `rejected`, which specs/089 now answers with a cooldown instead; these
    /// early-loss tests use `deactivated`, which still counts.)
    #[test]
    fn a_binding_dropped_right_after_registering_retries_on_backoff() {
        let mut st = LoopState::new();
        st.schedule_renewal_retry().expect("below the budget");
        st.on_renewal_success();
        assert_eq!(st.consecutive_renewal_failures, 0);

        st.on_network_deregistration(
            st.last_registered + Duration::from_secs(8),
            false,
            &dereg(DeregEvent::Deactivated),
            COOLDOWN,
        )
        .expect("still below the budget");
        assert!(st.force_renewal);
        assert!(
            st.next_renewal_attempt.is_some(),
            "a fresh binding's rejection must wait on the backoff, not re-register at once"
        );
        assert_eq!(
            st.consecutive_renewal_failures, 2,
            "the failure before the success counts too"
        );
    }

    /// A network that keeps rejecting every binding it accepts must end in the
    /// same supervisor exit as one refusing REGISTER outright — not a REGISTER
    /// every few seconds forever, each `200 OK` wiping the slate.
    #[test]
    fn bindings_rejected_every_time_exhaust_the_renewal_budget() {
        let mut st = LoopState::new();
        for n in 1..MAX_CONSECUTIVE_RENEWAL_FAILURES {
            st.on_network_deregistration(
                st.last_registered + Duration::from_secs(8),
                false,
                &dereg(DeregEvent::Deactivated),
                COOLDOWN,
            )
            .unwrap_or_else(|e| panic!("rejection {n} should still retry in-process: {e}"));
            st.on_renewal_success();
        }
        let err = st
            .on_network_deregistration(
                st.last_registered + Duration::from_secs(8),
                false,
                &dereg(DeregEvent::Deactivated),
                COOLDOWN,
            )
            .expect_err("the retry budget is spent; the agent must exit for the supervisor");
        assert!(
            err.to_string()
                .contains("exiting so the supervisor can recover"),
            "{err}"
        );
    }

    /// One loss reported in several NOTIFYs is one failure: the repeat must
    /// neither push the pending retry out nor undo a renewal failure counted
    /// since.
    #[test]
    fn a_repeated_notice_of_the_same_loss_counts_once() {
        let mut st = LoopState::new();
        let first = st.last_registered + Duration::from_secs(8);
        st.on_network_deregistration(first, false, &dereg(DeregEvent::Deactivated), COOLDOWN)
            .expect("below the budget");
        let retry_at = st.next_renewal_attempt;
        st.schedule_renewal_retry()
            .expect("a renewal failure in between");
        let (failures, backoff, retry_after_failure) = (
            st.consecutive_renewal_failures,
            st.backoff,
            st.next_renewal_attempt,
        );
        assert_ne!(retry_after_failure, retry_at);

        st.on_network_deregistration(
            first + Duration::from_secs(5),
            false,
            &dereg(DeregEvent::Deactivated),
            COOLDOWN,
        )
        .expect("below the budget");
        assert_eq!(st.consecutive_renewal_failures, failures);
        assert_eq!(st.backoff, backoff);
        assert_eq!(st.next_renewal_attempt, retry_after_failure);
    }

    /// The loss that spends the budget arriving mid-call must not end the
    /// loop under the call — but it is still owed: the exit is taken as soon
    /// as the line is idle, before a renewal's success could clear the count.
    #[test]
    fn a_loss_that_spends_the_budget_mid_call_exits_once_the_call_ends() {
        let mut st = LoopState::new();
        for _ in 1..MAX_CONSECUTIVE_RENEWAL_FAILURES {
            st.on_network_deregistration(
                st.last_registered + Duration::from_secs(8),
                false,
                &dereg(DeregEvent::Deactivated),
                COOLDOWN,
            )
            .expect("below the budget");
            st.on_renewal_success();
        }
        let during_call = st.last_registered + Duration::from_secs(8);
        st.on_network_deregistration(during_call, true, &dereg(DeregEvent::Deactivated), COOLDOWN)
            .expect("never an exit while a call is up");
        // A repeat notice mid-call changes nothing.
        let retry_at = st.next_renewal_attempt;
        st.on_network_deregistration(
            during_call + Duration::from_secs(5),
            true,
            &dereg(DeregEvent::Deactivated),
            COOLDOWN,
        )
        .expect("never an exit while a call is up");
        assert_eq!(st.next_renewal_attempt, retry_at);
        assert_eq!(
            st.consecutive_renewal_failures,
            MAX_CONSECUTIVE_RENEWAL_FAILURES
        );

        // No call in progress (LoopState::new has none): the owed exit is taken.
        let err = st
            .take_owed_budget_exit()
            .expect_err("the budget was spent; the supervisor is owed the exit");
        assert!(
            err.to_string()
                .contains("exiting so the supervisor can recover"),
            "{err}"
        );
        assert!(st.take_owed_budget_exit().is_ok(), "taken once");
    }

    /// A binding that has been live a while and is then dropped is a new event
    /// on the network's side: re-register at once, as before, without
    /// touching the failure budget.
    #[test]
    fn a_settled_binding_dropped_re_registers_at_once() {
        let mut st = LoopState::new();
        st.on_network_deregistration(
            st.last_registered + REGISTRATION_SETTLE,
            false,
            &dereg(DeregEvent::Deactivated),
            COOLDOWN,
        )
        .expect("never an exit");
        assert!(st.force_renewal);
        assert_eq!(st.next_renewal_attempt, None);
        assert_eq!(st.consecutive_renewal_failures, 0);
        assert_eq!(st.backoff, RETRY_INITIAL_BACKOFF);
    }

    /// The budget has to span the whole backoff ladder, or it would exit while
    /// the retries were still getting cheaper — restarting the agent over a
    /// blip the in-process retry was about to absorb. Recomputed from the real
    /// constants so raising `RETRY_MAX_BACKOFF` fails here rather than silently
    /// turning this into a restart generator.
    #[test]
    fn the_retry_budget_spans_the_whole_backoff_ladder() {
        // Bound the walk before taking it, for the same reason the exit test
        // does: an unbounded budget would otherwise be "checked" by four
        // billion iterations that all pass.
        assert!(
            (2..=10).contains(&MAX_CONSECUTIVE_RENEWAL_FAILURES),
            "the retry budget must stay finite and small, not {MAX_CONSECUTIVE_RENEWAL_FAILURES}"
        );
        let mut backoff = RETRY_INITIAL_BACKOFF;
        let mut spent = Duration::ZERO;
        for _ in 1..MAX_CONSECUTIVE_RENEWAL_FAILURES {
            spent += backoff;
            backoff = next_backoff(backoff, RETRY_MAX_BACKOFF);
        }
        assert_eq!(
            backoff, RETRY_MAX_BACKOFF,
            "the ladder should be fully climbed"
        );
        assert!(
            spent >= Duration::from_secs(150),
            "only {spent:?} of in-process retrying before exiting"
        );
        // And the ceiling, which is the half that actually encodes the fix: a
        // renewal that keeps failing has to reach the supervisor in minutes.
        // The incident this came from spent 10h12m here.
        assert!(
            spent <= Duration::from_secs(600),
            "{spent:?} of in-process retrying is long enough to be the old bug again"
        );
    }

    /// SC-002 (issue #124): a renewal moved us to a new port, and the network
    /// retires the old binding a second later. The line must stay as it was —
    /// no forced renewal, nothing scheduled, nothing counted.
    #[test]
    fn a_retired_binding_right_after_renewing_changes_nothing() {
        let body = r#"<reginfo xmlns="urn:ietf:params:xml:ns:reginfo" version="7" state="full">
  <registration aor="sip:+919000000000@ims.example" id="a1" state="active">
    <contact id="old" state="terminated" event="rejected">
      <uri>sip:+919000000000@10.0.0.1:5064;transport=tcp</uri>
      <unknown-param name="+sip.instance">"&lt;urn:gsma:imei:860000000000000&gt;"</unknown-param>
    </contact>
  </registration>
</reginfo>"#;
        let verdict = crate::ims::session::classify_reg_notify(
            body,
            "active;expires=600000",
            "860000000000000",
            "10.0.0.1:5070".parse().unwrap(),
        );
        let mut st = LoopState::new();
        st.on_renewal_success();
        let applied = st
            .on_reg_notify(
                st.last_registered + Duration::from_secs(1),
                false,
                verdict.deregistration.as_ref(),
                COOLDOWN,
            )
            .expect("never an exit");
        assert!(!applied);
        assert!(!st.force_renewal);
        assert_eq!(st.next_renewal_attempt, None);
        assert_eq!(st.consecutive_renewal_failures, 0);
    }

    const COOLDOWN: Duration = Duration::from_secs(1800);

    fn dereg(event: DeregEvent) -> Deregistration {
        Deregistration {
            event,
            retry_after: None,
            scope: crate::ims::session::DeregScope::BindingRemoved,
        }
    }

    fn dereg_after(event: DeregEvent, secs: u64) -> Deregistration {
        Deregistration {
            retry_after: Some(Duration::from_secs(secs)),
            ..dereg(event)
        }
    }

    /// `next_renewal_attempt` lands `expected` after the call, give or take
    /// the time the test itself took.
    fn assert_retry_in(st: &LoopState, expected: Duration) {
        let at = st.next_renewal_attempt.expect("a retry is scheduled");
        let now = Instant::now();
        assert!(at <= now + expected, "{:?} > {expected:?}", at - now);
        assert!(
            at + Duration::from_secs(2) >= now + expected,
            "{:?} < {expected:?}",
            at.saturating_duration_since(now)
        );
    }

    /// Story 2, scenario 1: a mature binding deactivated re-registers at once,
    /// uncounted.
    #[test]
    fn a_mature_binding_deactivated_re_registers_at_once_uncounted() {
        let mut st = LoopState::new();
        st.on_network_deregistration(
            st.last_registered + REGISTRATION_SETTLE,
            false,
            &dereg(DeregEvent::Deactivated),
            COOLDOWN,
        )
        .expect("never an exit");
        assert!(st.force_renewal);
        assert_eq!(st.next_renewal_attempt, None);
        assert_eq!(st.consecutive_renewal_failures, 0);
    }

    /// Story 2, scenario 2 (clarification Q2): an early deactivated on a clean
    /// ladder goes out at once but is counted; a second one after the next
    /// success waits on the ladder.
    #[test]
    fn an_early_deactivated_goes_out_at_once_but_is_counted() {
        let mut st = LoopState::new();
        st.on_network_deregistration(
            st.last_registered + Duration::from_secs(8),
            false,
            &dereg(DeregEvent::Deactivated),
            COOLDOWN,
        )
        .expect("below the budget");
        assert_eq!(
            st.next_renewal_attempt, None,
            "the first attempt is immediate"
        );
        assert_eq!(st.consecutive_renewal_failures, 1);

        st.on_renewal_success();
        st.on_network_deregistration(
            st.last_registered + Duration::from_secs(8),
            false,
            &dereg(DeregEvent::Deactivated),
            COOLDOWN,
        )
        .expect("below the budget");
        assert_retry_in(&st, Duration::from_secs(10));
        assert_eq!(st.consecutive_renewal_failures, 2);
    }

    /// Story 2, scenario 3: unregistered waits on the ladder even on a mature
    /// binding, counted once.
    #[test]
    fn unregistered_on_a_mature_binding_retries_on_backoff() {
        let mut st = LoopState::new();
        let now = st.last_registered + REGISTRATION_SETTLE;
        st.on_network_deregistration(now, false, &dereg(DeregEvent::Unregistered), COOLDOWN)
            .expect("below the budget");
        assert!(st.force_renewal);
        assert_retry_in(&st, RETRY_INITIAL_BACKOFF);
        assert_eq!(st.consecutive_renewal_failures, 1);

        // The same loss reported again is not counted again.
        st.on_network_deregistration(now, false, &dereg(DeregEvent::Unregistered), COOLDOWN)
            .expect("below the budget");
        assert_eq!(st.consecutive_renewal_failures, 1);
    }

    /// Story 2, scenario 4: probation never retries before the network's
    /// retry-after.
    #[test]
    fn probation_waits_at_least_the_networks_retry_after() {
        let mut st = LoopState::new();
        let now = Instant::now();
        st.last_registered = now - REGISTRATION_SETTLE;
        st.on_network_deregistration(
            now,
            false,
            &dereg_after(DeregEvent::Probation, 600),
            COOLDOWN,
        )
        .expect("below the budget");
        assert!(st.next_renewal_attempt >= Some(now + Duration::from_secs(600)));
        assert_eq!(st.consecutive_renewal_failures, 1);
    }

    /// Story 2, scenario 5 (SC-003): rejected holds off for the cooldown,
    /// without spending the failure budget, then tries once.
    #[test]
    fn rejected_holds_off_for_the_cooldown_uncounted() {
        let mut st = LoopState::new();
        st.schedule_renewal_retry().expect("below the budget");
        st.on_renewal_success();
        let now = st.last_registered + Duration::from_secs(1);
        st.on_network_deregistration(now, false, &dereg(DeregEvent::Rejected), COOLDOWN)
            .expect("never an exit");
        let rejection = st.rejection.expect("a cooldown is running");
        assert_eq!(rejection.until, now + COOLDOWN);
        assert_eq!(st.next_renewal_attempt, Some(rejection.until));
        assert_eq!(st.consecutive_renewal_failures, 0);
        assert_eq!(st.backoff, RETRY_INITIAL_BACKOFF);
        assert!(st.force_renewal);
    }

    /// The network's retry-after wins when it is longer than the cooldown.
    #[test]
    fn rejected_with_a_longer_retry_after_waits_for_it() {
        let mut st = LoopState::new();
        let now = st.last_registered + Duration::from_secs(1);
        st.on_network_deregistration(
            now,
            false,
            &dereg_after(DeregEvent::Rejected, 3600),
            COOLDOWN,
        )
        .expect("never an exit");
        assert_eq!(
            st.next_renewal_attempt,
            Some(now + Duration::from_secs(3600))
        );
    }

    /// A rejection replaces a ladder retry already pending.
    #[test]
    fn rejected_replaces_a_pending_ladder_retry() {
        let mut st = LoopState::new();
        st.schedule_renewal_retry().expect("below the budget");
        let now = Instant::now();
        st.on_network_deregistration(now, false, &dereg(DeregEvent::Rejected), COOLDOWN)
            .expect("never an exit");
        assert_eq!(st.next_renewal_attempt, Some(now + COOLDOWN));
        assert_eq!(st.consecutive_renewal_failures, 0);
    }

    /// SC-004: a network that rejects us every time it accepts us — about 24h
    /// of it at a 30-minute cooldown — never exits the agent.
    #[test]
    fn a_day_of_repeated_rejections_never_spends_the_budget() {
        let mut st = LoopState::new();
        for n in 0..48 {
            let now = st.last_registered + Duration::from_secs(1);
            st.on_network_deregistration(now, false, &dereg(DeregEvent::Rejected), COOLDOWN)
                .unwrap_or_else(|e| panic!("rejection {n} exited the agent: {e}"));
            assert_eq!(st.consecutive_renewal_failures, 0, "rejection {n}");
            // The cooldown elapses, its one attempt succeeds, and the network
            // rejects the new binding again.
            st.rejection.take();
            st.on_renewal_success();
        }
        assert_eq!(st.consecutive_renewal_failures, 0);
    }

    /// FR-011: if the attempt after the cooldown fails, the ordinary ladder
    /// takes over from a clean start.
    #[test]
    fn a_failed_attempt_after_the_cooldown_lands_on_a_clean_ladder() {
        let mut st = LoopState::new();
        st.on_network_deregistration(
            st.last_registered + Duration::from_secs(1),
            false,
            &dereg(DeregEvent::Rejected),
            COOLDOWN,
        )
        .expect("never an exit");
        st.rejection.take();
        st.schedule_renewal_retry().expect("below the budget");
        assert_retry_in(&st, RETRY_INITIAL_BACKOFF);
        assert_eq!(st.consecutive_renewal_failures, 1);
    }

    #[test]
    fn a_successful_renewal_clears_the_rejection() {
        let mut st = LoopState::new();
        st.on_network_deregistration(
            st.last_registered + Duration::from_secs(1),
            false,
            &dereg(DeregEvent::Rejected),
            COOLDOWN,
        )
        .expect("never an exit");
        st.on_renewal_success();
        assert_eq!(st.rejection, None);
        assert_eq!(st.next_renewal_attempt, None);
    }

    /// Story 2, scenario 6 (FR-009): a rejection ends the call in progress and
    /// still starts the cooldown.
    #[test]
    fn rejected_ends_the_call_in_progress() {
        let mut st = LoopState::new();
        st.active_call = Some(test_active_call(
            "call-1",
            "totag",
            "<sip:+919000000001@ims.example>;tag=a",
        ));
        let mut session =
            crate::ims::test_session_with_sa(None, "127.0.0.1:5060".parse().unwrap(), None);
        let reported = std::cell::Cell::new(0);
        st.end_calls_on_rejection(&mut session, &|_| reported.set(reported.get() + 1));
        assert!(st.active_call.is_none());
        assert_eq!(reported.get(), 1);

        let busy = st.busy();
        let applied = st
            .on_reg_notify(
                Instant::now(),
                busy,
                Some(&dereg(DeregEvent::Rejected)),
                COOLDOWN,
            )
            .expect("never an exit");
        assert!(applied);
        assert!(st.rejection.is_some());
    }

    /// A later notice of the same loss carrying a longer `retry-after` is
    /// honoured, though not counted again (PR #125 review).
    #[test]
    fn a_repeat_notice_with_a_longer_retry_after_pushes_the_retry_out() {
        let mut st = LoopState::new();
        let now = Instant::now();
        st.last_registered = now - REGISTRATION_SETTLE;
        st.on_network_deregistration(now, false, &dereg(DeregEvent::Unregistered), COOLDOWN)
            .expect("below the budget");
        assert_retry_in(&st, RETRY_INITIAL_BACKOFF);

        st.on_network_deregistration(
            now,
            false,
            &dereg_after(DeregEvent::Probation, 600),
            COOLDOWN,
        )
        .expect("below the budget");
        assert!(st.next_renewal_attempt >= Some(now + Duration::from_secs(600)));
        assert_eq!(
            st.consecutive_renewal_failures, 1,
            "the same loss, counted once"
        );
    }

    /// During a rejection cooldown an outbound call is refused before the
    /// carrier is touched, so Agent B moves on to another line (PR #125
    /// review).
    #[test]
    fn outbound_calls_are_refused_during_a_rejection_cooldown() {
        let mut st = LoopState::new();
        assert_eq!(st.outbound_refusal(), None);
        st.on_network_deregistration(
            st.last_registered + Duration::from_secs(1),
            false,
            &dereg(DeregEvent::Rejected),
            COOLDOWN,
        )
        .expect("never an exit");
        assert_eq!(st.outbound_refusal(), Some(reason::REGISTRATION_REJECTED));

        // The cooldown's own attempt starts (`on_idle_tick` takes it).
        st.rejection.take();
        assert_eq!(st.outbound_refusal(), None);
    }

    /// A notice that extends the wait during a rejection cooldown moves the
    /// cooldown's reported deadline with it (PR #125 review).
    #[test]
    fn a_longer_wait_during_a_rejection_cooldown_moves_its_deadline() {
        let mut st = LoopState::new();
        let now = st.last_registered + Duration::from_secs(1);
        st.on_network_deregistration(now, false, &dereg(DeregEvent::Rejected), COOLDOWN)
            .expect("never an exit");
        let before = st.rejection.expect("a cooldown is running");

        st.on_network_deregistration(
            now,
            false,
            &dereg_after(DeregEvent::Probation, 3600),
            COOLDOWN,
        )
        .expect("never an exit");
        let after = st.rejection.expect("still cooling down");
        let extended = Duration::from_secs(3600) - COOLDOWN;
        assert_eq!(after.until, now + Duration::from_secs(3600));
        assert_eq!(st.next_renewal_attempt, Some(after.until));
        assert_eq!(after.until_wall, before.until_wall + extended);
        assert_eq!(
            st.consecutive_renewal_failures, 0,
            "the rejection is not counted"
        );

        // A shorter retry-after leaves the cooldown alone.
        st.on_network_deregistration(
            now,
            false,
            &dereg_after(DeregEvent::Unregistered, 60),
            COOLDOWN,
        )
        .expect("never an exit");
        assert_eq!(st.rejection, Some(after));
    }

    // ---- specs/090: the reg-event subscription in the dispatch loop ----

    use crate::ims::session::SentSubscribe;

    /// A registered session whose client transport is a real UDP socket to
    /// `peer`, so a test can read what the loop sends.
    fn session_to(peer: &std::net::UdpSocket) -> crate::ims::RegisteredSession {
        let transport =
            crate::ims::sip_client::SipTransport::connect(peer.local_addr().unwrap(), false)
                .unwrap();
        crate::ims::test_session_with_sa(Some(transport), peer.local_addr().unwrap(), None)
    }

    fn peer() -> std::net::UdpSocket {
        let peer = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        peer
    }

    // --- specs/091: P-CSCF failover in the agent ---

    use pcscf_pool::{FailureCause as PoolCause, PcscfPool as Pool, SelectReason as Reason};
    use std::cell::RefCell;

    /// A reg config aimed at `addr:port`. Its modem is never opened: every test
    /// below stubs the registration itself.
    fn pcscf_cfg(addr: IpAddr, port: u16, use_tcp: bool) -> ImsRegisterConfig {
        ImsRegisterConfig {
            modem_port: PathBuf::from("/dev/null"),
            pcsc_reader: false,
            pcscf_addr: addr,
            pcscf_port: port,
            mcc: "001".into(),
            mnc: "01".into(),
            imsi: None,
            imei: None,
            use_tcp,
            sec_agree: false,
            msisdn: None,
            access_network_info: crate::ims::ACCESS_NETWORK_WLAN.into(),
            register_uri_home_domain: true,
            gm_auth_alg: None,
            gm_cipher_alg: None,
        }
    }

    fn lo(n: u8) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, n))
    }

    /// The transport half of a registration attempt, for real: connect to the
    /// candidate and send a REGISTER. A refused connect and an unanswered
    /// REGISTER fail exactly as `register_session` does. Getting past that
    /// needs a SIM — MOCK JUSTIFICATION (constitution I): no modem or SIM in
    /// CI — so a connection that is accepted counts as registered.
    fn transport_only_register(
        tried: &RefCell<Vec<IpAddr>>,
    ) -> impl FnMut(&ImsRegisterConfig) -> Result<crate::ims::RegisteredSession, RegisterFailure> + '_
    {
        move |cfg| {
            tried.borrow_mut().push(cfg.pcscf_addr);
            let addr = SocketAddr::new(cfg.pcscf_addr, cfg.pcscf_port);
            let mut t = crate::ims::sip_client::SipTransport::connect(addr, cfg.use_tcp)?;
            if !cfg.use_tcp {
                t.send_and_recv("REGISTER sip:ims.example.test SIP/2.0\r\n\r\n")?;
            }
            Ok(crate::ims::test_session_with_sa(None, addr, None))
        }
    }

    /// B listens on `127.0.0.2:P`; the same port on `127.0.0.1` (A) is closed.
    fn a_refuses_b_listens() -> (std::net::TcpListener, u16) {
        let b = std::net::TcpListener::bind((lo(2), 0)).unwrap();
        let port = b.local_addr().unwrap().port();
        (b, port)
    }

    fn rejected(status: u16, retry_after: Option<u64>) -> RegisterFailure {
        RegisterFailure {
            error: BridgeError::Ims(format!("renewal REGISTER rejected: {status} X")),
            status: Some(status),
            retry_after: retry_after.map(Duration::from_secs),
        }
    }

    fn io_failure(text: &str) -> RegisterFailure {
        BridgeError::Ims(text.into()).into()
    }

    const REFUSED: &str = "TCP connect to 127.0.0.1:5060 failed: Connection refused (os error 111)";

    #[test]
    fn startup_fails_over_to_the_next_pcscf_in_the_same_process() {
        // US1 / R8: a refused A is marked and B is tried in the same call.
        let (_b, port) = a_refuses_b_listens();
        let mut pool = Pool::new(lo(1), vec![lo(1), lo(2)], None);
        let tried = RefCell::new(Vec::new());
        let waited = RefCell::new(Vec::new());
        let session = register_via_pool(
            &mut pool,
            &pcscf_cfg(lo(1), port, true),
            transport_only_register(&tried),
            |_| true,
            || None,
            |d| waited.borrow_mut().push(d),
        );
        assert!(session.is_ok());
        assert_eq!(*tried.borrow(), vec![lo(1), lo(2)]);
        assert!(pool.is_marked(lo(1), SystemTime::now()));
        assert_eq!(pool.snapshot(SystemTime::now()).active, Some(lo(2)));
        assert!(waited.borrow().is_empty());
    }

    #[test]
    fn startup_fails_over_after_an_unanswered_register() {
        let silent = std::net::UdpSocket::bind((lo(1), 0)).unwrap();
        let port = silent.local_addr().unwrap().port();
        let _b = std::net::UdpSocket::bind((lo(2), port)).unwrap();
        let mut pool = Pool::new(lo(1), vec![lo(1), lo(2)], None);
        let tried = RefCell::new(Vec::new());
        let mut first = true;
        let mut register = transport_only_register(&tried);
        let result = register_via_pool(
            &mut pool,
            &pcscf_cfg(lo(1), port, false),
            |cfg| {
                if std::mem::take(&mut first) {
                    register(cfg)
                } else {
                    tried.borrow_mut().push(cfg.pcscf_addr);
                    Ok(crate::ims::test_session_with_sa(
                        None,
                        SocketAddr::new(cfg.pcscf_addr, port),
                        None,
                    ))
                }
            },
            |_| true,
            || None,
            |_| {},
        );
        assert!(result.is_ok());
        assert_eq!(*tried.borrow(), vec![lo(1), lo(2)]);
    }

    #[test]
    fn startup_without_a_route_marks_nothing_and_exits_as_before() {
        // FR-009 / edge case: a failure with the tunnel gone is the tunnel's.
        let (_b, port) = a_refuses_b_listens();
        let mut pool = Pool::new(lo(1), vec![lo(1), lo(2)], None);
        let tried = RefCell::new(Vec::new());
        let result = register_via_pool(
            &mut pool,
            &pcscf_cfg(lo(1), port, true),
            transport_only_register(&tried),
            |_| false,
            || None,
            |_| {},
        );
        assert!(result.is_err());
        assert_eq!(*tried.borrow(), vec![lo(1)]);
        assert!(!pool.is_marked(lo(1), SystemTime::now()));
    }

    #[test]
    fn startup_returns_a_non_pcscf_failure_unchanged() {
        let mut pool = Pool::new(lo(1), vec![lo(1), lo(2)], None);
        let mut calls = 0;
        let result = register_via_pool(
            &mut pool,
            &pcscf_cfg(lo(1), 5060, true),
            |_| {
                calls += 1;
                Err(rejected(403, None))
            },
            |_| true,
            || None,
            |_| {},
        );
        assert_eq!(result.err().and_then(|f| f.status), Some(403));
        assert_eq!(calls, 1);
        assert!(!pool.is_marked(lo(1), SystemTime::now()));
    }

    #[test]
    fn startup_with_every_pcscf_marked_waits_then_tries_once() {
        // R8 / SC-003: a restart inside the marks' window cannot outrun the
        // ladder.
        for (retry_after, want) in [(30, 30), (600, 120)] {
            let mut pool = Pool::new(lo(1), vec![lo(1), lo(2)], None);
            let now = SystemTime::now();
            pool.select(now);
            pool.on_failure(
                lo(1),
                PoolCause::RetryAfter(Duration::from_secs(retry_after)),
                now,
            );
            pool.select(now);
            pool.on_failure(lo(2), PoolCause::RetryAfter(Duration::from_secs(5000)), now);
            let waited = RefCell::new(Vec::new());
            let mut calls = 0;
            let result = register_via_pool(
                &mut pool,
                &pcscf_cfg(lo(1), 5060, true),
                |_| {
                    calls += 1;
                    Err(io_failure(REFUSED))
                },
                |_| true,
                || None,
                |d| waited.borrow_mut().push(d),
            );
            assert!(result.is_err());
            assert_eq!(calls, 1, "one attempt after the wait");
            // Waited in steps, rechecking for an outage between them.
            let w: Duration = waited.borrow().iter().sum();
            assert!(
                w <= Duration::from_secs(want)
                    && w + Duration::from_secs(2) >= Duration::from_secs(want),
                "{retry_after}s mark waited {w:?}"
            );
            assert!(waited.borrow().iter().all(|d| *d <= OUTAGE_RECHECK));
        }
    }

    #[test]
    fn marks_set_during_an_outage_the_path_came_back_from_do_not_hold_startup() {
        // specs/100 FR-011: the agent restarted while the path was down and
        // marked the only P-CSCF; the ePDG handed the same one back.
        let mut pool = Pool::new(lo(1), vec![lo(1)], None);
        let marked_at = SystemTime::now() - Duration::from_secs(30);
        pool.select(marked_at);
        pool.on_failure(lo(1), PoolCause::GmKeepalive, marked_at);
        let secs = |t: SystemTime| t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let outage = crate::ims::pcscf_files::Outage {
            from: secs(marked_at) - 60,
            to: secs(SystemTime::now()),
        };
        let waited = RefCell::new(Vec::new());
        let mut tried = Vec::new();
        let result = register_via_pool(
            &mut pool,
            &pcscf_cfg(lo(1), 5060, true),
            |cfg| {
                tried.push(cfg.pcscf_addr);
                Ok(crate::ims::test_session_with_sa(
                    None,
                    SocketAddr::new(cfg.pcscf_addr, 5060),
                    None,
                ))
            },
            |_| true,
            || Some(outage),
            |d| waited.borrow_mut().push(d),
        );
        assert!(result.is_ok());
        assert_eq!(tried, vec![lo(1)]);
        assert!(waited.borrow().is_empty(), "no wait on an outage's mark");
    }

    #[test]
    fn a_startup_wait_ends_once_the_path_it_was_marked_across_comes_back() {
        // Waiting on a mark when the hold ends: the next recheck drops it.
        let mut pool = Pool::new(lo(1), vec![lo(1)], None);
        let marked_at = SystemTime::now();
        pool.select(marked_at);
        pool.on_failure(lo(1), PoolCause::GmKeepalive, marked_at);
        let secs = |t: SystemTime| t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let checks = std::cell::Cell::new(0);
        let waited = RefCell::new(Vec::new());
        let result = register_via_pool(
            &mut pool,
            &pcscf_cfg(lo(1), 5060, true),
            |cfg| {
                Ok(crate::ims::test_session_with_sa(
                    None,
                    SocketAddr::new(cfg.pcscf_addr, 5060),
                    None,
                ))
            },
            |_| true,
            // No outage on record until the third look.
            || {
                checks.set(checks.get() + 1);
                (checks.get() >= 3).then(|| crate::ims::pcscf_files::Outage {
                    from: secs(marked_at) - 1,
                    to: secs(marked_at) + 1,
                })
            },
            |d| waited.borrow_mut().push(d),
        );
        assert!(result.is_ok());
        assert_eq!(*waited.borrow(), vec![OUTAGE_RECHECK; 2]);
    }

    #[test]
    fn a_mark_from_before_the_outage_still_waits_its_time() {
        // A P-CSCF that failed while the path was fine is still avoided.
        let mut pool = Pool::new(lo(1), vec![lo(1)], None);
        let marked_at = SystemTime::now();
        pool.select(marked_at);
        pool.on_failure(
            lo(1),
            PoolCause::RetryAfter(Duration::from_secs(30)),
            marked_at,
        );
        let secs = |t: SystemTime| t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let waited = RefCell::new(Vec::new());
        let _ = register_via_pool(
            &mut pool,
            &pcscf_cfg(lo(1), 5060, true),
            |_| Err(io_failure(REFUSED)),
            |_| true,
            || {
                Some(crate::ims::pcscf_files::Outage {
                    from: secs(marked_at) + 10,
                    to: secs(marked_at) + 60,
                })
            },
            |d| waited.borrow_mut().push(d),
        );
        let total: Duration = waited.borrow().iter().sum();
        assert!(
            total + Duration::from_secs(2) >= Duration::from_secs(30),
            "{total:?}"
        );
    }

    fn failing_over_loop() -> LoopState {
        let mut st = LoopState::new();
        st.pool = Pool::new(lo(1), vec![lo(1), lo(2)], None);
        st.pool.select(SystemTime::now());
        st
    }

    #[test]
    fn a_renewal_failure_of_the_pcscf_fails_over_at_once() {
        // US1 AS1–AS3 / R7: marked, counted, no backoff step, next tick → B.
        for failure in [
            io_failure(REFUSED),
            io_failure("timed out waiting for a SIP response"),
            rejected(503, Some(600)),
        ] {
            let mut st = failing_over_loop();
            st.on_renewal_failure(lo(1), &failure, true, SystemTime::now())
                .unwrap();
            assert!(
                st.pool.is_marked(lo(1), SystemTime::now()),
                "{}",
                failure.error
            );
            assert_eq!(st.next_renewal_attempt, None);
            assert!(st.force_renewal);
            assert_eq!(st.consecutive_renewal_failures, 1);
            assert_eq!(st.backoff, RETRY_INITIAL_BACKOFF);
            let next = st.pool.select(SystemTime::now());
            assert_eq!((next.addr, next.reason), (lo(2), Reason::Failover));
        }
    }

    #[test]
    fn a_503_retry_after_marks_for_exactly_that_long() {
        let mut st = failing_over_loop();
        let now = SystemTime::now();
        st.on_renewal_failure(lo(1), &rejected(503, Some(600)), true, now)
            .unwrap();
        let snap = st.pool.snapshot(now);
        assert_eq!(
            snap.unavailable,
            vec![(lo(1), now + Duration::from_secs(600))]
        );
    }

    #[test]
    fn failures_that_are_not_the_pcscfs_take_todays_ladder() {
        // US2 AS1/AS4, R12, clarification Q2.
        let unreachable =
            "TCP connect to 127.0.0.1:5060 failed: Network is unreachable (os error 101)";
        let cases: Vec<(RegisterFailure, bool)> = vec![
            (io_failure(unreachable), true),
            (io_failure(REFUSED), false),
            (io_failure("timed out waiting for a SIP response"), false),
            (rejected(403, None), true),
            (rejected(503, None), true),
            (rejected(503, Some(60)), true),
            (
                io_failure("connection closed by peer with no data (0 bytes read)"),
                true,
            ),
            (io_failure("AT+CSIM failed: 0"), true),
        ];
        for (failure, routed) in cases {
            let mut st = failing_over_loop();
            st.on_renewal_failure(lo(1), &failure, routed, SystemTime::now())
                .unwrap();
            assert!(
                !st.pool.is_marked(lo(1), SystemTime::now()),
                "{} (routed={routed}) must not mark",
                failure.error
            );
            assert!(st.next_renewal_attempt.is_some(), "{}", failure.error);
            assert_eq!(
                st.backoff,
                next_backoff(RETRY_INITIAL_BACKOFF, RETRY_MAX_BACKOFF)
            );
            assert_eq!(st.pool.select(SystemTime::now()).addr, lo(1));
        }
    }

    #[test]
    fn a_reg_event_rejection_marks_no_pcscf() {
        // US2 AS5.
        let mut st = failing_over_loop();
        let d = dereg(DeregEvent::Rejected);
        st.on_network_deregistration(Instant::now(), false, &d, Duration::from_secs(60))
            .unwrap();
        assert!(!st.pool.is_marked(lo(1), SystemTime::now()));
    }

    #[test]
    fn with_every_pcscf_dead_the_ladder_runs_and_the_budget_still_ends_it() {
        // US3 AS1 / SC-003: A then B at once, then the ladder only, each step
        // at the soonest-expiring mark, and the existing exit.
        let mut st = failing_over_loop();
        let now = SystemTime::now();
        st.on_renewal_failure(lo(1), &io_failure(REFUSED), true, now)
            .unwrap();
        assert_eq!(st.next_renewal_attempt, None, "A→B is immediate");
        let b = st.pool.select(now);
        assert_eq!(b.addr, lo(2));
        st.on_renewal_failure(lo(2), &io_failure(REFUSED), true, now)
            .unwrap();
        assert!(
            st.next_renewal_attempt.is_some(),
            "B's failure takes the ladder"
        );
        let mut ladder = vec![RETRY_INITIAL_BACKOFF];
        let mut outcome = Ok(());
        while outcome.is_ok() {
            let target = st.pool.select(now);
            assert_eq!(target.reason, Reason::AllUnavailable);
            ladder.push(st.backoff);
            outcome = st.on_renewal_failure(target.addr, &io_failure(REFUSED), true, now);
            if outcome.is_ok() {
                assert!(st.next_renewal_attempt.is_some(), "never immediate again");
            }
        }
        assert_eq!(
            st.consecutive_renewal_failures,
            MAX_CONSECUTIVE_RENEWAL_FAILURES
        );
        assert!(outcome
            .unwrap_err()
            .to_string()
            .contains("exiting so the supervisor"));
        assert!(
            ladder
                .windows(2)
                .all(|w| w[1] == next_backoff(w[0], RETRY_MAX_BACKOFF)),
            "every step after A→B is a ladder step: {ladder:?}"
        );
    }

    #[test]
    fn a_single_configured_pcscf_is_marked_and_keeps_todays_ladder() {
        // US3 AS2 / R11: VoLTE, or a line with no published pool.
        let mut st = LoopState::new();
        st.pool = Pool::new(lo(1), Vec::new(), None);
        st.pool.select(SystemTime::now());
        st.on_renewal_failure(lo(1), &io_failure(REFUSED), true, SystemTime::now())
            .unwrap();
        assert!(st.pool.is_marked(lo(1), SystemTime::now()));
        assert!(st.next_renewal_attempt.is_some());
        assert!(!st.force_renewal);
        assert_eq!(st.pool.select(SystemTime::now()).addr, lo(1));
    }

    #[test]
    fn the_live_registrations_failures_mark_the_active_pcscf_once() {
        // FR-007: spec 090's endpoint failure and the Gm keepalive escalation
        // both mark the active P-CSCF; one episode counts once.
        let mut st = failing_over_loop();
        st.mark_session_pcscf(lo(1), PoolCause::GmKeepalive, 5060);
        assert!(st.pool.is_marked(lo(1), SystemTime::now()));
        st.mark_session_pcscf(
            lo(1),
            PoolCause::EndpointFailure("subscribe-timeout".into()),
            5060,
        );
        let snap = st.pool.snapshot(SystemTime::now());
        assert_eq!(snap.unavailable.len(), 1);
        let until = snap.unavailable[0].1;
        let left = until.duration_since(SystemTime::now()).unwrap();
        assert!(left <= Duration::from_secs(330), "counted once: {left:?}");
        assert_eq!(st.pool.select(SystemTime::now()).addr, lo(2));
    }

    #[test]
    fn an_old_sessions_failure_never_marks_the_next_candidate() {
        // Greptile (PR #130): a renewal through B failed without marking it,
        // so the loop is still on A's session while the pool names B. A's
        // keepalive escalation must not mark B.
        let mut st = failing_over_loop();
        st.on_renewal_failure(lo(1), &io_failure(REFUSED), true, SystemTime::now())
            .unwrap();
        assert_eq!(st.pool.select(SystemTime::now()).addr, lo(2));
        st.on_renewal_failure(lo(2), &rejected(403, None), true, SystemTime::now())
            .unwrap();
        st.mark_session_pcscf(lo(1), PoolCause::GmKeepalive, 5060);
        assert!(!st.pool.is_marked(lo(2), SystemTime::now()));
    }

    #[test]
    fn one_lines_failover_never_touches_anothers_state() {
        // FR-018: per-line pool and state file.
        let dir = std::env::temp_dir().join(format!("pcscf-lines-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut line0 = LoopState::new();
        line0.pool = Pool::new(lo(1), vec![lo(1), lo(2)], Some(dir.join("pcscf-0.state")));
        line0.pool.select(SystemTime::now());
        let mut line1 = LoopState::new();
        line1.pool = Pool::new(lo(1), vec![lo(1), lo(2)], Some(dir.join("pcscf-1.state")));
        line1.pool.select(SystemTime::now());
        line0
            .on_renewal_failure(lo(1), &io_failure(REFUSED), true, SystemTime::now())
            .unwrap();
        assert!(line0.pool.is_marked(lo(1), SystemTime::now()));
        assert!(!line1.pool.is_marked(lo(1), SystemTime::now()));
        assert!(!dir.join("pcscf-1.state").exists());
        let reloaded = Pool::new(lo(1), vec![lo(1), lo(2)], Some(dir.join("pcscf-1.state")));
        assert!(!reloaded.is_marked(lo(1), SystemTime::now()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn registered_status() -> Mutex<crate::ims::RegistrationStatus> {
        Mutex::new(crate::ims::RegistrationStatus {
            state: crate::ims::RegistrationState::Registered,
            ..Default::default()
        })
    }

    /// A loop whose current subscription is `sub-a` (our tag `ltag-a`), after
    /// an earlier `sub-old` (`ltag-old`) it replaced.
    fn subscribed_loop(now: Instant) -> LoopState {
        let mut st = LoopState::new();
        st.reg_sub.on_new_registration(now);
        let old = SentSubscribe {
            call_id: "sub-old".into(),
            local_tag: "ltag-old".into(),
            cseq: 3,
            expires: 3600,
        };
        st.reg_sub.on_sent(&old, SubKind::Initial, now);
        st.reg_sub.on_new_registration(now);
        let current = SentSubscribe {
            call_id: "sub-a".into(),
            local_tag: "ltag-a".into(),
            cseq: 7,
            expires: 3600,
        };
        st.reg_sub.on_sent(&current, SubKind::Initial, now);
        st
    }

    fn carrier_response(status: u16, cseq: &str) -> SipResponse {
        SipResponse {
            status,
            reason: "OK".into(),
            headers: vec![
                ("Call-ID".into(), "sub-gone".into()),
                ("CSeq".into(), cseq.into()),
            ],
            body: String::new(),
        }
    }

    /// Research R3: every SUBSCRIBE response is the subscription's, even an
    /// unknown one whose CSeq number equals a keepalive's — it never reaches
    /// the keepalive's CSeq-only matcher. An OPTIONS that is not the probe
    /// is left for the keepalive.
    #[test]
    fn a_subscribe_response_never_reaches_the_keepalive_matcher() {
        let now = Instant::now();
        let mut st = subscribed_loop(now);
        assert!(st.take_reg_sub_response(&carrier_response(200, "9 SUBSCRIBE"), now));
        assert!(!st.take_reg_sub_response(&carrier_response(200, "9 OPTIONS"), now));
        assert!(!st.take_reg_sub_response(&carrier_response(200, "9 INVITE"), now));
    }

    #[test]
    fn a_qualifying_endpoint_failure_forces_a_re_registration_past_the_ladder() {
        let mut st = LoopState::new();
        // A rebuild ladder part-way through (clarification Q1 edge case).
        st.reconnect_attempts = 2;
        let status = registered_status();
        assert!(st.on_qualifying_endpoint_failure(
            EndpointFailureCause::SubscribeTimeout,
            Some("sub-a"),
            &status
        ));
        assert!(
            st.force_renewal,
            "probe_gm_connection returns early from here on"
        );
        assert_eq!(st.reconnect_attempts, 0);
        assert!(matches!(
            st.gm_conn,
            crate::ims::GmConnectionState::Failed { .. }
        ));
        let guard = status.lock().unwrap();
        assert_eq!(guard.state, crate::ims::RegistrationState::Failed);
        assert_eq!(
            guard.last_failure.as_ref().map(|(_, r)| r.as_str()),
            Some(QUALIFYING_FAILURE_REASON)
        );
    }

    /// FR-022: mid-call the failure is recorded at once, the call is left
    /// alone, and the re-registration waits for it.
    #[test]
    fn a_qualifying_endpoint_failure_mid_call_keeps_the_call_and_defers_renewal() {
        let mut st = LoopState::new();
        st.active_call = Some(test_active_call(
            "c1",
            "our-to-tag",
            REQUEST_HELPER_CALLER_FROM,
        ));
        let status = registered_status();
        assert!(st.on_qualifying_endpoint_failure(
            EndpointFailureCause::SubscribeTimeout,
            None,
            &status
        ));
        assert!(st.active_call.is_some());
        assert_eq!(
            status.lock().unwrap().state,
            crate::ims::RegistrationState::Failed
        );
        assert_eq!(
            st.maintenance.decide(Maintenance::Renewal, st.busy()),
            MaintenanceDecision::Defer
        );
    }

    /// Edge case: an escalation already pending is not stacked.
    #[test]
    fn a_qualifying_endpoint_failure_with_a_re_registration_pending_changes_nothing() {
        let mut st = LoopState::new();
        st.force_renewal = true;
        let status = registered_status();
        assert!(!st.on_qualifying_endpoint_failure(
            EndpointFailureCause::SubscribeTimeout,
            None,
            &status
        ));
        assert_eq!(
            status.lock().unwrap().state,
            crate::ims::RegistrationState::Registered
        );
        assert!(st.gm_conn.is_up());
    }

    /// Clarification Q3: the confirming probe goes out mid-call, straight
    /// from the subscription, not gated on `busy()`.
    #[test]
    fn the_confirming_probe_is_sent_mid_call() {
        let peer = peer();
        let mut session = session_to(&peer);
        let mut st = LoopState::new();
        st.active_call = Some(test_active_call(
            "c1",
            "our-to-tag",
            REQUEST_HELPER_CALLER_FROM,
        ));
        let status = registered_status();
        let escalated = st.apply_sub_action(
            &mut session,
            "3GPP-WLAN",
            &status,
            SubAction::Probe {
                cause: EndpointFailureCause::SubscribeTimeout,
                call_id: Some("sub-a".into()),
            },
        );
        assert!(!escalated);
        let sent = recv_response(&peer);
        assert!(sent.starts_with("OPTIONS "), "{sent}");
    }

    #[test]
    fn an_initial_subscribe_is_sent_and_tracked() {
        let peer = peer();
        let mut session = session_to(&peer);
        let mut st = LoopState::new();
        let first = st.reg_sub.on_new_registration(Instant::now());
        st.apply_sub_action(&mut session, "3GPP-WLAN", &registered_status(), first);
        let sent = recv_response(&peer);
        assert!(sent.starts_with("SUBSCRIBE "), "{sent}");
        let (call_id, tag) = st.reg_sub.current_ids().expect("tracked");
        assert!(sent.contains(&format!("Call-ID: {call_id}\r\n")));
        assert!(sent.contains(&format!(";tag={tag}\r\n")));
    }

    /// US1 scenario 2 end to end: with no transport the SUBSCRIBE cannot be
    /// sent, nor can the probe, and the line re-registers.
    #[test]
    fn an_unsendable_subscribe_and_probe_escalate_to_re_registration() {
        let mut session =
            crate::ims::test_session_with_sa(None, "127.0.0.1:5060".parse().unwrap(), None);
        let mut st = LoopState::new();
        let status = registered_status();
        let first = st.reg_sub.on_new_registration(Instant::now());
        assert!(st.apply_sub_action(&mut session, "3GPP-WLAN", &status, first));
        assert!(st.force_renewal);
        assert_eq!(
            status.lock().unwrap().state,
            crate::ims::RegistrationState::Failed
        );
    }

    fn reg_notify(
        call_id: &str,
        to_tag: &str,
        event: &str,
        sub_state: Option<&str>,
        body: &str,
    ) -> SipRequest {
        let sub_state = sub_state
            .map(|v| format!("Subscription-State: {v}\r\n"))
            .unwrap_or_default();
        let raw = format!(
            "NOTIFY sip:x@127.0.0.1:5063 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKn1\r\n\
             From: <sip:+919000000000@ims.example.test>;tag=rtag-1\r\n\
             To: <sip:+919000000000@ims.example.test>;tag={to_tag}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: 1 NOTIFY\r\n\
             Event: {event}\r\n\
             {sub_state}Content-Type: application/reginfo+xml\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
    }

    /// Our binding (the test session's contact, `127.0.0.1:5063`) reported
    /// `terminated` with `event`.
    fn our_binding(event: &str) -> String {
        format!(
            r#"<reginfo xmlns="urn:ietf:params:xml:ns:reginfo" version="1" state="full">
  <registration aor="sip:+919000000000@ims.example.test" id="a1" state="active">
    <contact id="c1" state="terminated" event="{event}">
      <uri>sip:+919000000000@127.0.0.1:5063</uri>
    </contact>
  </registration>
</reginfo>"#
        )
    }

    fn status_line(peer: &std::net::UdpSocket) -> String {
        recv_response(peer)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string()
    }

    /// US3 scenario 1: a current NOTIFY is answered `200` and evaluated.
    #[test]
    fn a_current_notify_is_answered_200_and_evaluated() {
        let now = Instant::now();
        let mut st = subscribed_loop(now);
        let session =
            crate::ims::test_session_with_sa(None, "127.0.0.1:5060".parse().unwrap(), None);
        let (sink, peer) = udp_sink_pair();
        let req = reg_notify(
            "sub-a",
            "ltag-a",
            "reg",
            Some("active;expires=3600"),
            &our_binding("deactivated"),
        );
        let (dereg, state) = st
            .answer_reg_notify(&session, &req, &sink, now)
            .expect("current");
        assert_eq!(status_line(&peer), "SIP/2.0 200 OK");
        assert_eq!(dereg.map(|d| d.event), Some(DeregEvent::Deactivated));
        assert_eq!(state.state, crate::ims::session::SubStateKind::Active);
    }

    /// US3 scenario 2: a superseded NOTIFY is acknowledged, never applied —
    /// even one reporting our binding rejected.
    #[test]
    fn a_superseded_notify_is_answered_200_and_not_applied() {
        let now = Instant::now();
        let mut st = subscribed_loop(now);
        let session =
            crate::ims::test_session_with_sa(None, "127.0.0.1:5060".parse().unwrap(), None);
        let (sink, peer) = udp_sink_pair();
        let req = reg_notify(
            "sub-old",
            "ltag-old",
            "reg",
            Some("active"),
            &our_binding("rejected"),
        );
        assert!(st.answer_reg_notify(&session, &req, &sink, now).is_none());
        assert_eq!(status_line(&peer), "SIP/2.0 200 OK");
        assert!(!st.force_renewal);
        assert_eq!(st.rejection, None);
    }

    /// US3 scenario 3, and a package we never subscribed to.
    #[test]
    fn a_notify_for_no_subscription_of_ours_is_refused_481() {
        let now = Instant::now();
        let session =
            crate::ims::test_session_with_sa(None, "127.0.0.1:5060".parse().unwrap(), None);
        for req in [
            reg_notify("stray", "ltag-a", "reg", Some("active"), ""),
            reg_notify("sub-a", "ltag-a", "presence", Some("active"), ""),
        ] {
            let mut st = subscribed_loop(now);
            let (sink, peer) = udp_sink_pair();
            assert!(st.answer_reg_notify(&session, &req, &sink, now).is_none());
            assert_eq!(
                status_line(&peer),
                "SIP/2.0 481 Call/Transaction Does Not Exist"
            );
            assert!(!st.force_renewal);
        }
    }

    #[test]
    fn a_current_notify_without_subscription_state_is_treated_as_active() {
        let now = Instant::now();
        let mut st = subscribed_loop(now);
        let session =
            crate::ims::test_session_with_sa(None, "127.0.0.1:5060".parse().unwrap(), None);
        let (sink, peer) = udp_sink_pair();
        let req = reg_notify("sub-a", "ltag-a", "reg", None, "");
        let (_, state) = st
            .answer_reg_notify(&session, &req, &sink, now)
            .expect("current");
        assert_eq!(status_line(&peer), "SIP/2.0 200 OK");
        assert_eq!(state.state, crate::ims::session::SubStateKind::Missing);
        st.reg_sub.apply_subscription_state(&state, now);
        assert_eq!(st.reg_sub.current_ids(), Some(("sub-a", "ltag-a")));
    }
}
