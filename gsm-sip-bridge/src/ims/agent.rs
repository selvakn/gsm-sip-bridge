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
use crate::error::{BridgeError, BridgeResult};
use crate::ims::sdp::{self, NegotiatedCodec};
use crate::ims::sip_client::{
    build_100_trying, build_180_ringing, build_200_ok_bye, build_200_ok_invite,
    build_486_busy_here, build_bye, build_uas_response, format_sip_addr, random_hex,
    spawn_gm_server, ByeRequest, GmServer, SipMessage, SipRequest, SipSink,
};
use crate::ims::ImsRegisterConfig;
use crate::vowifi::control::{read_msg, reason, write_msg, ControlMessage};
use crate::vowifi::VETH_SIP_PORT;
use std::io::BufReader;
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime};

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
/// How often the main dispatch loop wakes up (when idle) to check whether
/// the registration needs renewing — matches the existing project's
/// `[resilience].network_poll_interval_sec` default (feature 009) for
/// consistency, not a hard requirement of this feature.
const REGISTRATION_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// How long the client-connection reader thread blocks per read before
/// looping. Only bounds how quickly it notices its channel has gone away —
/// messages themselves arrive as soon as they are read.
const CLIENT_READ_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// How far ahead of the registration's actual expiry Agent A starts trying
/// to renew it — SC-003's 90s recovery budget plus margin for the
/// renewal's own AKA-challenge round trip.
const RENEWAL_HEADROOM: Duration = Duration::from_secs(300);
const RETRY_INITIAL_BACKOFF: Duration = Duration::from_secs(5);
const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(120);

/// Entry point for the `vowifi-ims-agent` subcommand.
pub fn run(config: &VowifiConfig) -> ExitCode {
    match run_inner(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn read_pcscf(path: &str) -> BridgeResult<IpAddr> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| BridgeError::Ims(format!("failed to read P-CSCF address from {path}: {e}")))?;
    raw.trim()
        .parse()
        .map_err(|e| BridgeError::Ims(format!("invalid P-CSCF address in {path}: {e}")))
}

fn run_inner(config: &VowifiConfig) -> BridgeResult<()> {
    let pcscf_addr = read_pcscf(&config.pcscf_source_path)?;
    let reg_cfg = ImsRegisterConfig {
        modem_port: PathBuf::from(&config.modem_port),
        pcscf_addr,
        pcscf_port: 5060,
        mcc: config.mcc.clone(),
        mnc: config.mnc.clone(),
        imsi: None,
        imei: None,
        use_tcp: config.use_tcp,
        sec_agree: config.sec_agree,
        msisdn: None,
    };

    let veth_local_ip: IpAddr = config
        .veth_local_addr
        .parse()
        .map_err(|e| BridgeError::Ims(format!("invalid vowifi.veth_local_addr: {e}")))?;
    let control_addr: SocketAddr = format!("{}:{}", config.veth_peer_addr, config.control_port)
        .parse()
        .map_err(|e| BridgeError::Ims(format!("invalid vowifi control address: {e}")))?;

    let mut session = super::register_session(&reg_cfg)?;
    if session.status != 200 {
        let status = session.status;
        let reason = session.reason.clone();
        session.cleanup();
        return Err(BridgeError::Ims(format!(
            "IMS registration failed: {status} {reason}"
        )));
    }
    tracing::info!("vowifi-ims-agent registered, listening for inbound calls");
    // Before the SUBSCRIBE, so the listeners are up to catch its response and
    // the NOTIFY the network sends straight back on a new connection.
    let mut inbound = start_inbound(&session)?;
    subscribe_reg_event(&mut session);

    let status = Arc::new(Mutex::new(super::RegistrationStatus {
        state: super::RegistrationState::Registered,
        registered_at: Some(SystemTime::now()),
        expires_at: Some(SystemTime::now() + Duration::from_secs(super::DEFAULT_EXPIRES as u64)),
        last_failure: None,
    }));

    {
        let status_for_listener = status.clone();
        std::thread::spawn(move || {
            if let Err(e) = run_status_listener(veth_local_ip, status_for_listener) {
                tracing::warn!(error = %e, "registration-status listener failed");
            }
        });
    }

    let result = dispatch_loop(
        &mut session,
        &mut inbound,
        &reg_cfg,
        &status,
        control_addr,
        veth_local_ip,
        config.wideband,
    );
    session.cleanup();
    result
}

