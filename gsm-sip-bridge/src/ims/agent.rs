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
    map_registration_status_code, next_backoff, respond, restart_client_reader, restart_gm_server,
    start_inbound, subscribe_reg_event, to_unix, Inbound,
};
use crate::ims::sip_client::{
    build_100_trying, build_180_ringing, build_200_ok_bye, build_200_ok_invite,
    build_200_ok_message, build_486_busy_here, build_bye, build_uas_response, format_sip_addr,
    random_hex, ByeRequest, SipMessage, SipRequest, SipResponse, SipSink,
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

/// How often, while idle, an `OPTIONS` keepalive probes the Gm client
/// connection for liveness. A silently-reset connection is otherwise not
/// noticed until the next scheduled renewal (~55 min) or an attempted call
/// fails mid-way. 120s bounds the worst-case dead-line duration to ~130s
/// (this interval + `PING_RESPONSE_TIMEOUT`) at ~30 exchanges/line/hour —
/// negligible against an hour-long registration. See
/// specs/028-gm-tcp-reconnect (clarification Q2, R10).
const PING_INTERVAL: Duration = Duration::from_secs(120);
/// How long to wait for the `OPTIONS` response before scoring the ping — and
/// thus the connection — dead. Generous against a P-CSCF's normal response
/// time, and 12× inside `PING_INTERVAL`. The unanswered-ping case is the one
/// that catches a blackholed connection, where the `send` itself still
/// succeeds (R2).
const PING_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Consecutive failed transport rebuilds before escalating to a full
/// re-registration. Three failures is strong evidence the layer underneath
/// (the Gm IPsec SA) is the problem, which a bare TCP rebind cannot fix —
/// only a re-registration renegotiates a fresh SA (R6).
const MAX_RECONNECT_ATTEMPTS: u32 = 3;

/// The `OPTIONS` keepalive currently in flight on the Gm client connection,
/// if any. See specs/028-gm-tcp-reconnect R1: the ping is sent
/// fire-and-forget and its response is correlated by `CSeq`, because the
/// reader thread owns the read half of the socket — a second reader
/// (`send_and_recv`) would race it and corrupt SIP framing.
#[derive(Debug, Clone, Copy)]
struct PendingPing {
    /// The `CSeq` number the `OPTIONS` went out with — the correlation key.
    cseq: u32,
    /// When it was sent, for the `PING_RESPONSE_TIMEOUT` deadline.
    sent_at: Instant,
}

/// Liveness-probe state for one line's Gm client connection, living on
/// `dispatch_loop`'s stack. At most one ping is in flight at a time.
#[derive(Debug, Default)]
struct PingState {
    /// When the last ping was sent, driving the `PING_INTERVAL`.
    last_sent: Option<Instant>,
    /// The unanswered ping, if one is outstanding.
    pending: Option<PendingPing>,
}

/// What the idle-poll branch should do about liveness this iteration. Kept a
/// pure function of state + `now` + whether a call is up, so it is unit
/// testable without a socket (R12).
#[derive(Debug, PartialEq, Eq)]
enum PingVerdict {
    /// Do nothing this iteration — a call is in progress (it proves liveness
    /// by itself, R10), or the interval hasn't elapsed and nothing is pending.
    Idle,
    /// No ping is pending and the interval has elapsed (or none was ever
    /// sent): send one now.
    Send,
    /// A ping is pending and still within its response deadline: keep waiting.
    Await,
    /// A ping is pending and past its deadline: the connection is dead.
    Dead,
}

impl PingState {
    /// Decide what to do about the liveness probe this iteration. Pure: no I/O,
    /// takes `now` so tests never sleep.
    fn verdict(&self, now: Instant, call_in_progress: bool) -> PingVerdict {
        if call_in_progress {
            return PingVerdict::Idle;
        }
        match self.pending {
            Some(p) => {
                if now.duration_since(p.sent_at) >= PING_RESPONSE_TIMEOUT {
                    PingVerdict::Dead
                } else {
                    PingVerdict::Await
                }
            }
            None => match self.last_sent {
                Some(t) if now.duration_since(t) < PING_INTERVAL => PingVerdict::Idle,
                _ => PingVerdict::Send,
            },
        }
    }

    /// Record that a ping with `cseq` just went out at `now`.
    fn on_sent(&mut self, cseq: u32, now: Instant) {
        self.last_sent = Some(now);
        self.pending = Some(PendingPing { cseq, sent_at: now });
    }

    /// A response arrived. Returns `true` if it answers the pending ping
    /// (clearing it); `false` if it doesn't match and should be ignored — a
    /// late response to a superseded ping must not revive a dead connection.
    fn on_response(&mut self, cseq: u32) -> bool {
        match self.pending {
            Some(p) if p.cseq == cseq => {
                self.pending = None;
                true
            }
            _ => false,
        }
    }

    /// Drop any in-flight ping. Called when the session (and thus the
    /// transport the ping referenced) is replaced, so a stale `CSeq` can't be
    /// scored as a failure against the fresh connection (R11).
    fn reset(&mut self) {
        self.last_sent = None;
        self.pending = None;
    }
}

/// Extract the numeric part of a `CSeq` header value (`"5 OPTIONS"` → `5`).
/// Responses echo the request's `CSeq`, so this is how a keepalive answer is
/// matched back to the ping that provoked it.
fn parse_cseq_number(cseq: &str) -> Option<u32> {
    cseq.split_whitespace().next()?.parse().ok()
}

/// When the current Gm failure episode began — carried across
/// `Reconnecting`/`Failed`, restarted at "now" for a connection that was `Up`.
fn gm_episode_since(gm_conn: super::GmConnectionState) -> SystemTime {
    match gm_conn {
        super::GmConnectionState::Reconnecting { since, .. }
        | super::GmConnectionState::Failed { since } => since,
        super::GmConnectionState::Up => SystemTime::now(),
    }
}

/// Rebuild the Gm **client** connection: reconnect the transport on the
/// still-live Gm SA and restart the reader thread that had died with the old
/// socket. Mirrors what `hangup_carrier` does reactively — reused here
/// proactively (specs/028 R6).
fn reconnect_gm_client(
    session: &mut super::RegisteredSession,
    inbound: &Inbound,
) -> BridgeResult<()> {
    session.reconnect_transport()?;
    restart_client_reader(session, inbound)
}

/// One idle-poll pass of Gm connection liveness (specs/028). Probes both
/// halves of the association independently, repairs a detected failure, and —
/// after `MAX_RECONNECT_ATTEMPTS` consecutive failures — sets `*force_renewal`
/// so the caller escalates to a full re-registration.
///
/// Only ever called with no call in progress (the caller gates on it), so the
/// ping verdict is evaluated as not-in-a-call.
fn probe_gm_connection(
    session: &mut super::RegisteredSession,
    inbound: &mut Inbound,
    obs: &observability::AgentObservability,
    ping: &mut PingState,
    gm_conn: &mut super::GmConnectionState,
    reconnect_attempts: &mut u32,
    force_renewal: &mut bool,
) {
    let now = Instant::now();

    // Listener half (R4): its accept loop dying is invisible to the client
    // ping (which only exercises the client connection), so poll it directly.
    let mut listener_restart_failed = false;
    if inbound._server.as_ref().is_some_and(|s| !s.is_alive()) {
        tracing::warn!("Gm server listener accept loop died; restarting");
        match restart_gm_server(session, inbound) {
            Ok(()) => tracing::info!("Gm server listener restarted"),
            Err(e) => {
                tracing::warn!(error = %e, "Gm server listener restart failed");
                listener_restart_failed = true;
            }
        }
    }

    // Client half (R1/R2): OPTIONS keepalive, correlated at the response arm.
    let mut client_down = false;
    match ping.verdict(now, false) {
        PingVerdict::Idle | PingVerdict::Await => {}
        PingVerdict::Send => match session.send_gm_ping() {
            Ok(cseq) => ping.on_sent(cseq, now),
            Err(e) => {
                tracing::warn!(error = %e, "Gm keepalive send failed; client connection is down");
                ping.pending = None;
                client_down = true;
            }
        },
        PingVerdict::Dead => {
            tracing::warn!("Gm keepalive went unanswered; client connection is down");
            ping.pending = None;
            client_down = true;
        }
    }

    if !client_down && !listener_restart_failed {
        return;
    }

    // A repair is needed. Attribute one failure to the current episode and
    // report the connection as reconnecting.
    *reconnect_attempts += 1;
    *gm_conn = super::GmConnectionState::Reconnecting {
        since: gm_episode_since(*gm_conn),
        attempts: *reconnect_attempts,
    };
    obs.set_gm_connection_up(false);

    if *reconnect_attempts >= MAX_RECONNECT_ATTEMPTS {
        // Bare rebuilds haven't helped; the Gm SA underneath is likely gone,
        // and only a re-registration can renegotiate it. Escalate — the caller
        // runs the renewal this same iteration (R6). Drop the ping so the
        // fresh session starts clean.
        tracing::warn!(
            attempts = *reconnect_attempts,
            "Gm reconnect exhausted; escalating to re-registration"
        );
        *force_renewal = true;
        ping.reset();
        return;
    }

    // Only the *client* half is rebuilt here — a failed listener restart is
    // retried on the next poll and, failing that, fixed by the escalation
    // above; tearing down a healthy client transport for it would be wrong.
    if client_down {
        match reconnect_gm_client(session, inbound) {
            // Confirm the rebuilt connection actually carries signaling before
            // reporting it up: send a fresh probe now, and let the response arm
            // flip `gm_conn` to `Up` only when it round-trips (R7). This is
            // what stops a rebuild over a dead SA from reporting a false
            // recovery.
            Ok(()) => match session.send_gm_ping() {
                Ok(cseq) => ping.on_sent(cseq, Instant::now()),
                Err(e) => {
                    tracing::warn!(error = %e, "confirming Gm probe failed to send; will retry")
                }
            },
            Err(e) => tracing::warn!(error = %e, "Gm client reconnect failed; will retry"),
        }
    }
}

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
/// listening for" symptom T072 pass 3 first described.
///
/// **Does not read the response** (specs/029, greptile PR #35): the CANCEL's
/// outcome — a `487` for the INVITE, or a `200` that raced it — arrives on
/// `inbound.rx` and is handled at `dispatch_loop`'s response arm via the
/// `AwaitingCancel` step. The origination is kept alive until then. The old
/// version read the carrier socket directly here, which raced the
/// always-running client-reader thread (research R2): if that thread grabbed
/// the racing `200`, this timed out and sent no ACK/BYE, leaving a phantom
/// carrier leg — the exact failure this whole feature exists to prevent.
///
/// Two triggers reach this now (specs/029): our own carrier-response timeout
/// (as before), and — since the origination wait became interruptible — a
/// caller hanging up mid-ring, which `dispatch_loop` learns about within
/// ~100ms via the pending origination's `ctrl_rx`.
///
/// Returns whether the CANCEL was actually sent.
#[allow(clippy::too_many_arguments)]
fn send_cancel(
    session: &mut super::RegisteredSession,
    callee_uri: &str,
    route_headers: &[String],
    via_transport: &str,
    call_id: &str,
    from_tag: &str,
    invite_cseq: u32,
    invite_branch: &str,
) -> bool {
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
        return false;
    };
    if transport.send(&cancel).is_ok() {
        tracing::info!(call_id, "outbound: sent CANCEL for an abandoned INVITE");
        true
    } else {
        false
    }
}