/// Every SIP message the network sends us, from either of the two
/// connections that make up a Gm association, funnelled into one queue —
/// each paired with the sink that answers on the connection it arrived on.
struct Inbound {
    rx: mpsc::Receiver<(SipMessage, SipSink)>,
    /// Held only for its `Drop`, which shuts the listener down. Replaced
    /// wholesale on re-registration, since a renewal negotiates a fresh SA
    /// on a fresh pair of ports.
    _server: Option<GmServer>,
}

/// Start reading both halves of the Gm association for `session`:
///
/// - the **client** connection we registered over, which carries responses
///   to requests *we* originate (e.g. the reg-event SUBSCRIBE); and
/// - the **protected server port** (`port-s`), which is the only place the
///   network delivers anything it originates — including inbound `INVITE`s.
///   Without it a registration looks healthy but is unreachable; see
///   `sip_client::spawn_gm_server`.
fn start_inbound(session: &super::RegisteredSession) -> BridgeResult<Inbound> {
    let (tx, rx) = mpsc::channel();

    let mut client_reader = session.transport.try_clone_reader()?;
    let client_sink = session.transport.sink()?;
    let client_tx = tx.clone();
    std::thread::spawn(move || loop {
        match client_reader.recv_message_deadline(CLIENT_READ_POLL_INTERVAL) {
            Ok(Some(msg)) => {
                if client_tx.send((msg, client_sink.clone())).is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "Gm client connection reader stopped");
                return;
            }
        }
    });

    let server = match session.gm_server_addr() {
        Some(addr) => Some(spawn_gm_server(addr, session.use_tcp, tx)?),
        None => {
            tracing::warn!(
                "no Gm IPsec SA on this registration — there is no protected server port, so the network cannot deliver inbound calls"
            );
            None
        }
    };

    Ok(Inbound {
        rx,
        _server: server,
    })
}

/// Answers `vowifi-status` queries (`ControlMessage::StatusQuery` →
/// `RegistrationStatusReply`) on `crate::vowifi::AGENT_A_STATUS_PORT` for
/// as long as the agent runs. A separate, always-listening connection from
/// the main dispatch loop's own SIP transport, so a status query never
/// competes with call signaling.
fn run_status_listener(
    veth_local_ip: IpAddr,
    status: Arc<Mutex<super::RegistrationStatus>>,
) -> BridgeResult<()> {
    let listener = std::net::TcpListener::bind((veth_local_ip, crate::vowifi::AGENT_A_STATUS_PORT))
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
            Ok(ControlMessage::StatusQuery) => {
                let snapshot = status.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let reply = ControlMessage::RegistrationStatusReply {
                    state: format!("{:?}", snapshot.state),
                    registered_at: snapshot.registered_at.and_then(to_unix),
                    expires_at: snapshot.expires_at.and_then(to_unix),
                    last_failure: snapshot
                        .last_failure
                        .map(|(t, msg)| (to_unix(t).unwrap_or(0), msg)),
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

fn to_unix(t: SystemTime) -> Option<u64> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Doubling backoff for registration-renewal retry, capped at `max`. Pure
/// and testable without a real timer.
fn next_backoff(current: Duration, max: Duration) -> Duration {
    current.checked_mul(2).unwrap_or(max).min(max)
}

/// Re-runs the full IMS-AKA REGISTER flow (a fresh AT+CSIM challenge, same
/// as the initial registration — there is no cheaper incremental refresh in
/// this protocol) to get a new, live `RegisteredSession`. Does not touch
/// `session`/`status` itself; the caller swaps them in only on success, so a
/// failed attempt leaves the still-valid old session in place until it
/// actually expires or a later retry succeeds.
fn attempt_renewal(reg_cfg: &ImsRegisterConfig) -> BridgeResult<super::RegisteredSession> {
    let mut new_session = super::register_session(reg_cfg)?;
    if new_session.status != 200 {
        let status = new_session.status;
        let reason = new_session.reason.clone();
        new_session.cleanup();
        return Err(BridgeError::Ims(format!(
            "renewal REGISTER rejected: {status} {reason}"
        )));
    }
    Ok(new_session)
}

/// Everything needed to build a reg-event SUBSCRIBE — split out from
/// `subscribe_reg_event` so the message formatting is unit-testable without
/// a live session.
struct SubscribeParts<'a> {
    /// Request-URI *and* To/From identity: the default public user identity
    /// (first sip: `P-Associated-URI` the registrar returned).
    impu: &'a str,
    route_headers: &'a [String],
    via_transport: &'a str,
    /// Sent from (Via) — the protected client port.
    local_addr: SocketAddr,
    /// Reached at (Contact) — the protected server port. See
    /// `super::RegisteredSession::contact_addr`.
    contact_addr: SocketAddr,
    public_user: &'a str,
    call_id: &'a str,
    from_tag: &'a str,
    cseq: u32,
    expires: u32,
}

fn build_subscribe(p: &SubscribeParts) -> String {
    let via_addr = format_sip_addr(p.local_addr);
    let contact_addr = format_sip_addr(p.contact_addr);
    let mut msg = format!(
        "SUBSCRIBE {impu} SIP/2.0\r\n\
         Via: SIP/2.0/{transport} {via_addr};branch=z9hG4bK{branch};rport\r\n\
         Max-Forwards: 70\r\n",
        impu = p.impu,
        transport = p.via_transport,
        via_addr = via_addr,
        branch = random_hex(6),
    );
    for route in p.route_headers {
        msg.push_str(route);
        msg.push_str("\r\n");
    }
    msg.push_str(&format!(
        "From: <{impu}>;tag={from_tag}\r\n\
         To: <{impu}>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} SUBSCRIBE\r\n\
         Contact: <sip:{public_user}@{contact_addr};transport={transport}>\r\n\
         Event: reg\r\n\
         Expires: {expires}\r\n\
         Accept: application/reginfo+xml\r\n\
         P-Access-Network-Info: 3GPP-WLAN\r\n\
         Content-Length: 0\r\n\r\n",
        impu = p.impu,
        from_tag = p.from_tag,
        call_id = p.call_id,
        cseq = p.cseq,
        public_user = p.public_user,
        contact_addr = contact_addr,
        transport = p.via_transport,
        expires = p.expires,
    ));
    msg
}

/// TS 24.229 §5.1.1.3: a UE subscribes to its own registration-state event
/// package (`Event: reg`) immediately after a successful registration. Some
/// IMS cores treat a binding whose UE never subscribes as incomplete and
/// exclude it from terminating-call routing; independently of that, the
/// resulting `NOTIFY`s (reginfo XML) are the only authoritative view of how
/// the network sees this binding — a server-side deregistration is otherwise
/// silent. Best-effort: the SUBSCRIBE's own response and the NOTIFYs arrive
/// asynchronously on the shared transport and are handled by
/// `dispatch_loop`, and a send failure only costs us that visibility.
fn subscribe_reg_event(session: &mut super::RegisteredSession) {
    let impu = session
        .headers
        .iter()
        .find(|(k, v)| k.eq_ignore_ascii_case("P-Associated-URI") && v.contains("sip:"))
        .and_then(|(_, v)| {
            let start = v.find('<')? + 1;
            let end = v.find('>')?;
            Some(v[start..end].to_string())
        })
        .unwrap_or_else(|| format!("sip:{}", session.public_uri));
    let route_headers: Vec<String> = session
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("Service-Route"))
        .map(|(_, v)| format!("Route: {v}"))
        .collect();
    let public_user = session
        .public_uri
        .split('@')
        .next()
        .unwrap_or(&session.public_uri)
        .to_string();
    let cseq = session.cseq;
    session.cseq += 1;
    let msg = build_subscribe(&SubscribeParts {
        impu: &impu,
        route_headers: &route_headers,
        via_transport: if session.use_tcp { "TCP" } else { "UDP" },
        local_addr: session.local_addr,
        contact_addr: session.contact_addr,
        public_user: &public_user,
        call_id: &random_hex(8),
        from_tag: &random_hex(4),
        cseq,
        expires: super::DEFAULT_EXPIRES,
    });
    match session.transport.send(&msg) {
        Ok(()) => tracing::info!(impu = %impu, "sent reg-event SUBSCRIBE"),
        Err(e) => tracing::warn!(error = %e, "failed to send reg-event SUBSCRIBE"),
    }
}

/// Acknowledges a `NOTIFY` and surfaces its payload. For `Event: reg` the
/// body is the network's reginfo XML — logged in full because it is the
/// ground truth for whether our binding is actually active for terminating
/// calls. The `To` header already carries our tag (it echoes our SUBSCRIBE's
/// `From` tag), so no tag is added.
fn handle_notify(sink: &SipSink, req: &SipRequest) {
    let _ = sink.send(&build_uas_response(200, "OK", req, None, None, None));
    let event = req.header("Event").unwrap_or("?").to_string();
    let sub_state = req.header("Subscription-State").unwrap_or("?").to_string();
    if req.body.contains("terminated") {
        tracing::warn!(
            event = %event,
            subscription_state = %sub_state,
            body = %req.body,
            "NOTIFY reports a terminated state — the network may have dropped our registration binding"
        );
    } else {
        tracing::info!(event = %event, subscription_state = %sub_state, body = %req.body, "received NOTIFY");
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
}

/// Answers on the connection a request arrived on, logging rather than
/// propagating a send failure — a broken connection is already terminal for
/// that dialog, and every caller is on a path where there is nothing better
/// to do about it.
fn respond(sink: &SipSink, what: &str, message: &str) {
    if let Err(e) = sink.send(message) {
        tracing::warn!(error = %e, response = %what, "failed to send SIP response");
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch_loop(
    session: &mut super::RegisteredSession,
    inbound: &mut Inbound,
    reg_cfg: &ImsRegisterConfig,
    status: &Arc<Mutex<super::RegistrationStatus>>,
    control_addr: SocketAddr,
    veth_local_ip: IpAddr,
    wideband: bool,
) -> BridgeResult<()> {
    let mut active_call: Option<ActiveCall> = None;
    let mut backoff = RETRY_INITIAL_BACKOFF;
    loop {
        // A hangup can start on *either* side. The carrier's arrives as a BYE
        // below; the PBX's arrives here, as a `CallEnded` from Agent B — and
        // must be turned into a BYE toward the carrier, or hanging up the SIP
        // extension would leave the caller listening to a call that is already
        // over.
        if let Some(call) = &mut active_call {
            match call.ctrl_rx.try_recv() {
                Ok(ControlMessage::CallEnded { reason, .. }) => {
                    let call = active_call.take().expect("just matched Some");
                    hangup_carrier(session, call, &reason);
                    continue;
                }
                Ok(other) => {
                    tracing::debug!(message = ?other, "ignoring control message during an active call");
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Agent B is gone; we can't keep a half-bridged call up.
                    let call = active_call.take().expect("just matched Some");
                    tracing::warn!(call_id = %call.call_id, "Agent B's control connection dropped mid-call");
                    hangup_carrier(session, call, reason::TRANSPORT_ERROR);
                    continue;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        // Poll fast enough to notice a PBX-side hangup promptly while a call is
        // up; idle otherwise, where the only deadline is registration renewal.
        let poll = if active_call.is_some() {
            ACTIVE_CALL_POLL_INTERVAL
        } else {
            REGISTRATION_POLL_INTERVAL
        };
        match inbound.rx.recv_timeout(poll) {
            Ok((SipMessage::Request(req), sink)) if req.method == "INVITE" => {
                if active_call.is_some() {
                    tracing::info!("declining inbound call: another VoWiFi call is already active");
                    let _ = sink.send(&build_486_busy_here(&req, &random_hex(4)));
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
                ) {
                    Ok(call) => active_call = call,
                    Err(e) => tracing::warn!(error = %e, "failed to handle inbound INVITE"),
                }
            }
            Ok((SipMessage::Request(req), sink)) if req.method == "BYE" => {
                match active_call.take() {
                    Some(call) => handle_bye(&sink, &req, call),
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
                if active_call.is_some() {
                    continue;
                }
                let Some(expires_at) = status.lock().unwrap_or_else(|e| e.into_inner()).expires_at
                else {
                    continue;
                };
                if !super::renewal_due(SystemTime::now(), expires_at, RENEWAL_HEADROOM) {
                    continue;
                }
                status.lock().unwrap_or_else(|e| e.into_inner()).state =
                    super::RegistrationState::Renewing;
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
                        drop(guard);
                        backoff = RETRY_INITIAL_BACKOFF;
                        tracing::info!("registration renewed");
                        subscribe_reg_event(session);
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            retry_in_secs = backoff.as_secs(),
                            "registration renewal failed, retrying with backoff"
                        );
                        let mut guard = status.lock().unwrap_or_else(|e| e.into_inner());
                        guard.state = super::RegistrationState::Failed;
                        guard.last_failure = Some((SystemTime::now(), e.to_string()));
                        drop(guard);
                        std::thread::sleep(backoff);
                        backoff = next_backoff(backoff, RETRY_MAX_BACKOFF);
                    }
                }
            }
        }
    }
}

fn extract_caller(req: &SipRequest) -> String {
    req.header("From")
        .and_then(|f| f.split("sip:").nth(1))
        .and_then(|rest| rest.split(['@', ';', '>']).next())
        .unwrap_or("unknown")
        .to_string()
}

/// Answers (or declines) one inbound carrier `INVITE`. Returns `Some` with
/// the bookkeeping `handle_bye` will need once the call is actually
/// bridged; `None` if it was declined (busy line, no compatible codec, or
/// Agent B couldn't bridge it) — every decline path sends a fast, explicit
/// `486 Busy Here` per the spec's Clarifications answer, never silence or
/// unanswered ringing (FR-009/FR-010).
fn handle_invite(
    session: &super::RegisteredSession,
    req: &SipRequest,
    sink: &SipSink,
    inbound: &Inbound,
    control_addr: SocketAddr,
    veth_local_ip: IpAddr,
    wideband: bool,
) -> BridgeResult<Option<ActiveCall>> {
    let call_id = req.header("Call-ID").unwrap_or_default().to_string();
    let caller = extract_caller(req);
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
    let Some(precheck) = sdp::select_codec(&offer, amr_safe::is_available(), wideband) else {
        tracing::info!(
            call_id = %call_id,
            amr_linked = amr_safe::is_available(),
            offered = ?offer.offered.iter().map(|c| (c.payload_type, c.codec)).collect::<Vec<_>>(),
            "offer has no codec we can answer with; declining"
        );
        sink.send(&build_486_busy_here(req, &random_hex(4)))?;
        return Ok(None);
    };

    // Only a wideband *carrier* leg has anything for a wideband veth leg to
    // preserve. A narrowband call (PCMU or AMR-NB — the two shapes Airtel
    // sends when the originating leg is narrowband) keeps the veth link on
    // PCMU, exactly the path it took before L16 existed.
    let veth_wideband = precheck.codec == NegotiatedCodec::AmrWb;
    let veth_rx = spawn_veth_uas_listener(veth_local_ip, veth_wideband)?;

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
            caller,
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
            )?;

            // Do NOT answer yet. The PBX extension is only ringing; our
            // `180 Ringing` above is what makes the network play ringback to
            // the caller, and a `200 OK` here would cut that off and leave them
            // in silence until someone picks up. Wait for Agent B to report a
            // real answer — while still watching the carrier's own signaling,
            // since the caller may give up (`CANCEL`) while it rings.
            match await_pbx_answer(&call_id, &ctrl_rx, inbound, req, &to_tag, sink)? {
                RingOutcome::Answered => {}
                RingOutcome::PbxDeclined => return Ok(None),
                RingOutcome::Abandoned { reason } => {
                    // Agent B is still ringing the extension — stop it.
                    let _ = write_msg(
                        &mut control,
                        &ControlMessage::CallEnded {
                            call_id: call_id.clone(),
                            reason: reason.to_string(),
                        },
                    );
                    return Ok(None);
                }
            }

            sink.send(&build_200_ok_invite(req, &to_tag, &contact, &answer_sdp))?;

            let stop = Arc::new(AtomicBool::new(false));
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
                )?;
            } else {
                // Both legs speak PCMU: forward the payloads untouched.
                spawn_relay(ims_rtp_socket, veth.rtp_socket, stop.clone());
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

            Ok(Some(ActiveCall {
                control,
                ctrl_rx,
                stop,
                dialog: DialogInfo::from_invite(req, &to_tag, session),
                call_id,
                to_tag,
            }))
        }
        ControlMessage::BridgeFailed {
            reason: fail_reason,
            ..
        } => {
            tracing::info!(call_id = %call_id, reason = %fail_reason, "Agent B could not bridge the call, declining");
            sink.send(&build_486_busy_here(req, &random_hex(4)))?;
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
fn hangup_carrier(session: &mut super::RegisteredSession, call: ActiveCall, reason: &str) {
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
    match session.transport.send(&bye) {
        Ok(()) => {
            tracing::info!(call_id = %call.call_id, reason, "PBX hung up; sent BYE to the carrier")
        }
        Err(e) => {
            tracing::warn!(call_id = %call.call_id, error = %e, "failed to BYE the carrier after a PBX hangup")
        }
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
    wideband: bool,
) -> BridgeResult<mpsc::Receiver<BridgeResult<VethUasResult>>> {
    let sip_socket = UdpSocket::bind((veth_local_ip, VETH_SIP_PORT))
        .map_err(|e| BridgeError::Ims(format!("veth SIP socket bind failed: {e}")))?;
    sip_socket
        .set_read_timeout(Some(VETH_INVITE_TIMEOUT))
        .map_err(|e| BridgeError::Ims(format!("veth SIP socket set_read_timeout failed: {e}")))?;

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(accept_veth_invite(&sip_socket, veth_local_ip, wideband));
    });
    Ok(rx)
}

fn accept_veth_invite(
    sip_socket: &UdpSocket,
    veth_local_ip: IpAddr,
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
    let contact = format!("<sip:agent-a@{veth_local_ip}:{VETH_SIP_PORT}>");
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

fn spawn_relay(a: UdpSocket, b: UdpSocket, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || relay_rtp(a, b, stop));
}

/// Relays raw UDP payloads bidirectionally between `a` and `b` (both
/// already `connect()`-ed to their remote peer) until `stop` is set.
/// Forwards bytes verbatim rather than decoding/re-encoding: both legs
/// speak the same codec by construction — `handle_invite` only reaches this
/// point once the carrier offer negotiated PCMU, and Agent B's PJSIP leg is
/// always PCMU too — so the wire bytes (RTP header included: SSRC,
/// sequence, timestamp all stay whatever the real source generated) are
/// already correct for the other side without modification.
pub fn relay_rtp(a: UdpSocket, b: UdpSocket, stop: Arc<AtomicBool>) {
    let (a2, b2, stop2) = match (a.try_clone(), b.try_clone()) {
        (Ok(a2), Ok(b2)) => (a2, b2, stop.clone()),
        (Err(e), _) | (_, Err(e)) => {
            tracing::error!(error = %e, "RTP relay socket clone failed, aborting relay");
            return;
        }
    };
    let _ = a.set_read_timeout(Some(RELAY_POLL_INTERVAL));
    let _ = b.set_read_timeout(Some(RELAY_POLL_INTERVAL));

    let h1 = std::thread::spawn(move || forward(a, b2, stop));
    let h2 = std::thread::spawn(move || forward(b, a2, stop2));
    let _ = h1.join();
    let _ = h2.join();
}

fn forward(src: UdpSocket, dst: UdpSocket, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        match src.recv(&mut buf) {
            Ok(n) => {
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
    use std::net::Ipv4Addr;

    fn loopback_socket() -> UdpSocket {
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap()
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
        let handle = std::thread::spawn(move || relay_rtp(ims_side, veth_side, stop_clone));

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