/// The carrier answered despite our CANCEL — a `200` that raced the `487`. ACK
/// it (reusing the INVITE's branch/CSeq, §17.1.1.3) then immediately BYE, or
/// the carrier leg would stay up with nothing on our end tracking it. Called
/// from the response arm when a `200` arrives in the `AwaitingCancel` step;
/// best-effort, there is nothing to retry from here.
#[allow(clippy::too_many_arguments)]
fn ack_and_bye_racing_answer(
    session: &mut super::RegisteredSession,
    callee_uri: &str,
    route_headers: &[String],
    via_transport: &str,
    call_id: &str,
    from_tag: &str,
    invite_cseq: u32,
    invite_branch: &str,
    resp: &SipResponse,
) {
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

/// An outbound origination in flight, held by `dispatch_loop` across poll
/// ticks so the wait for the carrier (and then Agent B's veth leg) is
/// interruptible (specs/029-interruptible-origination-wait). This replaces the
/// old blocking `originate_and_bridge`, which read the carrier socket directly
/// — racing the always-running client-reader thread (research R2) and wedging
/// the whole loop for up to ~80s (no inbound call answered, no caller hangup
/// observed). Carrier responses now arrive through `inbound.rx` like every
/// other message; this struct is the state the loop advances them against.
struct PendingOrigination {
    step: OriginationStep,
    /// Correlates carrier responses (by `Call-ID`) and Agent B's `CallEnded`
    /// (by `call_id`) to *this* attempt — a message naming another call is
    /// ignored, never acted on (FR-010).
    call_id: String,
    from_tag: String,
    branch: String,
    invite_cseq: u32,
    callee_uri: String,
    route_headers: Vec<String>,
    via_transport: &'static str,
    destination: String,
    /// Connection to Agent B: written to here (`CallRinging`/`CallPlaced`/
    /// `CallFailed`), read from via `ctrl_rx`'s reader thread.
    control: TcpStream,
    /// Spawned in `begin_origination` — *before* the carrier answers, unlike
    /// the old code that only started reading Agent B once a call was fully
    /// bridged — so a caller hangup mid-attempt is observable at all.
    ctrl_rx: mpsc::Receiver<ControlMessage>,
    rtp_socket: UdpSocket,
    veth_local_ip: IpAddr,
    veth_sip_port: u16,
    wideband: bool,
    /// While `AwaitingCarrier`: the carrier-response deadline (initially
    /// `OUTBOUND_INVITE_TIMEOUT`, extended to `OUTBOUND_RING_TIMEOUT` once any
    /// response arrives). While `AwaitingVeth`: the veth-leg deadline
    /// (`VETH_INVITE_TIMEOUT`). One field, reset on the transition.
    deadline: Instant,
    any_response_seen: bool,
    ringing_relayed: bool,
    lifecycle: BridgedCall,
}

/// The waits the old `originate_and_bridge` blocked on, now explicit states.
/// The shared `Awaiting` prefix is deliberate — each is a distinct thing the
/// loop is waiting for.
#[allow(clippy::enum_variant_names)]
enum OriginationStep {
    /// INVITE sent; waiting for the carrier's final response.
    AwaitingCarrier,
    /// Carrier answered `200 OK` and was ACKed; waiting for Agent B's veth leg.
    /// The `VETH_INVITE_TIMEOUT` window, previously a blocking
    /// `veth_rx.recv_timeout`.
    AwaitingVeth {
        dialog: DialogInfo,
        veth_rx: mpsc::Receiver<BridgeResult<VethUasResult>>,
        /// The codec the carrier answered with, carried to `finish_origination`
        /// where the transcoding decision (and the "codec we never offered"
        /// check) is made — kept in the same order as the old blocking path.
        answer_codec: sdp::NegotiatedCodec,
    },
    /// A CANCEL has been sent (abandonment or our own timeout) and we are
    /// waiting for its outcome — a `487` for the INVITE (expected), or a `200`
    /// that raced the CANCEL (the carrier answered anyway; we ACK then BYE).
    /// That outcome arrives on `inbound.rx` like every other response, so it is
    /// handled at the response arm rather than by a direct socket read that
    /// would race the client reader (greptile PR #35 / research R2).
    AwaitingCancel,
}

/// Whether a pending origination is still in flight or has resolved this tick.
enum OriginationStatus {
    /// Keep waiting — the caller retains the `PendingOrigination`.
    Pending,
    /// Resolved (failure, timeout, or abandonment). Any teardown and any
    /// `CallFailed` to Agent B has already been done; the caller drops it.
    Ended,
}

/// Best-effort `CallFailed` to Agent B, factored out of the old `fail` closure.
fn origination_failed(control: &mut TcpStream, call_id: &str, reason: &str) {
    tracing::warn!(call_id, reason, "outbound: could not place carrier call");
    let _ = write_msg(
        control,
        &ControlMessage::CallFailed {
            call_id: call_id.to_string(),
            reason: reason.to_string(),
        },
    );
}

/// Builds and sends the carrier INVITE and returns the in-flight state, or
/// `None` (having told Agent B `CallFailed`) if it could not even be sent. The
/// front half of the old `originate_and_bridge`, up to and including the
/// INVITE — plus spawning the Agent B control reader *now* so a mid-attempt
/// hangup can be heard. `dispatch_loop` has already sent `CallAttempting`.
#[allow(clippy::too_many_arguments)]
fn begin_origination(
    session: &mut super::RegisteredSession,
    mut control: TcpStream,
    call_id: String,
    destination: &str,
    veth_local_ip: IpAddr,
    veth_sip_port: u16,
    wideband: bool,
) -> Option<PendingOrigination> {
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
            origination_failed(
                &mut control,
                &call_id,
                &format!("RTP socket bind failed: {e}"),
            );
            return None;
        }
    };
    let rtp_port = match rtp_socket.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            origination_failed(
                &mut control,
                &call_id,
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
            origination_failed(
                &mut control,
                &call_id,
                &format!("no carrier transport: {e}"),
            );
            return None;
        }
    };
    if let Err(e) = transport.send(&invite) {
        origination_failed(&mut control, &call_id, &format!("INVITE send failed: {e}"));
        return None;
    }

    // Start listening to Agent B now (not after the call is bridged, as the old
    // code did): this is the whole point of specs/029 — a caller hanging up
    // while the carrier is still ringing has to reach `dispatch_loop`, and this
    // reader is the only path for it.
    let ctrl_rx = match control.try_clone() {
        Ok(s) => spawn_control_reader(s),
        Err(e) => {
            origination_failed(
                &mut control,
                &call_id,
                &format!("control connection clone failed: {e}"),
            );
            return None;
        }
    };

    // Lifecycle from the moment the INVITE is sent: `Offered → Answering`. This
    // is also what makes the line read as busy to an inbound INVITE arriving
    // mid-attempt (`Admission::for_current`, FR-011) with no new rule.
    let mut lifecycle = BridgedCall::new(call_id.clone(), destination.to_string(), None);
    lifecycle.advance_to(CallStage::Answering);

    Some(PendingOrigination {
        step: OriginationStep::AwaitingCarrier,
        call_id,
        from_tag,
        branch,
        invite_cseq,
        callee_uri,
        route_headers,
        via_transport,
        destination: destination.to_string(),
        control,
        ctrl_rx,
        rtp_socket,
        veth_local_ip,
        veth_sip_port,
        wideband,
        deadline: Instant::now() + OUTBOUND_INVITE_TIMEOUT,
        any_response_seen: false,
        ringing_relayed: false,
        lifecycle,
    })
}

impl PendingOrigination {
    /// Does this carrier response belong to this attempt? Matched by `Call-ID`,
    /// so it never collides with the Gm keepalive's `OPTIONS` (a different
    /// `Call-ID`, correlated by `CSeq` at the response arm instead).
    fn matches_response(&self, resp: &SipResponse) -> bool {
        resp.header("Call-ID")
            .is_some_and(|id| id.trim() == self.call_id)
    }

    /// Tell Agent B this attempt failed. Best-effort; Agent B may already have
    /// moved on (e.g. it initiated an abandonment).
    fn fail(&mut self, reason: &str) {
        let call_id = self.call_id.clone();
        origination_failed(&mut self.control, &call_id, reason);
    }

    /// Send a CANCEL for a still-pending INVITE and move to `AwaitingCancel`,
    /// where the response arm handles the `487`/racing-`200` via `inbound.rx`.
    /// Valid from `AwaitingCarrier`. The origination is kept alive (a short
    /// `CANCEL_RESPONSE_TIMEOUT` deadline) so a racing answer is still ACKed and
    /// BYE'd rather than leaking (greptile PR #35).
    fn begin_cancel(&mut self, session: &mut super::RegisteredSession) {
        send_cancel(
            session,
            &self.callee_uri,
            &self.route_headers,
            self.via_transport,
            &self.call_id,
            &self.from_tag,
            self.invite_cseq,
            &self.branch,
        );
        self.step = OriginationStep::AwaitingCancel;
        self.deadline = Instant::now() + CANCEL_RESPONSE_TIMEOUT;
    }

    /// Handle a carrier response while awaiting the CANCEL's outcome. A `200`
    /// that raced the CANCEL is ACKed and BYE'd; a `487` (or any other final)
    /// means the transaction is dead. Provisionals are ignored.
    fn on_cancel_response(
        &mut self,
        resp: &SipResponse,
        session: &mut super::RegisteredSession,
    ) -> OriginationStatus {
        if resp.status < 200 {
            return OriginationStatus::Pending;
        }
        if resp.status == 200 {
            ack_and_bye_racing_answer(
                session,
                &self.callee_uri,
                &self.route_headers,
                self.via_transport,
                &self.call_id,
                &self.from_tag,
                self.invite_cseq,
                &self.branch,
                resp,
            );
        }
        OriginationStatus::Ended
    }

    /// Advance on a carrier response delivered via `inbound.rx`. Returns
    /// `Ended` (and has already sent `CallFailed`/ACKed as needed) when the
    /// attempt is resolved as a failure; `Pending` while it is still in flight
    /// (including the `200 OK → AwaitingVeth` transition).
    fn on_carrier_response(
        &mut self,
        resp: &SipResponse,
        session: &mut super::RegisteredSession,
    ) -> OriginationStatus {
        // Only the carrier-wait phase interprets responses as the INVITE's
        // outcome. In the other phases a Call-ID-matched response is not a fresh
        // final to act on (greptile PR #35):
        match self.step {
            OriginationStep::AwaitingCancel => return self.on_cancel_response(resp, session),
            OriginationStep::AwaitingVeth { .. } => {
                // A retransmitted `200` (our ACK was lost/slow) — the final was
                // already handled and the veth leg is being placed. Re-running
                // the answer path would spawn a second veth listener and send
                // `CallPlaced` twice; ignore it, as the old blocking code did
                // (it simply never read the socket again during the veth wait).
                tracing::debug!(
                    call_id = %self.call_id,
                    status = resp.status,
                    "ignoring a carrier response after the final was already handled"
                );
                return OriginationStatus::Pending;
            }
            OriginationStep::AwaitingCarrier => {}
        }

        // First response of any kind (even a provisional) switches from the
        // short "did the network acknowledge us at all" window to the long
        // ring window — the same rule the old blocking origination read
        // applied internally before this became a poll loop.
        if !self.any_response_seen {
            self.any_response_seen = true;
            self.deadline = Instant::now() + OUTBOUND_RING_TIMEOUT;
        }

        if resp.status < 200 {
            tracing::info!(status = resp.status, reason = %resp.reason, "provisional response");
            // Relay ringback exactly once, and advance the lifecycle to
            // `PbxRinging` — the transition the old outbound path skipped, so
            // no successful outbound call ever recorded as reaching `Bridged`
            // (research R5).
            if resp.status == 180 && !self.ringing_relayed {
                self.ringing_relayed = true;
                self.lifecycle.advance_to(CallStage::PbxRinging);
                let _ = write_msg(
                    &mut self.control,
                    &ControlMessage::CallRinging {
                        call_id: self.call_id.clone(),
                    },
                );
            }
            return OriginationStatus::Pending;
        }

        tracing::info!(call_id = %self.call_id, status = resp.status, reason = %resp.reason, "outbound: final INVITE response");

        if resp.status != 200 {
            // Non-2xx final: ACK reuses the INVITE's own branch/CSeq
            // (RFC 3261 §17.1.1.3), best-effort.
            let ack = super::call::build_ack(&super::call::AckParts {
                request_uri: &self.callee_uri,
                route_headers: &self.route_headers,
                via_transport: self.via_transport,
                local_addr: session.local_addr,
                public_uri: &session.public_uri,
                to_header: resp.header("To").unwrap_or(&self.callee_uri),
                call_id: &self.call_id,
                from_tag: &self.from_tag,
                cseq: self.invite_cseq,
                branch: &self.branch,
            });
            let _ = session.transport_mut().and_then(|t| t.send(&ack));
            self.fail(&format!("{} {}", resp.status, resp.reason));
            return OriginationStatus::Ended;
        }

        // 200 OK. Everything from here mirrors the old post-final block, in the
        // same order (SDP → RTP → ACK → dialog → veth listener → CallPlaced),
        // so every teardown path keeps its original meaning.
        let answer = match sdp::parse_answer(&resp.body) {
            Ok(a) => a,
            Err(e) => {
                self.fail(&format!("bad SDP answer: {e}"));
                return OriginationStatus::Ended;
            }
        };
        if let Err(e) = self.rtp_socket.connect(answer.remote_rtp) {
            self.fail(&format!("RTP connect failed: {e}"));
            return OriginationStatus::Ended;
        }

        let ack_branch = format!("z9hG4bK{}", random_hex(6));
        let ack = super::call::build_ack(&super::call::AckParts {
            request_uri: &self.callee_uri,
            route_headers: &self.route_headers,
            via_transport: self.via_transport,
            local_addr: session.local_addr,
            public_uri: &session.public_uri,
            to_header: resp.header("To").unwrap_or(&self.callee_uri),
            call_id: &self.call_id,
            from_tag: &self.from_tag,
            cseq: self.invite_cseq,
            branch: &ack_branch,
        });
        if let Err(e) = session.transport_mut().and_then(|t| t.send(&ack)) {
            self.fail(&format!("ACK send failed: {e}"));
            return OriginationStatus::Ended;
        }
        session.cseq = self.invite_cseq + 1;

        let dialog = DialogInfo::from_uac_response(
            resp,
            self.route_headers.clone(),
            &self.callee_uri,
            &session.public_uri,
            &self.from_tag,
            session.cseq,
            session,
        );

        // Spawn the veth listener *before* telling Agent B to call in — same
        // ordering `handle_invite` uses for the inbound direction, so the
        // listener is guaranteed up by the time Agent B's `Call::make` reaches
        // it.
        let veth_rx = match spawn_veth_uas_listener(
            self.veth_local_ip,
            self.veth_sip_port,
            self.wideband,
        ) {
            Ok(rx) => rx,
            Err(e) => {
                // The carrier leg is already ACKed and up (the `dialog` proves
                // it) — `fail()` alone only tells Agent B, it does not hang up
                // the real carrier call.
                tracing::warn!(call_id = %self.call_id, error = %e, "outbound: veth listener failed");
                let call_id = self.call_id.clone();
                hangup_answered_carrier_leg(
                    session,
                    &mut self.control,
                    &dialog,
                    &call_id,
                    reason::VETH_LEG_FAILED,
                );
                return OriginationStatus::Ended;
            }
        };

        if let Err(e) = write_msg(
            &mut self.control,
            &ControlMessage::CallPlaced {
                call_id: self.call_id.clone(),
            },
        ) {
            // Same leak as above: the carrier leg is already up. Best-effort
            // even though this very write just failed.
            tracing::warn!(call_id = %self.call_id, error = %e, "outbound: failed to notify Agent B the carrier leg is up");
            let call_id = self.call_id.clone();
            hangup_answered_carrier_leg(
                session,
                &mut self.control,
                &dialog,
                &call_id,
                reason::TRANSPORT_ERROR,
            );
            return OriginationStatus::Ended;
        }

        self.step = OriginationStep::AwaitingVeth {
            dialog,
            veth_rx,
            answer_codec: answer.codec,
        };
        self.deadline = Instant::now() + VETH_INVITE_TIMEOUT;
        OriginationStatus::Pending
    }
}

/// Advance a pending origination on a timer tick (no new carrier response):
/// watch Agent B's control channel for a caller hangup, enforce the current
/// deadline, and — once the carrier has answered — pick up Agent B's veth leg.
/// May take `*pending` (leaving it `None`) when the attempt resolves; returns
/// the bridged `ActiveCall` only on the success path.
fn tick_pending_origination(
    pending: &mut Option<PendingOrigination>,
    session: &mut super::RegisteredSession,
) -> Option<ActiveCall> {
    let mut p = pending.take()?;

    // 1. A caller hangup (or Agent B vanishing) abandons the attempt (FR-003).
    //    A `CallEnded` naming a different call is ignored, never acted on
    //    (FR-010).
    let abandoned = match p.ctrl_rx.try_recv() {
        Ok(ControlMessage::CallEnded { call_id, .. }) if call_id == p.call_id => {
            tracing::info!(call_id = %p.call_id, "outbound: caller abandoned the attempt; abandoning the carrier leg");
            true
        }
        Ok(ControlMessage::CallEnded { call_id, .. }) => {
            tracing::warn!(this = %p.call_id, other = %call_id, "ignoring CallEnded for a different call during origination");
            false
        }
        Ok(other) => {
            tracing::debug!(message = ?other, "ignoring control message during origination");
            false
        }
        Err(mpsc::TryRecvError::Disconnected) => {
            tracing::warn!(call_id = %p.call_id, "Agent B control connection dropped during origination; abandoning");
            true
        }
        Err(mpsc::TryRecvError::Empty) => false,
    };
    if abandoned {
        abandon_origination(&mut p, session);
        // If that left us awaiting the CANCEL's outcome, keep the origination
        // alive so a racing `200` is still ACKed and BYE'd (greptile PR #35);
        // otherwise it is fully resolved and dropped.
        if matches!(p.step, OriginationStep::AwaitingCancel) {
            *pending = Some(p);
        }
        return None;
    }

    // 2. Read the veth channel without holding a borrow into `p` across a move.
    let veth_ready = match &p.step {
        OriginationStep::AwaitingVeth { veth_rx, .. } => Some(veth_rx.try_recv()),
        _ => None,
    };
    match veth_ready {
        // Agent B's veth leg arrived — bridge the two legs.
        Some(Ok(veth_result)) => return finish_origination(p, veth_result, session),
        // Nothing yet: enforce the veth deadline, else keep waiting.
        Some(Err(mpsc::TryRecvError::Empty)) => {
            if Instant::now() >= p.deadline {
                tracing::warn!(call_id = %p.call_id, "outbound: Agent B's veth call never arrived");
                hangup_pending_carrier_leg(&mut p, session, reason::VETH_LEG_FAILED);
            } else {
                *pending = Some(p);
            }
            return None;
        }
        Some(Err(mpsc::TryRecvError::Disconnected)) => {
            tracing::warn!(call_id = %p.call_id, "outbound: veth listener stopped before answering");
            hangup_pending_carrier_leg(&mut p, session, reason::VETH_LEG_FAILED);
            return None;
        }
        None => {}
    }

    // 3. Awaiting the carrier or its CANCEL response — enforce the deadline.
    if Instant::now() >= p.deadline {
        match p.step {
            OriginationStep::AwaitingCarrier => {
                // Our own timeout: tell Agent B, then CANCEL and wait out the
                // outcome via `inbound.rx` (AwaitingCancel), so a racing answer
                // is still cleaned up rather than leaking.
                tracing::warn!(call_id = %p.call_id, "outbound: no final response from carrier in time");
                p.fail(&format!(
                    "{}: no final response from carrier",
                    reason::CARRIER_TIMEOUT
                ));
                p.begin_cancel(session);
                *pending = Some(p);
            }
            OriginationStep::AwaitingCancel => {
                // The CANCEL's own response never arrived within the window;
                // stop tracking it. The CANCEL itself already abandoned the
                // carrier's INVITE transaction.
                tracing::debug!(call_id = %p.call_id, "outbound: CANCEL response window elapsed");
            }
            // Unreachable: the veth match above returns for `AwaitingVeth`.
            OriginationStep::AwaitingVeth { .. } => {}
        }
        return None;
    }

    *pending = Some(p);
    None
}

/// Abandon an in-flight attempt because the originating caller is gone. Tears
/// the carrier side down as the current step requires: CANCEL a still-pending
/// INVITE (→ `AwaitingCancel`, kept alive so a racing answer is cleaned up), or
/// BYE an already-answered leg. No `CallFailed` is sent — Agent B initiated
/// this and has already reported `CallerAbandoned`.
fn abandon_origination(p: &mut PendingOrigination, session: &mut super::RegisteredSession) {
    match &p.step {
        OriginationStep::AwaitingCarrier => p.begin_cancel(session),
        OriginationStep::AwaitingVeth { .. } => {
            hangup_pending_carrier_leg(p, session, reason::CALLER_HANGUP);
        }
        // Already cancelling — a duplicate `CallEnded`; nothing more to do.
        OriginationStep::AwaitingCancel => {}
    }
}

/// BYE an already-answered carrier leg for a pending (not-yet-bridged) attempt,
/// reusing the dialog captured at `200 OK`. Only valid in `AwaitingVeth`.
fn hangup_pending_carrier_leg(
    p: &mut PendingOrigination,
    session: &mut super::RegisteredSession,
    reason: &str,
) {
    // Disjoint field borrows: `dialog` reads `p.step`, the BYE writes
    // `p.control`, `call_id` is copied out first — so none of these alias.
    if let OriginationStep::AwaitingVeth { dialog, .. } = &p.step {
        let call_id = p.call_id.clone();
        hangup_answered_carrier_leg(session, &mut p.control, dialog, &call_id, reason);
    }
}

/// Bridge a carrier leg (already answered and ACKed) to Agent B's veth leg — the
/// back half of the old `originate_and_bridge`, run once the veth call arrives.
/// Consumes `p`. Returns the `ActiveCall`, or `None` after tearing the carrier
/// leg down if bridging fails.
fn finish_origination(
    p: PendingOrigination,
    veth_result: BridgeResult<VethUasResult>,
    session: &mut super::RegisteredSession,
) -> Option<ActiveCall> {
    let PendingOrigination {
        step,
        call_id,
        from_tag,
        destination,
        control,
        ctrl_rx,
        rtp_socket,
        mut lifecycle,
        ..
    } = p;
    let mut control = control;
    let OriginationStep::AwaitingVeth {
        dialog,
        answer_codec,
        ..
    } = step
    else {
        // Unreachable: only ever called from the `AwaitingVeth` arm.
        return None;
    };

    let veth = match veth_result.map_err(|e| e.to_string()) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(call_id, error = %e, "outbound: Agent B's veth call failed");
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
    // (`NegotiatedCodec`), not a payload type — by RFC 3264, a re-used dynamic
    // payload type on the answer must mean what *our own offer* said it meant,
    // so there is nothing to re-parse. Reconstructs the rest from what
    // `sdp::build_offer` is known to always send.
    let chosen = match answer_codec {
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
            // Never offered — `sdp::build_offer` only ever lists PCMU/AMR-WB.
            // Agent B's phone/PBX leg is already answered by this point, so
            // leaving it stranded on dead air on top of leaking the carrier leg
            // would compound the failure.
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

    // `PbxRinging → Bridged`. Force `PbxRinging` first for the rare carrier
    // that answers `200` with no `180` in between (`advance_to` refuses the
    // illegal `Answering → Bridged` jump and would otherwise leave the stage
    // stuck) — research R5.
    lifecycle.advance_to(CallStage::PbxRinging);
    lifecycle.advance_to(CallStage::Bridged);

    Some(ActiveCall {
        control,
        ctrl_rx,
        stop,
        // Our own from_tag doubles as `to_tag`: `handle_bye`'s
        // `build_200_ok_bye` only falls back to it when the incoming request's
        // own To header lacks a tag, which a real in-dialog BYE never does.
        to_tag: from_tag,
        dialog,
        call_id,
        caller: destination,
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
    // An outbound call being placed but not yet bridged (specs/029). Held here,
    // not blocked on inside a helper, so the loop keeps answering everything
    // else — inbound INVITEs, a caller hangup, the Gm keepalive — while the
    // carrier is still ringing.
    let mut origination: Option<PendingOrigination> = None;
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
    // specs/028-gm-tcp-reconnect: Gm signaling-connection liveness.
    // `ping` drives the idle OPTIONS keepalive; `reconnect_attempts` counts
    // consecutive repair failures for the current episode (reset on a
    // confirmed recovery); `force_renewal`, once set, makes the next idle
    // iteration escalate to a full re-registration even though the
    // registration is nowhere near expiry — the only thing that can
    // renegotiate a dead Gm SA. `gm_conn` is the reported health, synced into
    // the shared status each poll.
    let mut ping = PingState::default();
    let mut reconnect_attempts: u32 = 0;
    let mut force_renewal = false;
    let mut gm_conn = super::GmConnectionState::Up;
    loop {
        // Keep the shared health inputs the status listener reads current — the
        // busy flag and any deferred maintenance — so a `volte-status` query is
        // answered from the same state the loop is acting on. Cheap: one lock
        // per poll, and the values are eventually consistent within a poll
        // interval regardless.
        {
            let mut guard = status.lock().unwrap_or_else(|e| e.into_inner());
            guard.busy = active_call.is_some() || origination.is_some();
            guard.deferred_maintenance = maintenance.deferred();
            // Reflect the telephone-side half's PBX registration. Absent (Wi-Fi
            // path), the PBX leg is not tracked here, so treat it as available
            // rather than falsely reporting the line unable to answer.
            guard.pbx_registered =
                pbx_registered.is_none_or(|f| f.load(std::sync::atomic::Ordering::SeqCst));
            // Reported Gm connection health (specs/028), kept current for a
            // `vowifi-status`/`volte-status` query the same way `busy` is.
            guard.gm_connection = gm_conn;
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

        // Advance an outbound origination that is mid-flight (specs/029):
        // watch Agent B's control channel for a caller hangup, enforce the
        // current deadline, and pick up Agent B's veth leg once the carrier
        // answers. This does *not* `continue` — the loop must fall through to
        // `inbound.rx` below so an inbound INVITE arriving during the attempt
        // still gets its prompt `486` (FR-011), and carrier responses (which
        // arrive on `inbound.rx`, no longer read here directly) still reach the
        // response arm.
        if origination.is_some() {
            if let Some(call) = tick_pending_origination(&mut origination, session) {
                // The outbound call is now bridged — treat it like any other
                // active call (fresh media baseline so the last call's counts
                // can't read as a stall on this one).
                obs.set_active_calls(1);
                watch = AttachmentWatch::default();
                active_call = Some(call);
            } else if origination.is_none() {
                // Resolved as a failure/abandonment (not bridged, not still
                // pending): a renewal held for it may now run.
                maintenance.release();
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
            if active_call.is_some() || origination.is_some() {
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
                // Send the INVITE and record the attempt as in-flight, rather
                // than blocking here until it resolves (specs/029). The wait —
                // for the carrier's response, then for Agent B's veth leg — is
                // now driven by `tick_pending_origination` and the response arm
                // on the loop's own thread, so an inbound INVITE, a caller
                // hangup, and the Gm keepalive all keep being serviced while
                // the carrier is still ringing. Carrier responses arrive on
                // `inbound.rx` (via the client-reader thread) like everything
                // else — this no longer reads the carrier socket directly, which
                // also removes the two-readers-on-one-socket race
                // (research R2).
                origination = begin_origination(
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
        // up — or a caller hangup / veth leg while an origination is in flight
        // (specs/029, so abandonment is observed within ~100ms rather than up
        // to ~80s); idle otherwise, where the only deadline is registration
        // renewal.
        let poll = if active_call.is_some() || origination.is_some() {
            ACTIVE_CALL_POLL_INTERVAL
        } else {
            IDLE_POLL_INTERVAL
        };
        match inbound.rx.recv_timeout(poll) {
            Ok((SipMessage::Request(req), sink)) if req.method == "INVITE" => {
                // An in-flight outbound origination occupies the line too
                // (specs/029, FR-011): consult whichever lifecycle exists so an
                // inbound INVITE during an attempt is refused `486` at once,
                // through this same path, rather than waiting out the attempt.
                let occupant = active_call
                    .as_ref()
                    .map(|c| &c.lifecycle)
                    .or(origination.as_ref().map(|o| &o.lifecycle));
                if Admission::for_current(occupant) == Admission::RejectBusy {
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
                // Is this a response to an outbound INVITE we are placing
                // (specs/029)? These now arrive here, on `inbound.rx`, instead
                // of being read from the carrier socket directly inside a
                // blocking helper — matched by `Call-ID`, which never collides
                // with the Gm keepalive's `OPTIONS` (correlated by `CSeq`
                // below). A `200 OK` moves the attempt to awaiting the veth leg;
                // a failure clears it.
                if origination
                    .as_ref()
                    .is_some_and(|o| o.matches_response(&resp))
                {
                    if let OriginationStatus::Ended = origination
                        .as_mut()
                        .expect("just matched Some")
                        .on_carrier_response(&resp, session)
                    {
                        origination = None;
                        // A renewal held for the attempt may now run.
                        maintenance.release();
                    }
                    continue;
                }
                // Is this the answer to our Gm keepalive? Any final response
                // counts as proof the client connection carries signaling —
                // even a 4xx/5xx: the question is liveness, not whether the
                // carrier liked the request (specs/028 R1). A matching answer
                // is also what confirms a *reconnect* actually worked, before
                // we report the line healthy (R7).
                let matched = resp
                    .header("CSeq")
                    .and_then(parse_cseq_number)
                    .is_some_and(|n| ping.on_response(n));
                if matched {
                    if !gm_conn.is_up() {
                        tracing::info!("Gm connection liveness confirmed; connection is up");
                    }
                    gm_conn = super::GmConnectionState::Up;
                    reconnect_attempts = 0;
                    obs.set_gm_connection_up(true);
                    continue;
                }
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
                //
                // Gm connection liveness (specs/028) runs here, on the idle
                // path, only when no call is in progress — a live call proves
                // the connection by itself and its own signaling must not be
                // disturbed (FR-006). An in-flight origination counts as a call
                // in progress (specs/029): its INVITE transaction is live on
                // this transport, so a keepalive OPTIONS or a reconnect must not
                // cut across it. Repeated repair failure sets `force_renewal`,
                // which the renewal gate below honours.
                if active_call.is_none() && origination.is_none() {
                    probe_gm_connection(
                        session,
                        inbound,
                        obs,
                        &mut ping,
                        &mut gm_conn,
                        &mut reconnect_attempts,
                        &mut force_renewal,
                    );
                }

                // Never renew mid-call — that would tear down the transport
                // a call's own signaling (e.g. the eventual BYE) still
                // needs; renewal is deferred until the call ends.
                let expires_at = status.lock().unwrap_or_else(|e| e.into_inner()).expires_at;
                let due = expires_at
                    .is_some_and(|e| super::renewal_due(SystemTime::now(), e, RENEWAL_HEADROOM));
                // `force_renewal` is the Gm-liveness escalation: re-register now
                // even though expiry is far off, because only a re-registration
                // can renegotiate a Gm SA that has gone dead underneath the
                // connection (R6). A scheduled renewal (`due`) proceeds as
                // before.
                if !due && !force_renewal {
                    continue;
                }
                // Renewal is genuinely due. Hold it if a call is in progress, or
                // an outbound origination is still being placed (specs/029) —
                // re-registering mid-attempt would replace the transport and the
                // session the pending INVITE's dialog lives on. Recorded by the
                // policy so the deferral is visible in status, and so the model,
                // not an inline `is_some()`, owns the rule.
                if maintenance.decide(
                    Maintenance::Renewal,
                    active_call.is_some() || origination.is_some(),
                ) == MaintenanceDecision::Defer
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
                        // If this renewal was the Gm-liveness escalation, its
                        // failure means the connection is still down and the
                        // heavy remedy didn't take — report Failed, but keep
                        // retrying on backoff (FR-010b: Failed is not terminal).
                        if force_renewal {
                            gm_conn = super::GmConnectionState::Failed {
                                since: gm_episode_since(gm_conn),
                            };
                            obs.set_gm_connection_up(false);
                        }
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
                        // The renewal replaced `session` and `inbound`
                        // wholesale — a fresh Gm SA, transport, and both
                        // readers. Any in-flight ping referenced the old
                        // socket and can never be answered on the new one, so
                        // it must be dropped (R11); the Gm connection is up
                        // again by construction, and the failure episode (if
                        // any) is over.
                        ping.reset();
                        reconnect_attempts = 0;
                        force_renewal = false;
                        gm_conn = super::GmConnectionState::Up;
                        obs.set_gm_connection_up(true);
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
                        // A failed Gm-liveness escalation: still down, keep
                        // retrying on backoff (FR-010b).
                        if force_renewal {
                            gm_conn = super::GmConnectionState::Failed {
                                since: gm_episode_since(gm_conn),
                            };
                            obs.set_gm_connection_up(false);
                        }
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
    fn ping_verdict_idle_during_a_call() {
        // A call proves liveness by itself, so no probe is sent while one is up
        // — even when the interval has long since elapsed (R10, FR-006).
        let mut s = PingState::default();
        let t0 = Instant::now();
        s.on_sent(1, t0 - PING_INTERVAL * 2);
        assert_eq!(s.verdict(t0, true), PingVerdict::Idle);
    }

    #[test]
    fn ping_verdict_send_when_never_sent_or_interval_elapsed() {
        let s = PingState::default();
        let t0 = Instant::now();
        // Never pinged yet.
        assert_eq!(s.verdict(t0, false), PingVerdict::Send);

        let mut s2 = PingState {
            last_sent: Some(t0),
            pending: None,
        };
        // Interval not yet elapsed → idle.
        assert_eq!(s2.verdict(t0 + PING_INTERVAL / 2, false), PingVerdict::Idle);
        // Interval elapsed → send.
        s2.pending = None;
        assert_eq!(s2.verdict(t0 + PING_INTERVAL, false), PingVerdict::Send);
    }

    #[test]
    fn ping_verdict_await_then_dead_across_the_response_deadline() {
        let mut s = PingState::default();
        let t0 = Instant::now();
        s.on_sent(7, t0);
        // Within the deadline: keep waiting.
        assert_eq!(
            s.verdict(t0 + PING_RESPONSE_TIMEOUT / 2, false),
            PingVerdict::Await
        );
        // Past the deadline: the connection is dead.
        assert_eq!(
            s.verdict(t0 + PING_RESPONSE_TIMEOUT, false),
            PingVerdict::Dead
        );
    }

    #[test]
    fn ping_never_sends_a_second_while_one_is_pending() {
        let mut s = PingState::default();
        let t0 = Instant::now();
        s.on_sent(1, t0);
        // Even long after the interval, a pending ping means Await/Dead — never
        // a second concurrent Send.
        let v = s.verdict(t0 + PING_INTERVAL * 2, false);
        assert!(matches!(v, PingVerdict::Dead), "got {v:?}");
    }

    #[test]
    fn ping_on_response_matches_only_the_pending_cseq() {
        let mut s = PingState::default();
        s.on_sent(42, Instant::now());
        // A stale/mismatched CSeq must not clear the pending ping.
        assert!(!s.on_response(41));
        assert!(s.pending.is_some());
        // The matching CSeq clears it.
        assert!(s.on_response(42));
        assert!(s.pending.is_none());
    }

    #[test]
    fn ping_full_cycle_alive_then_dropped_then_dead() {
        // The end-to-end verdict flow the OPTIONS keepalive drives: a probe is
        // sent, answered (alive), then a later probe goes unanswered and, once
        // the response deadline passes, the connection is scored dead — which
        // is what triggers a reconnect. The socket round-trip itself is
        // covered by `sip_client::gm_server_reports_alive_and_delivers_a_real_message`.
        let mut s = PingState::default();
        let t0 = Instant::now();

        // First probe, answered within the deadline → alive.
        assert_eq!(s.verdict(t0, false), PingVerdict::Send);
        s.on_sent(1, t0);
        assert!(
            s.on_response(1),
            "matching response marks the connection alive"
        );
        assert!(s.pending.is_none());

        // Interval elapses, second probe sent, and no answer arrives.
        let t1 = t0 + PING_INTERVAL;
        assert_eq!(s.verdict(t1, false), PingVerdict::Send);
        s.on_sent(2, t1);
        assert_eq!(
            s.verdict(t1 + PING_RESPONSE_TIMEOUT / 2, false),
            PingVerdict::Await
        );
        assert_eq!(
            s.verdict(t1 + PING_RESPONSE_TIMEOUT, false),
            PingVerdict::Dead
        );
    }

    #[test]
    fn ping_response_alive_regardless_of_would_be_status() {
        // Any final response to the keepalive proves the connection carries
        // signaling — the response arm never inspects the status code, so a
        // 4xx/5xx is as good a liveness proof as a 200. `on_response` matching
        // purely on CSeq is what encodes that (specs/028 R1).
        let mut s = PingState::default();
        s.on_sent(3, Instant::now());
        assert!(s.on_response(3));
    }

    #[test]
    fn gm_episode_since_is_preserved_across_reconnecting_and_failed() {
        let t = SystemTime::now() - Duration::from_secs(42);
        let reconnecting = super::super::GmConnectionState::Reconnecting {
            since: t,
            attempts: 2,
        };
        assert_eq!(gm_episode_since(reconnecting), t);
        let failed = super::super::GmConnectionState::Failed { since: t };
        assert_eq!(gm_episode_since(failed), t);
        // A healthy connection has no episode; "since" starts now.
        let up_since = gm_episode_since(super::super::GmConnectionState::Up);
        assert!(up_since.elapsed().unwrap() < Duration::from_secs(1));
    }

    #[test]
    fn ping_reset_drops_in_flight_state() {
        let mut s = PingState::default();
        s.on_sent(9, Instant::now());
        s.reset();
        assert!(s.pending.is_none());
        assert!(s.last_sent.is_none());
        // After a reset the next verdict is Send, not a spurious Dead against a
        // CSeq that belonged to the replaced session (R11).
        assert_eq!(s.verdict(Instant::now(), false), PingVerdict::Send);
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
        let (req, _) = SipRequest::try_parse(raw).unwrap().unwrap();
        assert_eq!(extract_caller(&req), "+919000000000");
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
