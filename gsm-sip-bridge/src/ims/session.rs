//! Registration-session machinery shared by every IMS transport
//! (specs/017-volte-inbound-bridge, FR-019).
//!
//! These pieces were extracted from `ims::agent` — where they served the
//! Wi-Fi calling path alone — so the host-side cellular service can use the
//! *same* implementation rather than a copy. That distinction matters more
//! than it looks: two copies of registration, renewal and inbound dispatch
//! would drift, and the drift would surface on whichever path was tested
//! less. SC-008 exists to prevent exactly that.
//!
//! Nothing here knows which transport carries it. Anything that referenced
//! the Wi-Fi path's private link or its second process stayed behind in
//! `agent.rs`, which is what makes this a **move rather than a rewrite** —
//! the extraction changes no behaviour, so a regression would have to be a
//! compile error rather than a silent difference.

use super::sip_client::{
    build_message, build_uas_response, format_sip_addr, random_hex, sec_agree_headers,
    spawn_gm_server, GmServer, MessageRequest, SipMessage, SipRequest, SipSink,
};
use super::ImsRegisterConfig;
use crate::control::protocol::RegistrationStatus;
use crate::error::{BridgeError, BridgeResult};
use std::net::SocketAddr;
use std::sync::{mpsc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

/// How long the Gm client reader blocks before checking whether it should
/// stop. Moved here with `start_inbound`, which is its only user.
const CLIENT_READ_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Best-effort classification of a registration failure's `BridgeError`
/// message into one of the four closed `RegistrationStatus` values
/// (FR-014) — `register_session`/`attempt_renewal` don't return a
/// structured failure category, so this is a substring heuristic over the
/// error text rather than an exhaustive mapping.
pub(crate) fn map_registration_error(e: &BridgeError) -> RegistrationStatus {
    let msg = e.to_string().to_ascii_lowercase();
    if msg.contains("auth") || msg.contains("aka") || msg.contains("challenge") {
        RegistrationStatus::AuthFailed
    } else if msg.contains("timeout") || msg.contains("timed out") {
        RegistrationStatus::Timeout
    } else {
        RegistrationStatus::Rejected
    }
}

/// Maps a SIP REGISTER final-response status code onto `RegistrationStatus`.
pub(crate) fn map_registration_status_code(status: u16) -> RegistrationStatus {
    match status {
        401 | 403 | 407 => RegistrationStatus::AuthFailed,
        408 | 504 => RegistrationStatus::Timeout,
        _ => RegistrationStatus::Rejected,
    }
}

/// Every SIP message the network sends us, from either of the two
/// connections that make up a Gm association, funnelled into one queue —
/// each paired with the sink that answers on the connection it arrived on.
pub(crate) struct Inbound {
    pub(crate) rx: mpsc::Receiver<(SipMessage, SipSink)>,
    /// Retained so `restart_client_reader` can feed a replacement reader
    /// thread into the same queue after the client transport is swapped —
    /// without this, restarting just the client half would need a new
    /// channel, which would orphan `_server`'s already-spawned accept-loop
    /// threads (they hold their own clone of the original sender).
    pub(crate) tx: mpsc::Sender<(SipMessage, SipSink)>,
    /// Held only for its `Drop`, which shuts the listener down. Replaced
    /// wholesale on re-registration, since a renewal negotiates a fresh SA
    /// on a fresh pair of ports.
    pub(crate) _server: Option<GmServer>,
    /// The thread reading the client connection. It ends when the network
    /// closes that connection, which is how a closed one is noticed without a
    /// keepalive; see [`Inbound::client_reader_alive`].
    client_reader: Mutex<JoinHandle<()>>,
}

impl Inbound {
    /// Whether the client connection's reader is still running. A P-CSCF that
    /// closes an idle Gm connection (Jio does, within minutes) stops it, and
    /// until a write fails nothing else says the connection is gone.
    pub(crate) fn client_reader_alive(&self) -> bool {
        !self
            .client_reader
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_finished()
    }
}

/// Spawns the thread that reads the **client** connection we registered
/// over — the half of a Gm association that carries responses to requests
/// *we* originate (e.g. the reg-event SUBSCRIBE, or a BYE toward the
/// carrier) — feeding every message it parses into `tx`.
///
/// **This is the single reader of the client transport.** Anything that needs
/// a response to a request it sent on this connection (outbound INVITE
/// responses, keepalive `OPTIONS`) must consume it from `inbound.rx`, correlated
/// there, rather than calling `transport.recv_*` directly: a second concurrent
/// reader on the same socket races this one for the bytes and loses them
/// intermittently (specs/029 research R2 — this is exactly why the outbound
/// origination path was moved off a direct socket read). The one remaining
/// direct reader, `cancel_pending_invite`'s post-CANCEL courtesy read, is
/// documented there as an accepted best-effort exception.
fn spawn_client_reader(
    session: &super::RegisteredSession,
    tx: mpsc::Sender<(SipMessage, SipSink)>,
) -> BridgeResult<JoinHandle<()>> {
    let mut client_reader = session.transport()?.try_clone_reader()?;
    let client_sink = session.transport()?.sink()?;
    Ok(std::thread::spawn(move || loop {
        match client_reader.recv_message_deadline(CLIENT_READ_POLL_INTERVAL) {
            Ok(Some(msg)) => {
                if tx.send((msg, client_sink.clone())).is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "Gm client connection reader stopped");
                return;
            }
        }
    }))
}

/// Start reading both halves of the Gm association for `session`:
///
/// - the **client** connection we registered over (`spawn_client_reader`);
///   and
/// - the **protected server port** (`port-s`), which is the only place the
///   network delivers anything it originates — including inbound `INVITE`s.
///   Without it a registration looks healthy but is unreachable; see
///   `sip_client::spawn_gm_server`.
pub(crate) fn start_inbound(session: &super::RegisteredSession) -> BridgeResult<Inbound> {
    let (tx, rx) = mpsc::channel();

    let client_reader = spawn_client_reader(session, tx.clone())?;

    let server = match session.gm_server_addr() {
        Some(addr) => Some(spawn_gm_server(addr, session.use_tcp, tx.clone())?),
        None => {
            tracing::warn!(
                "no Gm IPsec SA on this registration — there is no protected server port, so the network cannot deliver inbound calls"
            );
            None
        }
    };

    Ok(Inbound {
        rx,
        tx,
        _server: server,
        client_reader: Mutex::new(client_reader),
    })
}

/// Restarts just the client-connection reader thread, after
/// `RegisteredSession::reconnect_transport` replaces `session.transport`
/// mid-registration. Deliberately leaves `inbound._server` untouched: the
/// protected-server-port listener is a fully independent socket, unaffected
/// by the client transport's death, and still bound to the exact same
/// `port-s` this reconnect keeps (it reuses the still-live Gm SA rather than
/// negotiating a fresh one) — recreating it via `start_inbound` would try to
/// rebind the port it is already listening on and fail.
pub(crate) fn restart_client_reader(
    session: &super::RegisteredSession,
    inbound: &Inbound,
) -> BridgeResult<()> {
    let reader = spawn_client_reader(session, inbound.tx.clone())?;
    *inbound
        .client_reader
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = reader;
    Ok(())
}

/// Restarts just the Gm protected-server-port listener after its accept loop
/// dies (`GmServer::is_alive` went false), mirroring `restart_client_reader`
/// for the other half of the association. Replaces `inbound._server`
/// wholesale, feeding the replacement into the same `tx` queue.
///
/// The port is free to rebind by the time this runs: the old listener's
/// `TcpListener` was moved into its accept thread, so that thread's fatal-exit
/// `return` dropped it. Reuses the still-live Gm SA on the same `port-s` the
/// registration negotiated (like `restart_client_reader`, this is not a
/// renewal — no fresh SA). Returns `Err` if there is no Gm SA at all, since
/// then there is no server port to rebind. See specs/028-gm-tcp-reconnect R4.
pub(crate) fn restart_gm_server(
    session: &super::RegisteredSession,
    inbound: &mut Inbound,
) -> BridgeResult<()> {
    let addr = session.gm_server_addr().ok_or_else(|| {
        BridgeError::Ims("no Gm SA on this registration; there is no server port to restart".into())
    })?;
    // Drop the old server first so its `stop` flag is set and the dead accept
    // thread is not left referenced, then bind the replacement.
    inbound._server = None;
    inbound._server = Some(spawn_gm_server(addr, session.use_tcp, inbound.tx.clone())?);
    Ok(())
}

pub(crate) fn to_unix(t: SystemTime) -> Option<u64> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Doubling backoff for registration-renewal retry, capped at `max`. Pure
/// and testable without a real timer.
pub(crate) fn next_backoff(current: Duration, max: Duration) -> Duration {
    current.checked_mul(2).unwrap_or(max).min(max)
}

/// Re-runs the full IMS-AKA REGISTER flow (a fresh AT+CSIM challenge, same
/// as the initial registration — there is no cheaper incremental refresh in
/// this protocol) to get a new, live `RegisteredSession`. Does not touch
/// `session`/`status` itself; the caller swaps them in only on success, so a
/// failed attempt leaves the still-valid old session in place until it
/// actually expires or a later retry succeeds.
pub(crate) fn attempt_renewal(
    reg_cfg: &ImsRegisterConfig,
) -> Result<super::RegisteredSession, RegisterFailure> {
    register_checked(reg_cfg, "renewal REGISTER rejected")
}

/// A registration attempt that did not end in `200`, with what P-CSCF
/// failover needs to judge it (specs/091 R4, FR-008): the final status, if one
/// arrived, and its `Retry-After`.
#[derive(Debug)]
pub(crate) struct RegisterFailure {
    pub(crate) error: BridgeError,
    pub(crate) status: Option<u16>,
    pub(crate) retry_after: Option<Duration>,
}

impl From<BridgeError> for RegisterFailure {
    fn from(error: BridgeError) -> Self {
        Self {
            error,
            status: None,
            retry_after: None,
        }
    }
}

impl std::fmt::Display for RegisterFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

/// One full registration through `reg_cfg`'s P-CSCF; anything but a `200`
/// final response is a [`RegisterFailure`] whose message starts with
/// `rejected` (the session is cleaned up first). `Retry-After` is read before
/// the cleanup discards the response.
pub(crate) fn register_checked(
    reg_cfg: &ImsRegisterConfig,
    rejected: &str,
) -> Result<super::RegisteredSession, RegisterFailure> {
    let mut new_session = super::register_session(reg_cfg)?;
    if new_session.status != 200 {
        let status = new_session.status;
        let reason = new_session.reason.clone();
        let retry_after = super::retry_after(&new_session.headers);
        new_session.cleanup();
        return Err(RegisterFailure {
            error: BridgeError::Ims(format!("{rejected}: {status} {reason}")),
            status: Some(status),
            retry_after,
        });
    }
    Ok(new_session)
}

/// Everything needed to build a reg-event SUBSCRIBE — split out from
/// `subscribe_reg_event` so the message formatting is unit-testable without
/// a live session.
pub(crate) struct SubscribeParts<'a> {
    /// Request-URI *and* To/From identity: the default public user identity
    /// (first sip: `P-Associated-URI` the registrar returned).
    pub(crate) impu: &'a str,
    pub(crate) route_headers: &'a [String],
    pub(crate) via_transport: &'a str,
    /// Sent from (Via) — the protected client port.
    pub(crate) local_addr: SocketAddr,
    /// Reached at (Contact) — the protected server port. See
    /// `super::RegisteredSession::contact_addr`.
    pub(crate) contact_addr: SocketAddr,
    pub(crate) public_user: &'a str,
    pub(crate) call_id: &'a str,
    pub(crate) from_tag: &'a str,
    pub(crate) cseq: u32,
    pub(crate) expires: u32,
    /// This line's real access-network type (`ImsRegisterConfig::access_network_info`
    /// — `"3GPP-WLAN"` for VoWiFi, a real E-UTRAN value for VoLTE), echoed
    /// into `P-Access-Network-Info` instead of a hardcoded value
    /// (specs/045 MT-11).
    pub(crate) access_network_info: &'a str,
    /// `Security-Verify` value (RFC 3329 §2.3.1) — `None` when the
    /// registration negotiated no Gm SA.
    pub(crate) security_verify: Option<&'a str>,
    /// `Some` for a refresh inside an existing subscription dialog (RFC 6665
    /// §4.1.2.2), `None` for a new subscription. `route_headers` is then the
    /// dialog's route set rather than the Service-Route.
    pub(crate) dialog: Option<SubscribeDialog<'a>>,
}

/// The parts of an in-dialog SUBSCRIBE that differ from an initial one.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SubscribeDialog<'a> {
    /// The network's tag, carried in `To`.
    pub(crate) remote_tag: &'a str,
    /// The dialog's remote target (the notifier's `Contact`).
    pub(crate) request_uri: &'a str,
}

pub(crate) fn build_subscribe(p: &SubscribeParts) -> String {
    let via_addr = format_sip_addr(p.local_addr);
    let contact_addr = format_sip_addr(p.contact_addr);
    let mut msg = format!(
        "SUBSCRIBE {request_uri} SIP/2.0\r\n\
         Via: SIP/2.0/{transport} {via_addr};branch=z9hG4bK{branch};rport\r\n\
         Max-Forwards: 70\r\n",
        request_uri = p.dialog.map_or(p.impu, |d| d.request_uri),
        transport = p.via_transport,
        via_addr = via_addr,
        branch = random_hex(6),
    );
    for route in p.route_headers {
        msg.push_str(route);
        msg.push_str("\r\n");
    }
    let to_tag = p
        .dialog
        .map(|d| format!(";tag={}", d.remote_tag))
        .unwrap_or_default();
    msg.push_str(&format!(
        "From: <{impu}>;tag={from_tag}\r\n\
         To: <{impu}>{to_tag}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} SUBSCRIBE\r\n\
         Contact: <sip:{public_user}@{contact_addr};transport={transport}>\r\n\
         Event: reg\r\n\
         Expires: {expires}\r\n\
         Accept: application/reginfo+xml\r\n\
         {sec_agree}\
         P-Access-Network-Info: {access_network_info}\r\n\
         Content-Length: 0\r\n\r\n",
        impu = p.impu,
        from_tag = p.from_tag,
        call_id = p.call_id,
        cseq = p.cseq,
        public_user = p.public_user,
        contact_addr = contact_addr,
        transport = p.via_transport,
        expires = p.expires,
        access_network_info = p.access_network_info,
        sec_agree = sec_agree_headers(p.security_verify),
    ));
    msg
}

/// A reg-event SUBSCRIBE to send: a new subscription, or a refresh of an
/// existing one when `refresh` is set.
pub(crate) struct SubscribeRequest<'a> {
    pub(crate) access_network_info: &'a str,
    pub(crate) expires: u32,
    pub(crate) refresh: Option<SubscribeRefresh<'a>>,
}

/// The dialog a refresh is sent in (RFC 6665 §4.1.2.2): the same Call-ID and
/// tags as the subscription it renews.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SubscribeRefresh<'a> {
    pub(crate) call_id: &'a str,
    pub(crate) local_tag: &'a str,
    pub(crate) remote_tag: &'a str,
    /// `None` when the network gave no usable `Contact`: the IMPU is used.
    pub(crate) remote_target: Option<&'a str>,
    /// Empty when the network recorded no route: the Service-Route is used.
    pub(crate) route_set: &'a [String],
}

/// What [`send_reg_subscribe`] sent, so its response and NOTIFYs can be
/// matched back to it (specs/090 FR-001).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SentSubscribe {
    pub(crate) call_id: String,
    pub(crate) local_tag: String,
    pub(crate) cseq: u32,
    pub(crate) expires: u32,
}

/// TS 24.229 §5.1.1.3: a UE subscribes to its own registration-state event
/// package (`Event: reg`) immediately after a successful registration. Some
/// IMS cores treat a binding whose UE never subscribes as incomplete and
/// exclude it from terminating-call routing; independently of that, the
/// resulting `NOTIFY`s (reginfo XML) are the only authoritative view of how
/// the network sees this binding — a server-side deregistration is otherwise
/// silent.
///
/// Sends and returns at once: the response and the NOTIFYs arrive
/// asynchronously on the shared transport, and the dispatch loop matches them
/// to the identifiers returned here (specs/090). A send failure is `Err`,
/// because an unsendable SUBSCRIBE is itself evidence about the Gm path
/// (FR-011).
pub(crate) fn send_reg_subscribe(
    session: &mut super::RegisteredSession,
    req: &SubscribeRequest,
) -> BridgeResult<SentSubscribe> {
    let impu = session
        .default_impu()
        .unwrap_or_else(|| format!("sip:{}", session.public_uri));
    let route_headers = match req.refresh {
        Some(r) if !r.route_set.is_empty() => r.route_set.to_vec(),
        _ => service_route_headers(session),
    };
    let public_user = session
        .public_uri
        .split('@')
        .next()
        .unwrap_or(&session.public_uri)
        .to_string();
    let (call_id, local_tag) = match req.refresh {
        Some(r) => (r.call_id.to_string(), r.local_tag.to_string()),
        None => (random_hex(8), random_hex(4)),
    };
    let kind = if req.refresh.is_some() {
        "refresh"
    } else {
        "initial"
    };
    let cseq = session.cseq;
    session.cseq += 1;
    let msg = build_subscribe(&SubscribeParts {
        impu: &impu,
        route_headers: &route_headers,
        via_transport: if session.use_tcp { "TCP" } else { "UDP" },
        local_addr: session.local_addr,
        contact_addr: session.contact_addr,
        public_user: &public_user,
        call_id: &call_id,
        from_tag: &local_tag,
        cseq,
        expires: req.expires,
        access_network_info: req.access_network_info,
        security_verify: session.security_verify(),
        dialog: req.refresh.map(|r| SubscribeDialog {
            remote_tag: r.remote_tag,
            request_uri: r.remote_target.unwrap_or(&impu),
        }),
    });
    match session.transport_mut().and_then(|t| t.send(&msg)) {
        Ok(()) => {
            tracing::info!(
                impu = %impu,
                call_id = %call_id,
                cseq,
                kind,
                expires = req.expires,
                "sent reg-event SUBSCRIBE"
            );
            Ok(SentSubscribe {
                call_id,
                local_tag,
                cseq,
                expires: req.expires,
            })
        }
        Err(e) => {
            tracing::warn!(error = %e, call_id = %call_id, kind, "failed to send reg-event SUBSCRIBE");
            Err(e)
        }
    }
}

/// RFC 3608: an out-of-dialog request we originate routes via the
/// `Service-Route` set the registrar returned, in order.
fn service_route_headers(session: &super::RegisteredSession) -> Vec<String> {
    session
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("Service-Route"))
        .map(|(_, v)| format!("Route: {v}"))
        .collect()
}

/// TS 24.229 §5.1.2A.1.1 / TS 24.341 Annex B.6 table B.6-7: the first entry
/// of the route set is the P-CSCF itself, `lr`, ahead of the saved
/// Service-Route. The port is the P-CSCF's protected server port when a Gm
/// SA exists (traffic to 5060 would bypass the SA), otherwise the address
/// registered through.
pub(crate) fn pcscf_route_header(session: &super::RegisteredSession) -> String {
    let mut addr = session.pcscf_addr;
    if let Some(sa) = session.gm_state.as_ref() {
        addr.set_port(sa.theirs.port_s);
    }
    format!("Route: <sip:{};lr>", format_sip_addr(addr))
}

/// TS 24.341 §5.3.2.4: send the SMS delivery report for a short message
/// delivered to us over IMS — an RP-ACK (`ims::sms_pdu::build_rp_ack`)
/// carried in a **new `MESSAGE` request** addressed to the IP-SM-GW.
///
/// # Why this is not the `200 OK`
///
/// The `200 OK` answering the inbound `MESSAGE` is the SIP layer saying the
/// *request* arrived. The network waits separately for the RP layer to say
/// the *short message* was taken, and §5.3.2.3 asks for both: "generate a SIP
/// response according to RFC 3428" **and** "create a delivery report as
/// described in subclause 5.3.2.4". Annex B.6 shows the pair on the wire —
/// step 5 is a bodiless `200 OK`, step 8 a separate `MESSAGE` from the UE.
///
/// An earlier attempt at this (branch `jio-sms-ims-investigation`) put the
/// RP-ACK in the `200 OK`'s body instead, and concluded from the resulting
/// silence that acknowledgement was not the problem. A gateway does not read
/// a body off a `MESSAGE` response, so that experiment never tested what it
/// was thought to test.
///
/// # Why a request rather than a response is worth something here
///
/// It also moves the acknowledgement onto the direction that demonstrably
/// works. On the carrier this was written for, our REGISTER, INVITE and
/// SUBSCRIBE all complete while our *responses* are ignored — the whole
/// reason `vowifi.respond_on_client` exists. A report sent as a request does
/// not depend on that defect being solved first.
///
/// `in_reply_to` is the inbound `MESSAGE`'s `Call-ID` (§5.3.2.4 (d)).
/// `pcscf_route` puts the P-CSCF ahead of the `Service-Route` entries (see
/// [`pcscf_route_header`]).
///
/// Best-effort: the `202 Accepted` comes back
/// asynchronously on the shared transport and is handled by the dispatch
/// loop, and a send failure costs a redelivery, which the caller's dedupe
/// absorbs. Returns the report's own `Call-ID` when it was written, so the
/// dispatcher can match the network's answer.
pub(crate) fn send_sms_delivery_report(
    session: &mut super::RegisteredSession,
    ipsmgw_uri: &str,
    rp_ack: &[u8],
    in_reply_to: Option<&str>,
    pcscf_route: bool,
) -> Option<String> {
    let impu = session
        .default_impu()
        .unwrap_or_else(|| format!("sip:{}", session.public_uri));
    let route_headers = report_route_headers(session, pcscf_route);
    let cseq = session.cseq;
    session.cseq += 1;
    let call_id = random_hex(8);
    let msg = build_message(&MessageRequest {
        target_uri: ipsmgw_uri,
        impu: &impu,
        route_headers: &route_headers,
        local_addr: session.local_addr,
        transport: if session.use_tcp { "TCP" } else { "UDP" },
        call_id: &call_id,
        in_reply_to,
        from_tag: &random_hex(4),
        branch: &format!("z9hG4bK{}", random_hex(6)),
        cseq,
        content_type: "application/vnd.3gpp.sms",
        body: rp_ack,
        security_verify: session.security_verify(),
    });
    match session.transport_mut().and_then(|t| t.send_bytes(&msg)) {
        Ok(()) => {
            tracing::info!(
                ipsmgw = %ipsmgw_uri,
                report_call_id = %call_id,
                rp_ack = %rp_ack.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                "sent the SMS delivery report"
            );
            Some(call_id)
        }
        // Never silently swallowed: an undelivered report is exactly what
        // makes a message centre redeliver, and it is invisible otherwise.
        Err(e) => {
            tracing::warn!(
                error = %e,
                ipsmgw = %ipsmgw_uri,
                "failed to send the SMS delivery report; the network may redeliver this message"
            );
            None
        }
    }
}

/// The delivery report's `Route` set: the P-CSCF first when `pcscf_route`,
/// then the registrar's `Service-Route` entries in order.
fn report_route_headers(session: &super::RegisteredSession, pcscf_route: bool) -> Vec<String> {
    let mut routes = Vec::new();
    if pcscf_route {
        routes.push(pcscf_route_header(session));
    }
    routes.extend(service_route_headers(session));
    routes
}

/// One `<contact>` element of a reg-event NOTIFY body (RFC 3680 §4.1 / TS
/// 24.229 §5.1.1.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReginfoContact {
    pub(crate) state: String,
    pub(crate) event: String,
    /// The binding's address, from `<uri>` (see [`contact_uri_addr`]). `None`
    /// when there is no address to check either way — research R2.
    pub(crate) addr: Option<SocketAddr>,
    /// The `retry-after` attribute, in seconds (RFC 3680 §5.1): how long the
    /// network wants a `probation`/`rejected` contact to stay away.
    pub(crate) retry_after: Option<u64>,
    /// `state` of the enclosing `<registration>` element — `terminated` there
    /// means the whole public identity was deregistered, not just this binding
    /// (research R6). `None` for a contact not nested in one.
    pub(crate) registration_state: Option<String>,
}

/// One `<contact>` element's raw text and the opening tag of the
/// `<registration>` element enclosing it, if any.
struct ContactBlock<'a> {
    text: &'a str,
    registration_tag: Option<&'a str>,
}

/// Every `<contact ...>` element in `body`, as its raw text — the opening
/// tag alone when self-closing, or through the matching `</contact>`
/// otherwise — with its enclosing `<registration ...>` opening tag. Not a
/// general XML parser: just enough of reginfo's shape ([`find_own_contacts`]'s
/// only caller) to isolate one element at a time so its attributes can be read
/// without another element's same-named attribute getting matched first.
fn contact_blocks(body: &str) -> Vec<ContactBlock<'_>> {
    let mut blocks = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel_start) = body[cursor..].find("<contact") {
        let start = cursor + rel_start;
        let Some(rel_tag_end) = body[start..].find('>') else {
            break; // malformed: an opening tag that never closes
        };
        let tag_end = start + rel_tag_end;
        let self_closing =
            tag_end.checked_sub(1).and_then(|i| body.as_bytes().get(i)) == Some(&b'/');
        let (block, next_cursor) = if self_closing {
            (&body[start..=tag_end], tag_end + 1)
        } else if let Some(rel_close) = body[tag_end..].find("</contact>") {
            let close_end = tag_end + rel_close + "</contact>".len();
            (&body[start..close_end], close_end)
        } else {
            (&body[start..], body.len()) // malformed: no closing tag
        };
        blocks.push(ContactBlock {
            text: block,
            registration_tag: enclosing_registration_tag(&body[..start]),
        });
        cursor = next_cursor;
        if cursor >= body.len() {
            break;
        }
    }
    blocks
}

/// The opening tag of the `<registration>` element still open at the end of
/// `before` — the nearest preceding `<registration` with no `</registration>`
/// after it. `None` when the contact is not nested in one.
fn enclosing_registration_tag(before: &str) -> Option<&str> {
    let open = before.rfind("<registration")?;
    if before[open..].contains("</registration>") {
        return None;
    }
    let end = before[open..]
        .find('>')
        .map_or(before.len(), |e| open + e + 1);
    Some(&before[open..end])
}

/// The value of `name="..."` anywhere in `tag` — attribute order isn't
/// assumed, only that the value itself contains no `"`.
fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

fn parse_contact_attrs(block: &ContactBlock) -> Option<ReginfoContact> {
    let text = block.text;
    // `retry-after` is read from the opening tag only: a child element is
    // free to carry an attribute of the same name.
    let open_tag = text.find('>').map_or(text, |e| &text[..=e]);
    Some(ReginfoContact {
        state: extract_attr(text, "state")?,
        event: extract_attr(text, "event").unwrap_or_default(),
        addr: contact_uri_addr(text),
        retry_after: extract_attr(open_tag, "retry-after").and_then(|v| network_secs(&v)),
        registration_state: block
            .registration_tag
            .and_then(|t| extract_attr(t, "state")),
    })
}

/// The address in a `<contact>` element's `<uri>` child — the `host:port` a
/// registrar records for a binding, which for ours is exactly the
/// `contact_addr` we advertised (`sip:{user}@{contact_addr};...`). URI
/// parameters the network appends (`;Hpt=...`, `;transport=...`) are cut off
/// first. `None` for no `<uri>`, no port, or anything that isn't a literal
/// `ip:port` / `[ipv6]:port` — never guessed at.
fn contact_uri_addr(block: &str) -> Option<SocketAddr> {
    let start = block.find("<uri>")? + "<uri>".len();
    let end = block[start..].find("</uri>")? + start;
    let uri = block[start..end].trim();
    let rest = uri
        .strip_prefix("sips:")
        .or_else(|| uri.strip_prefix("sip:"))?;
    let host_port = rest.rsplit('@').next()?;
    let host_port = host_port.split([';', '?', '>']).next().unwrap_or(host_port);
    host_port.parse().ok()
}

/// How [`find_own_contacts`] decided which `<contact>` entries are ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Attribution {
    /// The entry echoes our `+sip.instance` IMEI and carries no address to
    /// check — never one naming another address, which is a retired binding.
    Imei,
    /// The entry's `<uri>` names the address we advertised in our Contact.
    ContactAddr,
    /// The document's only contact, carrying no address to check either way.
    SoleContact,
    /// Nothing could be attributed to us.
    None,
}

impl std::fmt::Display for Attribution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Imei => "imei",
            Self::ContactAddr => "contact-addr",
            Self::SoleContact => "sole-contact",
            Self::None => "none",
        })
    }
}

/// Every `<contact>` entry in a reg-event NOTIFY body that describes *our*
/// binding, and how they were recognised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnContacts {
    /// Entries for the binding this line holds now.
    pub(crate) entries: Vec<ReginfoContact>,
    /// Entries echoing our IMEI at another address: a binding of ours that a
    /// renewal has since replaced (issue #124). Logged, never acted on.
    pub(crate) retired: Vec<ReginfoContact>,
    pub(crate) matched_by: Attribution,
}

impl OwnContacts {
    /// RFC 3680 §4.1 deregistration on *any* of our entries. A reginfo
    /// document lists one `<registration>` per public identity, so one
    /// binding can appear several times (the SIP and tel AORs of an implicit
    /// set, issue #117); losing any of them is reason enough to re-register,
    /// which restores the whole set.
    #[cfg(test)]
    pub(crate) fn deregistered(&self) -> bool {
        self.entries.iter().any(contact_reports_deregistration)
    }
}

/// Sorts every `<contact>` element in a reg-event NOTIFY body into ours now,
/// ours from before, and someone else's (research R1).
///
/// The address decides first. An entry whose `<uri>` names `contact_addr`,
/// the address our current Contact advertised, is ours: its port is a fresh
/// ephemeral one per registration, so nothing else can name it. This is also
/// what identifies us on a carrier that never echoes the IMEI (issue #117: the
/// same contact under both a SIP and a tel AOR).
///
/// The IMEI only identifies the *device*, not the binding. TS 24.229
/// §5.1.1.4.1 NOTE 10 and §5.4.2.1.2 NOTE 2 let one instance hold more than
/// one contact, and after a renewal on a new port the network retires the old
/// one in a NOTIFY that still echoes our `+sip.instance`. Reading that as our
/// current binding being lost re-registered the line, which retired the
/// binding it had just made, about once per renewal (issue #124). So:
///
/// - an entry echoing our IMEI at a *different* address is `retired`;
/// - an entry echoing our IMEI with no checkable address is current, since
///   nothing proves it is an old binding and dropping it would make the line
///   deaf on a carrier that omits or rewrites `<uri>`.
///
/// Failing all of that, the document's only element is taken — but only when
/// it carries no address to check. A sole contact naming a *different* address
/// is someone else's: a `state="partial"` NOTIFY about a paired handset
/// carries just that handset's contact, and reading it as ours would force a
/// re-registration this line never needed.
///
/// Every match is collected, not just the first: one binding appears once
/// per public identity.
pub(crate) fn find_own_contacts(body: &str, imei: &str, contact_addr: SocketAddr) -> OwnContacts {
    let blocks = contact_blocks(body);
    let echoes_imei = |b: &ContactBlock| !imei.is_empty() && b.text.contains(imei);
    let mut entries = Vec::new();
    let mut retired = Vec::new();
    let mut any_by_addr = false;
    for block in &blocks {
        let Some(c) = parse_contact_attrs(block) else {
            continue;
        };
        match c.addr {
            Some(a) if a == contact_addr => {
                any_by_addr = true;
                entries.push(c);
            }
            Some(_) if echoes_imei(block) => retired.push(c),
            Some(_) => {}
            None if echoes_imei(block) => entries.push(c),
            None => {}
        }
    }
    if !entries.is_empty() {
        return OwnContacts {
            entries,
            retired,
            matched_by: if any_by_addr {
                Attribution::ContactAddr
            } else {
                Attribution::Imei
            },
        };
    }
    if let [only] = blocks.as_slice() {
        if contact_uri_addr(only.text).is_none() {
            if let Some(c) = parse_contact_attrs(only) {
                return OwnContacts {
                    entries: vec![c],
                    retired,
                    matched_by: Attribution::SoleContact,
                };
            }
        }
    }
    OwnContacts {
        entries: Vec::new(),
        retired,
        matched_by: Attribution::None,
    }
}

/// A deregistration event RFC 3680 §4.1 pairs with `state="terminated"`,
/// ordered by how restrictive the response TS 24.229 §5.1.1.7 asks for is —
/// so when one NOTIFY reports several, the `max()` is the one to act on
/// (FR-014).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DeregEvent {
    Unregistered,
    Probation,
    Deactivated,
    Rejected,
}

impl DeregEvent {
    /// The event a contact reports, if it is a deregistration at all.
    /// `expired` is our own scheduled renewal running its course, not the
    /// network dropping us, so it is deliberately excluded (FR-013), as is
    /// any contact whose `state` is not `terminated`.
    pub(crate) fn from_contact(c: &ReginfoContact) -> Option<Self> {
        if !c.state.eq_ignore_ascii_case("terminated") {
            return None;
        }
        match c.event.to_ascii_lowercase().as_str() {
            "unregistered" => Some(Self::Unregistered),
            "probation" => Some(Self::Probation),
            "deactivated" => Some(Self::Deactivated),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

impl std::fmt::Display for DeregEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unregistered => "unregistered",
            Self::Probation => "probation",
            Self::Deactivated => "deactivated",
            Self::Rejected => "rejected",
        })
    }
}

/// RFC 3680 §4.1: `state="terminated"` is only a deregistration when paired
/// with one of the [`DeregEvent`]s — see [`DeregEvent::from_contact`].
#[cfg(test)]
fn contact_reports_deregistration(c: &ReginfoContact) -> bool {
    DeregEvent::from_contact(c).is_some()
}

/// Whether a deregistration took the whole public identity down or only our
/// binding under it (FR-015). Reported, never acted on differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeregScope {
    /// Every `<registration>` holding our current binding is `terminated`.
    IdentityDeregistered,
    /// The identity stays registered (another device's binding, or another
    /// public identity of ours, is still active); only our binding went.
    BindingRemoved,
}

impl std::fmt::Display for DeregScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::IdentityDeregistered => "identity-deregistered",
            Self::BindingRemoved => "binding-removed",
        })
    }
}

/// The network deregistered our current binding: what the agent acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Deregistration {
    /// The most restrictive event among our current entries (FR-014).
    pub(crate) event: DeregEvent,
    /// The longest `retry-after` the network asked for, from our entries'
    /// attribute or the `Subscription-State` header's parameter.
    pub(crate) retry_after: Option<Duration>,
    pub(crate) scope: DeregScope,
}

/// What a reg-event NOTIFY says about this line: which entries are ours, and
/// the deregistration to act on, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotifyVerdict {
    pub(crate) own: OwnContacts,
    pub(crate) deregistration: Option<Deregistration>,
}

/// The longest delay any network-supplied `expires` or `retry-after` is taken
/// to mean. Larger values are clamped to it: they are nonsensical, and added to
/// an `Instant` unchecked, a value near `u64::MAX` overflows and panics the
/// agent — a crash any NOTIFY could trigger.
pub(crate) const MAX_NETWORK_DELAY_SECS: u64 = 366 * 24 * 3600;

/// A network-supplied number of seconds, clamped to [`MAX_NETWORK_DELAY_SECS`].
fn network_secs(value: &str) -> Option<u64> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(|v| v.min(MAX_NETWORK_DELAY_SECS))
}

/// The substate a `Subscription-State` header names (RFC 6665 §8.2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubStateKind {
    Active,
    Pending,
    Terminated,
    /// No header, an empty one, or a substate this UA does not know.
    Missing,
}

/// A parsed `Subscription-State` header value, e.g.
/// `terminated;reason=probation;retry-after=900`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubState {
    pub(crate) state: SubStateKind,
    pub(crate) expires: Option<u64>,
    /// Lower-cased: RFC 6665 event reasons are tokens, compared
    /// case-insensitively.
    pub(crate) reason: Option<String>,
    pub(crate) retry_after: Option<u64>,
}

/// Parses a `Subscription-State` header value. Pure, so every reason the
/// subscription acts on is testable (specs/090 R7). A parameter whose value is
/// not a number is treated as absent rather than as an error.
pub(crate) fn parse_subscription_state(value: &str) -> SubState {
    let mut parts = value.split(';');
    let state = match parts
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "active" => SubStateKind::Active,
        "pending" => SubStateKind::Pending,
        "terminated" => SubStateKind::Terminated,
        _ => SubStateKind::Missing,
    };
    let mut parsed = SubState {
        state,
        expires: None,
        reason: None,
        retry_after: None,
    };
    for param in parts {
        let Some((name, value)) = param.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "expires" => parsed.expires = network_secs(value),
            "retry-after" => parsed.retry_after = network_secs(value),
            "reason" => parsed.reason = Some(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    parsed
}

/// Classifies a reg-event NOTIFY body for this line: the pure decision behind
/// [`handle_notify`], kept free of the socket so it can be tested on real
/// bodies (research R3).
pub(crate) fn classify_reg_notify(
    body: &str,
    subscription_state: &str,
    imei: &str,
    contact_addr: SocketAddr,
) -> NotifyVerdict {
    let own = find_own_contacts(body, imei, contact_addr);
    let deregistration = own
        .entries
        .iter()
        .filter_map(DeregEvent::from_contact)
        .max()
        .map(|event| {
            let retry_after = own
                .entries
                .iter()
                .filter_map(|c| c.retry_after)
                .chain(parse_subscription_state(subscription_state).retry_after)
                .max()
                .map(Duration::from_secs);
            // Research R6: the identity as a whole went only when every
            // registration holding our binding is itself terminated.
            let identity_gone = own.entries.iter().all(|c| {
                c.registration_state
                    .as_deref()
                    .is_some_and(|s| s.eq_ignore_ascii_case("terminated"))
            });
            Deregistration {
                event,
                retry_after,
                scope: if identity_gone {
                    DeregScope::IdentityDeregistered
                } else {
                    DeregScope::BindingRemoved
                },
            }
        });
    NotifyVerdict {
        own,
        deregistration,
    }
}

/// Answers a reg-event `NOTIFY`: `200` for a subscription of ours, `481` for
/// one we do not have (RFC 6665 §4.4.1). Which applies is the subscription
/// matcher's decision (specs/090 R6), made before anything is sent. The `To`
/// header already carries our tag (it echoes our SUBSCRIBE's `From` tag), so
/// no tag is added.
pub(crate) fn respond_notify(sink: &SipSink, req: &SipRequest, status: u16) {
    let reason = match status {
        481 => "Call/Transaction Does Not Exist",
        _ => "OK",
    };
    respond(
        sink,
        "NOTIFY response",
        &build_uas_response(status, reason, req, None, None, None),
    );
}

/// Evaluates a reg-event NOTIFY for the current subscription, already
/// answered by [`respond_notify`]. The body is the network's reginfo XML —
/// logged in full because it is the ground truth for whether our binding is
/// actually active for terminating calls.
///
/// Returns the deregistration of our own binding (see [`find_own_contacts`]
/// and [`classify_reg_notify`]), if the NOTIFY reported one — so the caller
/// can apply the policy for its event. Before this, any such NOTIFY was only
/// logged: the line would keep reporting itself `Registered` and silently
/// receive nothing until the next scheduled renewal, up to an hour later
/// (specs/041 conformance review, MT-09).
///
/// What the line does about each event is the caller's policy
/// (`on_network_deregistration`, TS 24.229 §5.1.1.7).
pub(crate) fn evaluate_reg_notify(
    session: &super::RegisteredSession,
    req: &SipRequest,
) -> Option<Deregistration> {
    let sub_state = req.header("Subscription-State").unwrap_or("?").to_string();
    let verdict = classify_reg_notify(&req.body, &sub_state, &session.imei, session.contact_addr);
    log_reg_notify(&verdict, session.contact_addr, &sub_state, &req.body);
    verdict.deregistration
}

/// Logs a NOTIFY for a subscription this line has replaced: acknowledged,
/// never applied (specs/090 FR-015). The attribution summary is included so
/// a reader can see what it would have said about our binding.
pub(crate) fn log_superseded_notify(session: &super::RegisteredSession, req: &SipRequest) {
    let sub_state = req.header("Subscription-State").unwrap_or("?");
    let verdict = classify_reg_notify(&req.body, sub_state, &session.imei, session.contact_addr);
    tracing::info!(
        call_id = %req.header("Call-ID").unwrap_or_default(),
        subscription_state = %sub_state,
        attribution = %verdict.own.matched_by,
        own_entries = verdict.own.entries.len(),
        contact_states = %contact_states(&verdict.own),
        body = %req.body,
        "NOTIFY for a superseded reg-event subscription; acknowledged, not applied"
    );
}

fn contact_states(own: &OwnContacts) -> String {
    own.entries
        .iter()
        .map(|c| format!("{}/{}", c.state, c.event))
        .collect::<Vec<_>>()
        .join(",")
}

/// The FR-016 line every reg-event NOTIFY gets, plus one FR-005 line per
/// replaced binding the network retired. Field names are a contract
/// (`specs/089-reg-notify-binding-attribution/contracts/observability.md`).
fn log_reg_notify(verdict: &NotifyVerdict, contact_addr: SocketAddr, sub_state: &str, body: &str) {
    let own = &verdict.own;
    for r in own
        .retired
        .iter()
        .filter(|r| r.state.eq_ignore_ascii_case("terminated"))
    {
        tracing::info!(
            retired_addr = %r.addr.map(|a| a.to_string()).unwrap_or_default(),
            state = %r.state,
            event = %r.event,
            contact_addr = %contact_addr,
            "replaced binding retired"
        );
    }
    let states = contact_states(own);
    match &verdict.deregistration {
        Some(d) => tracing::warn!(
            attribution = %own.matched_by,
            contact_addr = %contact_addr,
            own_entries = own.entries.len(),
            retired_entries = own.retired.len(),
            contact_states = %states,
            applied = %d.event,
            retry_after_secs = d.retry_after.map(|r| r.as_secs()),
            scope = %d.scope,
            subscription_state = %sub_state,
            subscription = "current",
            body = %body,
            "reg-event NOTIFY: our current binding was deregistered"
        ),
        None => {
            let message = if own.entries.is_empty() {
                "reg-event NOTIFY, nothing attributable to our current binding"
            } else {
                "reg-event NOTIFY for our binding"
            };
            tracing::info!(
                attribution = %own.matched_by,
                contact_addr = %contact_addr,
                own_entries = own.entries.len(),
                retired_entries = own.retired.len(),
                contact_states = %states,
                applied = "none",
                subscription_state = %sub_state,
                subscription = "current",
                body = %body,
                "{message}"
            );
        }
    }
}

/// Answers on the connection a request arrived on, logging rather than
/// propagating a send failure — a broken connection is already terminal for
/// that dialog, and every caller is on a path where there is nothing better
/// to do about it.
pub(crate) fn respond(sink: &SipSink, what: &str, message: &str) {
    if let Err(e) = sink.send(message) {
        tracing::warn!(error = %e, response = %what, "failed to send SIP response");
    }
}

/// The caller's identity for this bridge's own internal attribution (logs,
/// CDRs, SMS sender fields) — never re-presented to any third party, so
/// RFC 3325 §9.1's `Privacy` withholding obligation (which governs onward
/// signaling) does not apply to this use.
///
/// Prefers `P-Asserted-Identity` (RFC 3325 §9.1: a trusted network element
/// vouching for the caller) over `From` (caller-supplied, unverified) when
/// both are present — measured on real carrier traffic where the two can
/// legitimately differ. Falls back to `From` when no asserted identity is
/// present, exactly as before (specs/045 MT-12).
///
/// Each header may name the caller as a `sip:`/`sips:` or a `tel:` URI, and
/// `P-Asserted-Identity` may carry both (RFC 3325 §9.1); see
/// [`super::identity`] for the grammar. A URI that names no user (a bare
/// `sip:gateway.example`) yields no number, and neither header yielding one
/// gives `"unknown"` (issue #104).
pub(crate) fn extract_caller(req: &SipRequest) -> String {
    caller_identity(req)
        .map(|identity| identity.number)
        .unwrap_or_else(|| "unknown".to_string())
}

/// The identity [`extract_caller`] reports: the asserted one when it yields a
/// number, else `From`'s. Shared with [`unresolved_caller_diagnostic`] so the
/// two can never disagree about whether a caller was found.
fn caller_identity(req: &SipRequest) -> Option<super::identity::Identity> {
    use super::identity::{header_identity, HeaderParams};
    header_identity(req, "P-Asserted-Identity", HeaderParams::None)
        .or_else(|| header_identity(req, "From", HeaderParams::Allowed))
}

/// What to log when neither `P-Asserted-Identity` nor `From` yields a number,
/// or `None` when one does (FR-015). Issue #104 was hard to diagnose because
/// the bridge only said `caller=unknown`; this names the raw values so the
/// next such report can be read off the log instead of a packet capture.
///
/// A separate pure function rather than a log line inside [`extract_caller`],
/// which one INVITE calls up to three times (the busy/decline paths and the
/// handler): the caller of this logs it once per request. `{:?}` escapes
/// control characters, so a caller-supplied header cannot forge or split a
/// log line, and an absent header prints as `[]`.
pub(crate) fn unresolved_caller_diagnostic(req: &SipRequest) -> Option<String> {
    caller_identity(req).is_none().then(|| {
        format!(
            "P-Asserted-Identity={:?} From={:?}",
            req.headers_all("P-Asserted-Identity"),
            req.headers_all("From")
        )
    })
}

/// The caller's display name — CNAP/CLI name delivery (confirmed live
/// 2026-09-03: Indian carriers send this unprompted, no negotiation
/// needed), for re-presenting to the PBX/SIP-server side.
///
/// Reads the *same* header [`extract_caller`] actually sourced the number
/// from — the asserted identity when it yields a number, else `From` —
/// rather than independently preferring PAI's name and falling back to
/// `From`'s. A carrier's `From` can name a different party than its
/// `P-Asserted-Identity` (an SMSC gateway, e.g.); pairing a from-derived name
/// with a PAI-derived number would present a name that does not belong to
/// that number. When the sourced header has no display name of its own, this
/// returns `None` rather than reaching into the *other* header for one.
///
/// Within that header, the name is the first one among *all* its values
/// (RFC 3325 §9.1 allows two, on one line or two), so a name on the `sip`
/// value is found even when the nameless `tel` value comes first.
///
/// Callers of this function MUST also check
/// [`caller_identity_is_private`] before re-presenting the result onward —
/// unlike `extract_caller` (internal attribution only, RFC 3325 §9.1's
/// withholding obligation doesn't apply), a name reaching this function's
/// caller is headed for a PBX or handset display, which is exactly the
/// onward signaling `Privacy` governs.
pub(crate) fn extract_caller_name(req: &SipRequest) -> Option<String> {
    use super::identity::{header_identity, HeaderParams};
    match header_identity(req, "P-Asserted-Identity", HeaderParams::None) {
        Some(asserted) => asserted.display,
        None => header_identity(req, "From", HeaderParams::Allowed)?.display,
    }
}

/// RFC 3323/3325: `Privacy: id` or `Privacy: user` on the inbound request
/// means the caller's asserted identity must not be re-presented past a
/// trust boundary. Token comparison is case-insensitive per RFC 3261 §7.3.1
/// (header field values built from tokens are case-insensitive unless the
/// grammar says otherwise, and `Privacy`'s `priv-value` is such a token).
/// RFC 3323 §4.2's grammar (`Privacy-hdr = "Privacy" HCOLON priv-value
/// *(";" priv-value)`) separates multiple values with `;`, not the `,` most
/// other SIP list headers use — splitting on `,` alone missed a
/// spec-conformant `Privacy: header;id`; `,` is still accepted too, as a
/// harmless tolerance for a sender that gets this wrong.
///
/// Checks **every** `Privacy` header field (`headers_all`, not `header`):
/// RFC 3261 §7.3.1 makes repeated header fields with the same name
/// equivalent to one comma-separated field, so `id`/`user` placed in a
/// second `Privacy:` line is exactly as binding as one in the first, and
/// `header`'s "first match only" would silently miss it.
///
/// Checked separately from — not folded into — [`extract_caller_name`]
/// itself, so a caller can still log/attribute the name internally while
/// withholding it from onward signaling, the same split `extract_caller`'s
/// own doc comment draws for the number.
pub(crate) fn caller_identity_is_private(req: &SipRequest) -> bool {
    req.headers_all("Privacy").iter().any(|v| {
        v.split([';', ','])
            .any(|p| p.trim().eq_ignore_ascii_case("id") || p.trim().eq_ignore_ascii_case("user"))
    })
}

/// Escapes a caller-supplied display name for safe embedding inside a SIP
/// `quoted-string` (RFC 3261 §25.1: `qdtext` excludes `"` and `\`, so both
/// must be `quoted-pair`-escaped — `\"` / `\\`). Without this, a name
/// containing either character breaks out of the quotes when this bridge
/// builds its own `P-Asserted-Identity`/`From` around it, producing
/// malformed syntax a strict endpoint may reject or misparse.
/// [`super::identity::parse_name_addr`] has already rejected embedded CR/LF, so this
/// only needs to handle the two `quoted-string`-special characters.
pub(crate) fn escape_display_name(name: &str) -> String {
    name.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The **whole URI** named by a header, where [`extract_caller`] wants only
/// the number — for addressing a new request back at whoever sent this one.
/// URI parameters stay (`;transport=udp` belongs to the URI).
///
/// Every value of every line of the header is considered
/// (RFC 3261 §7.3.1), and a `sip`/`sips` URI is preferred over any other: a
/// two-value `P-Asserted-Identity` (RFC 3325 §9.1) pairs a `tel` with a `sip`
/// one, and a request is addressed to a SIP node, not a phone number. With no
/// SIP URI the first URI of any scheme is used, as before.
///
/// Whether a `;` after a *bare* URI belongs to it depends on the header: on
/// `From`/`To`/`Contact` it is a header parameter (`;tag=…`, RFC 3261 §20.10)
/// and is cut; `P-Asserted-Identity` defines no header parameters (RFC 3325
/// §9.1), so there it is the URI's own.
///
/// `None` for a header that is absent or names no URI at all.
pub(crate) fn header_uri(req: &SipRequest, name: &str) -> Option<String> {
    use super::identity::{header_uri_values, HeaderParams};
    let params = if ["From", "To", "Contact"]
        .iter()
        .any(|h| h.eq_ignore_ascii_case(name))
    {
        HeaderParams::Allowed
    } else {
        HeaderParams::None
    };
    let values = header_uri_values(req, name, params);
    values
        .iter()
        .find(|v| {
            v.uri.split_once(':').is_some_and(|(scheme, _)| {
                scheme.eq_ignore_ascii_case("sip") || scheme.eq_ignore_ascii_case("sips")
            })
        })
        .or_else(|| values.first())
        .map(|v| v.uri.to_string())
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_client_reader_ends_when_the_network_closes_the_connection() {
        // Jio closes an idle Gm connection; with no keepalive the reader ending
        // is the only early sign, and it must be visible.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let transport =
            crate::ims::sip_client::SipTransport::connect(listener.local_addr().unwrap(), true)
                .unwrap();
        let (peer, _) = listener.accept().unwrap();
        let session =
            crate::ims::test_session_with_sa(Some(transport), listener.local_addr().unwrap(), None);
        let inbound = start_inbound(&session).unwrap();
        assert!(inbound.client_reader_alive());

        drop(peer);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while inbound.client_reader_alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !inbound.client_reader_alive(),
            "closing the connection stops its reader"
        );
    }
    use super::*;

    fn request_with(headers: &str) -> SipRequest {
        let raw = format!(
            "INVITE sip:x SIP/2.0\r\n{headers}Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
        );
        SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
    }

    /// FR-015: a request whose caller resolves leaves nothing to report —
    /// including one literally from a user named "unknown", which is a
    /// resolved number, not the placeholder.
    #[test]
    fn no_diagnostic_when_the_caller_resolves() {
        for headers in [
            "From: <tel:+919000000000>;tag=a\r\n",
            "From: <sip:gateway.ims.example>;tag=a\r\nP-Asserted-Identity: <tel:+919000000000>\r\n",
            "From: <sip:unknown@ims.example>;tag=a\r\n",
        ] {
            assert_eq!(
                unresolved_caller_diagnostic(&request_with(headers)),
                None,
                "{headers}"
            );
        }
    }

    #[test]
    fn diagnostic_names_both_raw_headers_when_no_number_resolves() {
        let req = request_with("From: <sip:gateway.ims.example>;tag=a\r\n");
        let text = unresolved_caller_diagnostic(&req).expect("an unresolved caller is reported");
        assert!(text.contains("P-Asserted-Identity=[]"), "{text}");
        assert!(text.contains("sip:gateway.ims.example"), "{text}");
    }

    /// A caller-supplied value must not be able to forge or split a log line.
    #[test]
    fn diagnostic_escapes_control_characters() {
        let req =
            request_with("P-Asserted-Identity: <sip:a%0D%0Ab@x>\r\nFrom: garbage\u{1}\u{7f}\t\r\n");
        let text = unresolved_caller_diagnostic(&req).expect("unresolved");
        assert!(text.contains("garbage"), "{text}");
        assert!(
            !text.chars().any(|c| c.is_control()),
            "control character leaked: {text:?}"
        );
    }

    /// A single-contact reginfo document, no IMEI needed to attribute it —
    /// the common case, and the shape of every network before Jio's paired
    /// Apple Watch example.
    #[test]
    fn a_single_contact_is_treated_as_ours_without_needing_the_imei() {
        let body = r#"<?xml version="1.0"?>
<reginfo xmlns="urn:ietf:params:xml:ns:reginfo" version="0" state="full">
  <registration aor="sip:+919000000000@ims.example" id="a1" state="active">
    <contact id="c1" state="active" event="registered" expires="3600">
      <uri>sip:+919000000000@10.0.0.1:5060;transport=tcp</uri>
    </contact>
  </registration>
</reginfo>"#;
        let own = find_own_contacts(body, "860000000000000", addr("10.0.0.1:5060"));
        assert_eq!(own.matched_by, Attribution::ContactAddr);
        assert_eq!(own.entries.len(), 1);
        assert_eq!(own.entries[0].state, "active");
        assert_eq!(own.entries[0].event, "registered");
        assert!(!own.deregistered());
    }

    /// Multiple contacts, ours identified by its `+sip.instance` IMEI —
    /// modelled on the Jio reginfo (a paired handset's contact alongside
    /// ours) that motivated matching by IMEI instead of always taking "the"
    /// contact.
    ///
    /// Issue #124 changed this fixture: it used to give each contact a `<uri>`
    /// at an address other than our current one and still expect the IMEI to
    /// claim it. That is exactly the retired-binding case, so the IMEI now
    /// only decides for entries carrying no address to check.
    #[test]
    fn our_contact_is_picked_out_by_imei_among_several() {
        let body = r#"<?xml version="1.0"?>
<reginfo xmlns="urn:ietf:params:xml:ns:reginfo" version="0" state="full">
  <registration aor="sip:+919000000000@ims.example" id="a1" state="active">
    <contact id="watch" state="active" event="registered" expires="3600">
      <unknown-param name="+sip.instance">"&lt;urn:gsma:imei:490154203237518&gt;"</unknown-param>
    </contact>
    <contact id="phone" state="terminated" event="deactivated" expires="0">
      <unknown-param name="+sip.instance">"&lt;urn:gsma:imei:860000000000000&gt;"</unknown-param>
    </contact>
  </registration>
</reginfo>"#;
        let own = find_own_contacts(body, "860000000000000", addr("10.0.0.9:5064"));
        assert_eq!(own.matched_by, Attribution::Imei);
        assert_eq!(own.entries.len(), 1);
        assert_eq!(own.entries[0].state, "terminated");
        assert_eq!(own.entries[0].event, "deactivated");
        assert!(own.deregistered());

        // The other device's own binding is untouched: a NOTIFY about it must
        // never be read as ours.
        let watch = find_own_contacts(body, "490154203237518", addr("10.0.0.9:5064"));
        assert_eq!(watch.matched_by, Attribution::Imei);
        assert_eq!(watch.entries.len(), 1);
        assert_eq!(watch.entries[0].state, "active");
        assert!(!watch.deregistered());
    }

    /// Several contacts and none mentioning our IMEI: give up rather than
    /// guess. Acting on a contact that isn't ours would force a
    /// re-registration this line never needed.
    #[test]
    fn multiple_unattributable_contacts_yield_none_rather_than_a_guess() {
        let body = r#"<reginfo xmlns="urn:ietf:params:xml:ns:reginfo" version="0" state="full">
  <registration aor="sip:+919000000000@ims.example" id="a1" state="active">
    <contact id="a" state="active" event="registered" expires="3600">
      <uri>sip:+919000000000@192.168.1.50:5060</uri>
    </contact>
    <contact id="b" state="terminated" event="rejected" expires="0">
      <uri>sip:+919000000000@10.0.0.1:5060</uri>
    </contact>
  </registration>
</reginfo>"#;
        let own = find_own_contacts(body, "860000000000000", addr("10.0.0.9:5064"));
        assert_eq!(own.matched_by, Attribution::None);
        assert!(own.entries.is_empty());
        assert!(!own.deregistered());
    }

    /// A self-closing `<contact/>` (no children at all) must still parse —
    /// RFC 3680's schema allows it, even though every real capture seen so
    /// far carries a `<uri>` child.
    #[test]
    fn a_self_closing_contact_element_is_parsed() {
        let body = r#"<reginfo><registration><contact id="c1" state="terminated" event="probation"/></registration></reginfo>"#;
        let own = find_own_contacts(body, "", addr("10.0.0.1:5060"));
        assert_eq!(own.matched_by, Attribution::SoleContact);
        assert_eq!(own.entries[0].state, "terminated");
        assert_eq!(own.entries[0].event, "probation");
        assert!(own.deregistered());
    }

    /// `expired` is our own scheduled renewal running its course, not the
    /// network dropping us — it must not trigger the same forced
    /// re-registration as a genuine deregistration.
    #[test]
    fn an_expired_event_is_not_treated_as_a_deregistration() {
        let contact = contact("terminated", "expired");
        assert!(!contact_reports_deregistration(&contact));
    }

    /// `state="active"` is never a deregistration, whatever `event` says —
    /// the state is the gate, not just the event name.
    #[test]
    fn an_active_contact_is_never_a_deregistration_even_with_a_deregistration_style_event() {
        let contact = contact("active", "deactivated");
        assert!(!contact_reports_deregistration(&contact));
    }

    /// Every event RFC 3680 §4.1 pairs with `state="terminated"` to mean the
    /// binding is genuinely gone, not just refreshed/shortened.
    #[test]
    fn every_deregistration_event_is_recognized() {
        for event in ["deactivated", "probation", "rejected", "unregistered"] {
            let contact = contact("terminated", event);
            assert!(
                contact_reports_deregistration(&contact),
                "{event} must be recognized as a deregistration"
            );
        }
    }

    /// No `<contact>` element at all (an empty or unrelated body) must not
    /// panic — it's simply nothing to attribute.
    #[test]
    fn a_body_with_no_contact_element_yields_none() {
        let ours = addr("10.0.0.1:5060");
        for body in ["<reginfo></reginfo>", ""] {
            let own = find_own_contacts(body, "860000000000000", ours);
            assert_eq!(own.matched_by, Attribution::None, "{body:?}");
        }
    }

    /// A `<contact` opening tag that never closes must not hang or panic —
    /// this is untrusted network input.
    #[test]
    fn a_truncated_contact_tag_does_not_panic() {
        let own = find_own_contacts(
            "<reginfo><contact state=\"terminated\"",
            "",
            addr("10.0.0.1:5060"),
        );
        assert_eq!(own.matched_by, Attribution::None);
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().expect("a literal socket address")
    }

    /// Issue #117's NOTIFY body, anonymized there and copied verbatim: one
    /// binding listed under both a SIP and a tel AOR, both `terminated/
    /// rejected`, no IMEI anywhere.
    const ISSUE_117_BODY: &str = r#"<?xml version="1.0"?>
<reginfo xmlns="urn:ietf:params:xml:ns:reginfo" version="1" state="full">
  <registration aor="sip:+919000000000@ims.example.invalid" id="registration-sip" state="terminated">
    <contact id="contact-1" state="terminated" event="rejected">
      <uri>sip:subscriber@198.51.100.10:42934;Hpt=route-token;suid=opaque-token;srti=opaque-token</uri>
    </contact>
  </registration>
  <registration aor="tel:+919000000000" id="registration-tel" state="terminated">
    <contact id="contact-1" state="terminated" event="rejected">
      <uri>sip:subscriber@198.51.100.10:42934;Hpt=route-token;suid=opaque-token;srti=opaque-token</uri>
    </contact>
  </registration>
</reginfo>"#;

    /// Issue #117: the same binding under two AORs used to be "several
    /// contacts, none attributable", so the deregistration was ignored.
    /// Matched by the address our Contact advertised, both entries are ours.
    #[test]
    fn our_binding_listed_under_sip_and_tel_aors_is_attributed_by_contact_address() {
        let own = find_own_contacts(
            ISSUE_117_BODY,
            "860000000000000",
            addr("198.51.100.10:42934"),
        );
        assert_eq!(own.matched_by, Attribution::ContactAddr);
        assert_eq!(own.entries.len(), 2);
        assert!(own.deregistered());
    }

    /// The same document naming a port our current registration doesn't use
    /// is a binding from a session a renewal has already replaced — not ours
    /// to act on.
    #[test]
    fn a_contact_on_another_port_is_not_ours() {
        let own = find_own_contacts(
            ISSUE_117_BODY,
            "860000000000000",
            addr("198.51.100.10:42936"),
        );
        assert_eq!(own.matched_by, Attribution::None);
        assert!(!own.deregistered());
    }

    /// A partial notification carries only the contacts that changed. When
    /// that is another device's, the old "the only contact must be ours"
    /// fallback read its deregistration as ours.
    #[test]
    fn a_partial_notify_about_another_device_is_not_ours() {
        let body = r#"<reginfo xmlns="urn:ietf:params:xml:ns:reginfo" version="3" state="partial">
  <registration aor="sip:+919000000000@ims.example" id="a1" state="active">
    <contact id="watch" state="terminated" event="rejected">
      <uri>sip:+919000000000@192.168.1.50:5060</uri>
    </contact>
  </registration>
</reginfo>"#;
        let own = find_own_contacts(body, "860000000000000", addr("10.0.0.1:5064"));
        assert_eq!(own.matched_by, Attribution::None);
        assert!(!own.deregistered());
    }

    /// Losing the binding under one public identity is a deregistration even
    /// while another still lists it active.
    #[test]
    fn one_terminated_entry_among_our_active_ones_is_a_deregistration() {
        let body = r#"<reginfo xmlns="urn:ietf:params:xml:ns:reginfo" version="2" state="full">
  <registration aor="sip:+919000000000@ims.example" id="a1" state="active">
    <contact id="c1" state="active" event="registered">
      <uri>sip:+919000000000@10.0.0.1:5064;transport=tcp</uri>
    </contact>
  </registration>
  <registration aor="tel:+919000000000" id="a2" state="terminated">
    <contact id="c1" state="terminated" event="unregistered">
      <uri>sip:+919000000000@10.0.0.1:5064;transport=tcp</uri>
    </contact>
  </registration>
</reginfo>"#;
        let own = find_own_contacts(body, "", addr("10.0.0.1:5064"));
        assert_eq!(own.entries.len(), 2);
        assert!(own.deregistered());
    }

    #[test]
    fn an_ipv6_contact_address_is_matched() {
        let body = r#"<reginfo><registration aor="sip:u@ims.example" id="a1" state="terminated">
    <contact id="c1" state="terminated" event="deactivated">
      <uri>sip:u@[2001:db8::1]:5064;transport=tcp</uri>
    </contact>
    <contact id="c2" state="active" event="registered">
      <uri>sip:u@[2001:db8::2]:5064</uri>
    </contact>
  </registration></reginfo>"#;
        let own = find_own_contacts(body, "", addr("[2001:db8::1]:5064"));
        assert_eq!(own.matched_by, Attribution::ContactAddr);
        assert_eq!(own.entries.len(), 1);
        assert!(own.deregistered());
    }

    #[test]
    fn contact_uri_addr_reads_only_a_literal_address_with_a_port() {
        let block = |uri: &str| format!("<contact state=\"active\"><uri>{uri}</uri></contact>");
        assert_eq!(
            contact_uri_addr(&block("sips:u@10.0.0.1:5061;transport=tls")),
            Some(addr("10.0.0.1:5061"))
        );
        assert_eq!(
            contact_uri_addr(&block("sip:10.0.0.1:5060")),
            Some(addr("10.0.0.1:5060"))
        );
        assert_eq!(contact_uri_addr(&block("sip:u@10.0.0.1")), None);
        assert_eq!(contact_uri_addr(&block("sip:u@pcscf.example:5060")), None);
        assert_eq!(contact_uri_addr(&block("tel:+919000000000")), None);
        assert_eq!(contact_uri_addr("<contact state=\"active\"/>"), None);
    }

    /// The IMEI echoed under one public identity must not hide our entry
    /// under another that is recognisable only by its address — that one is
    /// the one reporting the loss here.
    #[test]
    fn imei_and_address_matches_are_collected_together() {
        let body = r#"<reginfo xmlns="urn:ietf:params:xml:ns:reginfo" version="2" state="full">
  <registration aor="sip:+919000000000@ims.example" id="a1" state="active">
    <contact id="c1" state="active" event="registered">
      <uri>sip:+919000000000@10.0.0.1:5064</uri>
      <unknown-param name="+sip.instance">"&lt;urn:gsma:imei:860000000000000&gt;"</unknown-param>
    </contact>
  </registration>
  <registration aor="tel:+919000000000" id="a2" state="terminated">
    <contact id="c1" state="terminated" event="rejected">
      <uri>sip:+919000000000@10.0.0.1:5064</uri>
    </contact>
  </registration>
</reginfo>"#;
        let own = find_own_contacts(body, "860000000000000", addr("10.0.0.1:5064"));
        // Both entries name our address, so the address is what is reported
        // even though one also echoes the IMEI (issue #124: the address is
        // the stronger proof, and the label says which proof was used).
        assert_eq!(own.matched_by, Attribution::ContactAddr);
        assert_eq!(own.entries.len(), 2);
        assert!(own.retired.is_empty());
        assert!(own.deregistered());
    }

    fn contact(state: &str, event: &str) -> ReginfoContact {
        ReginfoContact {
            state: state.to_string(),
            event: event.to_string(),
            addr: None,
            retry_after: None,
            registration_state: None,
        }
    }

    /// The only contact of `body`, as parsed.
    fn only_contact(body: &str) -> ReginfoContact {
        let blocks = contact_blocks(body);
        assert_eq!(blocks.len(), 1, "{body}");
        parse_contact_attrs(&blocks[0]).expect("a parseable contact")
    }

    #[test]
    fn a_contact_carries_its_uri_address() {
        let c = only_contact(
            r#"<contact state="active" event="registered"><uri>sip:+919000000000@10.0.0.1:5070;transport=tcp</uri></contact>"#,
        );
        assert_eq!(c.addr, Some(addr("10.0.0.1:5070")));
        let c = only_contact(
            r#"<contact state="active" event="registered"><uri>sip:+919000000000@[2001:db8::1]:5070</uri></contact>"#,
        );
        assert_eq!(c.addr, Some(addr("[2001:db8::1]:5070")));
    }

    #[test]
    fn a_contact_without_a_literal_address_has_none() {
        let c = only_contact(r#"<contact state="active" event="registered"/>"#);
        assert_eq!(c.addr, None);
        let c = only_contact(
            r#"<contact state="active" event="registered"><uri>sip:u@pcscf.example:5060</uri></contact>"#,
        );
        assert_eq!(c.addr, None);
    }

    #[test]
    fn a_contact_carries_its_retry_after() {
        let c = only_contact(
            r#"<contact state="terminated" event="probation" retry-after="600"><uri>sip:u@10.0.0.1:5070</uri></contact>"#,
        );
        assert_eq!(c.retry_after, Some(600));
        assert_eq!(
            only_contact(r#"<contact state="active"/>"#).retry_after,
            None
        );
    }

    #[test]
    fn a_contact_reads_its_enclosing_registration_state() {
        let bare = only_contact(r#"<reginfo><contact state="active"/></reginfo>"#);
        assert_eq!(bare.registration_state, None);

        let body = r#"<reginfo>
  <registration aor="sip:+919000000000@ims.example" id="a1" state="active">
    <contact id="c1" state="active" event="registered"/>
  </registration>
  <registration aor="tel:+919000000000" id="a2" state="terminated">
    <contact id="c2" state="terminated" event="unregistered"/>
  </registration>
</reginfo>"#;
        let contacts: Vec<_> = contact_blocks(body)
            .iter()
            .filter_map(parse_contact_attrs)
            .collect();
        assert_eq!(contacts.len(), 2);
        assert_eq!(contacts[0].registration_state.as_deref(), Some("active"));
        // The second registration's state, not the first's.
        assert_eq!(
            contacts[1].registration_state.as_deref(),
            Some("terminated")
        );
    }

    #[test]
    fn a_contact_after_a_closed_registration_is_not_nested_in_it() {
        let body = r#"<reginfo><registration state="terminated"></registration><contact state="active"/></reginfo>"#;
        assert_eq!(only_contact(body).registration_state, None);
    }

    #[test]
    fn dereg_event_parses_only_terminated_deregistrations() {
        assert_eq!(
            DeregEvent::from_contact(&contact("TERMINATED", "Rejected")),
            Some(DeregEvent::Rejected)
        );
        assert_eq!(
            DeregEvent::from_contact(&contact("terminated", "deactivated")),
            Some(DeregEvent::Deactivated)
        );
        assert_eq!(
            DeregEvent::from_contact(&contact("terminated", "probation")),
            Some(DeregEvent::Probation)
        );
        assert_eq!(
            DeregEvent::from_contact(&contact("terminated", "unregistered")),
            Some(DeregEvent::Unregistered)
        );
        for (state, event) in [
            ("terminated", "expired"),
            ("terminated", "registered"),
            ("active", "rejected"),
            ("active", "registered"),
        ] {
            assert_eq!(
                DeregEvent::from_contact(&contact(state, event)),
                None,
                "{state}/{event}"
            );
        }
    }

    /// FR-014: the most restrictive event wins.
    #[test]
    fn the_most_restrictive_dereg_event_is_the_max() {
        let events = [
            DeregEvent::Unregistered,
            DeregEvent::Rejected,
            DeregEvent::Deactivated,
        ];
        assert_eq!(events.into_iter().max(), Some(DeregEvent::Rejected));
        assert!(DeregEvent::Unregistered < DeregEvent::Probation);
        assert!(DeregEvent::Probation < DeregEvent::Deactivated);
        assert!(DeregEvent::Deactivated < DeregEvent::Rejected);
    }

    const IMEI: &str = "860000000000000";

    /// `classify_reg_notify` finds exactly what `find_own_contacts` does, on
    /// every fixture in this module.
    #[test]
    fn classification_attributes_like_find_own_contacts() {
        for (body, current) in [
            (ISSUE_117_BODY, "198.51.100.10:42934"),
            (ISSUE_117_BODY, "198.51.100.10:42936"),
            (
                r#"<reginfo><registration><contact id="c1" state="terminated" event="probation"/></registration></reginfo>"#,
                "10.0.0.1:5060",
            ),
            ("<reginfo></reginfo>", "10.0.0.1:5060"),
        ] {
            let verdict = classify_reg_notify(body, "active", IMEI, addr(current));
            assert_eq!(verdict.own, find_own_contacts(body, IMEI, addr(current)));
            assert_eq!(
                verdict.deregistration.is_some(),
                verdict.own.deregistered(),
                "{body}"
            );
        }
    }

    #[test]
    fn classification_picks_the_most_restrictive_event_and_longest_retry_after() {
        let body = r#"<reginfo>
  <registration aor="sip:+919000000000@ims.example" id="a1" state="terminated">
    <contact id="c1" state="terminated" event="probation" retry-after="600">
      <uri>sip:+919000000000@10.0.0.1:5070</uri>
    </contact>
  </registration>
  <registration aor="tel:+919000000000" id="a2" state="terminated">
    <contact id="c1" state="terminated" event="rejected" retry-after="300">
      <uri>sip:+919000000000@10.0.0.1:5070</uri>
    </contact>
  </registration>
</reginfo>"#;
        let current = addr("10.0.0.1:5070");
        let d = classify_reg_notify(body, "active;expires=600000", IMEI, current)
            .deregistration
            .expect("a deregistration");
        assert_eq!(d.event, DeregEvent::Rejected);
        assert_eq!(d.retry_after, Some(Duration::from_secs(600)));

        let d = classify_reg_notify(
            body,
            "terminated;reason=probation;retry-after=900",
            IMEI,
            current,
        )
        .deregistration
        .expect("a deregistration");
        assert_eq!(d.retry_after, Some(Duration::from_secs(900)));
    }

    #[test]
    fn classification_finds_no_deregistration_when_our_entries_are_active_or_expired() {
        let body = r#"<reginfo>
  <registration aor="sip:+919000000000@ims.example" id="a1" state="active">
    <contact id="c1" state="active" event="registered"><uri>sip:+919000000000@10.0.0.1:5070</uri></contact>
    <contact id="c0" state="terminated" event="expired"><uri>sip:+919000000000@10.0.0.1:5070</uri></contact>
  </registration>
</reginfo>"#;
        let verdict = classify_reg_notify(body, "active", IMEI, addr("10.0.0.1:5070"));
        assert_eq!(verdict.own.entries.len(), 2);
        assert_eq!(verdict.deregistration, None);
    }

    #[test]
    fn subscription_state_retry_after_is_read_from_its_parameters() {
        let retry_after = |v: &str| parse_subscription_state(v).retry_after;
        assert_eq!(
            retry_after("terminated;reason=probation;retry-after=900"),
            Some(900)
        );
        assert_eq!(retry_after("terminated; Retry-After = 60"), Some(60));
        assert_eq!(retry_after("active;expires=600000"), None);
        assert_eq!(retry_after("retry-after=5"), None);
    }

    fn sub_state(
        state: SubStateKind,
        expires: Option<u64>,
        reason: Option<&str>,
        retry_after: Option<u64>,
    ) -> SubState {
        SubState {
            state,
            expires,
            reason: reason.map(str::to_string),
            retry_after,
        }
    }

    /// specs/090 R6: a refused NOTIFY is answered `481` with the request's own
    /// dialog headers echoed, so the notifier can match it.
    #[test]
    fn respond_notify_answers_481_on_the_requests_own_headers() {
        let peer = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let transport =
            super::super::sip_client::SipTransport::connect(peer.local_addr().unwrap(), false)
                .unwrap();
        let raw = "NOTIFY sip:x SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.8:5060;branch=z9hG4bKn1\r\n\
                   From: <sip:a@ims.example>;tag=rtag-1\r\nTo: <sip:a@ims.example>;tag=ltag-x\r\n\
                   Call-ID: stray\r\nCSeq: 4 NOTIFY\r\nEvent: reg\r\nContent-Length: 0\r\n\r\n";
        let req = SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0;
        for (status, line) in [
            (481, "SIP/2.0 481 Call/Transaction Does Not Exist\r\n"),
            (200, "SIP/2.0 200 OK\r\n"),
        ] {
            respond_notify(&transport.sink().unwrap(), &req, status);
            let mut buf = [0u8; 2048];
            let (n, _) = peer.recv_from(&mut buf).unwrap();
            let sent = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(sent.starts_with(line), "{sent}");
            for h in [
                "Via: SIP/2.0/UDP 10.0.0.8:5060;branch=z9hG4bKn1",
                "From: <sip:a@ims.example>;tag=rtag-1\r\n",
                "To: <sip:a@ims.example>;tag=ltag-x\r\n",
                "Call-ID: stray\r\n",
                "CSeq: 4 NOTIFY\r\n",
            ] {
                assert!(sent.contains(h), "missing {h:?} in {sent}");
            }
        }
    }

    /// specs/090 R7: every shape the subscription acts on.
    #[test]
    fn subscription_state_is_parsed_into_its_substate_and_parameters() {
        assert_eq!(
            parse_subscription_state("active;expires=600"),
            sub_state(SubStateKind::Active, Some(600), None, None)
        );
        assert_eq!(
            parse_subscription_state("pending"),
            sub_state(SubStateKind::Pending, None, None, None)
        );
        assert_eq!(
            parse_subscription_state("terminated;reason=probation;retry-after=300"),
            sub_state(SubStateKind::Terminated, None, Some("probation"), Some(300))
        );
        assert_eq!(
            parse_subscription_state("Terminated; Reason=NoResource"),
            sub_state(SubStateKind::Terminated, None, Some("noresource"), None)
        );
        assert_eq!(
            parse_subscription_state("terminated"),
            sub_state(SubStateKind::Terminated, None, None, None)
        );
        assert_eq!(parse_subscription_state("").state, SubStateKind::Missing);
        assert_eq!(parse_subscription_state("  ").state, SubStateKind::Missing);
        assert_eq!(
            parse_subscription_state("active;expires=soon;retry-after=x"),
            sub_state(SubStateKind::Active, None, None, None)
        );
        // An absurd value is clamped rather than allowed to overflow an
        // `Instant` downstream.
        assert_eq!(
            parse_subscription_state(
                "terminated;reason=probation;retry-after=18446744073709551615"
            )
            .retry_after,
            Some(MAX_NETWORK_DELAY_SECS)
        );
        assert_eq!(
            parse_subscription_state("active;expires=18446744073709551615").expires,
            Some(MAX_NETWORK_DELAY_SECS)
        );
    }

    const OLD: &str = "10.0.0.1:5064";
    const CURRENT: &str = "10.0.0.1:5070";

    /// One `<contact>` element at `ip_port` echoing our IMEI.
    fn imei_contact(id: &str, state: &str, event: &str, ip_port: &str) -> String {
        format!(
            r#"<contact id="{id}" state="{state}" event="{event}">
      <uri>sip:+919000000000@{ip_port};transport=tcp</uri>
      <unknown-param name="+sip.instance">"&lt;urn:gsma:imei:{IMEI}&gt;"</unknown-param>
    </contact>"#
        )
    }

    fn reginfo(registration_state: &str, contacts: &[String]) -> String {
        format!(
            r#"<reginfo xmlns="urn:ietf:params:xml:ns:reginfo" version="4" state="full">
  <registration aor="sip:+919000000000@ims.example" id="a1" state="{registration_state}">
    {}
  </registration>
</reginfo>"#,
            contacts.join("\n    ")
        )
    }

    /// Research R1, scenario 1 — issue #124 itself: after a renewal moved us
    /// to `:5070`, the network retires the `:5064` binding, still echoing our
    /// IMEI. That is not our current binding, and nothing is to be done.
    #[test]
    fn a_retired_previous_binding_is_not_our_current_binding() {
        let body = reginfo(
            "active",
            &[imei_contact("old", "terminated", "rejected", OLD)],
        );
        let verdict = classify_reg_notify(&body, "active;expires=600000", IMEI, addr(CURRENT));
        assert!(verdict.own.entries.is_empty());
        assert_eq!(verdict.own.retired.len(), 1);
        assert_eq!(verdict.own.retired[0].addr, Some(addr(OLD)));
        assert_eq!(verdict.own.matched_by, Attribution::None);
        assert_eq!(verdict.deregistration, None);
    }

    /// R1 scenario 2: a full-state document listing both the retired binding
    /// and the one we hold now.
    #[test]
    fn the_old_binding_is_retired_while_the_new_one_is_current() {
        let body = reginfo(
            "active",
            &[
                imei_contact("old", "terminated", "unregistered", OLD),
                imei_contact("new", "active", "registered", CURRENT),
            ],
        );
        let verdict = classify_reg_notify(&body, "active", IMEI, addr(CURRENT));
        assert_eq!(verdict.own.matched_by, Attribution::ContactAddr);
        assert_eq!(verdict.own.entries.len(), 1);
        assert_eq!(verdict.own.entries[0].state, "active");
        assert_eq!(verdict.own.retired.len(), 1);
        assert_eq!(verdict.deregistration, None);
    }

    /// R1 scenario 3: the network really did deactivate the binding we hold.
    #[test]
    fn our_current_binding_deactivated_is_a_deregistration() {
        let body = reginfo(
            "active",
            &[imei_contact("new", "terminated", "deactivated", CURRENT)],
        );
        let verdict = classify_reg_notify(&body, "active", IMEI, addr(CURRENT));
        assert_eq!(verdict.own.matched_by, Attribution::ContactAddr);
        assert_eq!(
            verdict.deregistration.map(|d| d.event),
            Some(DeregEvent::Deactivated)
        );
    }

    /// R1 scenario 4 (issue #117, SC-005): both AOR entries name our current
    /// address, and both stay ours.
    #[test]
    fn both_aor_entries_at_our_address_stay_current() {
        let verdict =
            classify_reg_notify(ISSUE_117_BODY, "active", IMEI, addr("198.51.100.10:42934"));
        assert_eq!(verdict.own.entries.len(), 2);
        assert!(verdict.own.retired.is_empty());
        assert_eq!(
            verdict.deregistration.map(|d| d.event),
            Some(DeregEvent::Rejected)
        );
    }

    /// An entry echoing our IMEI with no address to check cannot be proven to
    /// be an old binding, so it stays ours — next to someone else's contact.
    #[test]
    fn an_imei_entry_without_an_address_is_current() {
        let body = format!(
            r#"<reginfo><registration state="active">
    <contact id="other" state="active" event="registered"><uri>sip:u@192.0.2.7:5060</uri></contact>
    <contact id="ours" state="terminated" event="probation">
      <unknown-param name="+sip.instance">"&lt;urn:gsma:imei:{IMEI}&gt;"</unknown-param>
    </contact>
  </registration></reginfo>"#
        );
        let verdict = classify_reg_notify(&body, "active", IMEI, addr(CURRENT));
        assert_eq!(verdict.own.matched_by, Attribution::Imei);
        assert_eq!(verdict.own.entries.len(), 1);
        assert_eq!(verdict.own.entries[0].event, "probation");
        assert!(verdict.own.retired.is_empty());
    }

    /// Another device's binding is neither ours now nor ours before.
    #[test]
    fn a_foreign_address_without_our_imei_is_neither_current_nor_retired() {
        let body = reginfo(
            "active",
            &[
                r#"<contact id="other" state="terminated" event="rejected"><uri>sip:u@192.0.2.7:5060</uri></contact>"#
                    .to_string(),
                imei_contact("new", "active", "registered", CURRENT),
            ],
        );
        let verdict = classify_reg_notify(&body, "active", IMEI, addr(CURRENT));
        assert_eq!(verdict.own.entries.len(), 1);
        assert!(verdict.own.retired.is_empty());
        assert_eq!(verdict.deregistration, None);
    }

    /// An IPv6 current address is matched, and a `;Hpt=` suffix the network
    /// appends does not change which binding the entry names.
    #[test]
    fn an_ipv6_current_address_with_uri_parameters_is_matched() {
        let body = format!(
            r#"<reginfo><registration state="active">
    <contact id="new" state="active" event="registered">
      <uri>sip:+919000000000@[2001:db8::1]:5070;Hpt=8f_ex;transport=tcp</uri>
      <unknown-param name="+sip.instance">"&lt;urn:gsma:imei:{IMEI}&gt;"</unknown-param>
    </contact>
    <contact id="old" state="terminated" event="unregistered">
      <uri>sip:+919000000000@[2001:db8::1]:5064;Hpt=8f_ex</uri>
      <unknown-param name="+sip.instance">"&lt;urn:gsma:imei:{IMEI}&gt;"</unknown-param>
    </contact>
  </registration></reginfo>"#
        );
        let verdict = classify_reg_notify(&body, "active", IMEI, addr("[2001:db8::1]:5070"));
        assert_eq!(verdict.own.matched_by, Attribution::ContactAddr);
        assert_eq!(verdict.own.entries.len(), 1);
        assert_eq!(verdict.own.retired.len(), 1);
        assert_eq!(verdict.deregistration, None);
    }

    /// Story 3, scenario 1 (FR-015): the identity itself is deregistered.
    #[test]
    fn a_terminated_registration_is_an_identity_deregistration() {
        let body = reginfo(
            "terminated",
            &[imei_contact("new", "terminated", "deactivated", CURRENT)],
        );
        let d = classify_reg_notify(&body, "active", IMEI, addr(CURRENT))
            .deregistration
            .expect("a deregistration");
        assert_eq!(d.scope, DeregScope::IdentityDeregistered);
        assert_eq!(d.event, DeregEvent::Deactivated);
    }

    /// Story 3, scenario 2: the identity stays registered through another
    /// device; only our binding was removed.
    #[test]
    fn our_binding_removed_while_the_identity_stays_registered() {
        let body = reginfo(
            "active",
            &[
                imei_contact("new", "terminated", "rejected", CURRENT),
                r#"<contact id="other" state="active" event="registered"><uri>sip:u@192.0.2.7:5060</uri></contact>"#
                    .to_string(),
            ],
        );
        let d = classify_reg_notify(&body, "active", IMEI, addr(CURRENT))
            .deregistration
            .expect("a deregistration");
        assert_eq!(d.scope, DeregScope::BindingRemoved);
        assert_eq!(d.event, DeregEvent::Rejected);
    }

    /// Issue #117's body: both registrations holding our binding terminated.
    #[test]
    fn both_aors_terminated_is_an_identity_deregistration() {
        let d = classify_reg_notify(ISSUE_117_BODY, "active", IMEI, addr("198.51.100.10:42934"))
            .deregistration
            .expect("a deregistration");
        assert_eq!(d.scope, DeregScope::IdentityDeregistered);
    }

    /// A contact not nested in any `<registration>` says nothing about the
    /// identity, so only the binding is known to be gone.
    #[test]
    fn a_bare_contact_is_a_removed_binding() {
        let body = r#"<reginfo><contact id="c1" state="terminated" event="probation"/></reginfo>"#;
        let d = classify_reg_notify(body, "active", "", addr(CURRENT))
            .deregistration
            .expect("a deregistration");
        assert_eq!(d.scope, DeregScope::BindingRemoved);
    }

    // ---- specs/101: the SMS delivery report's routing and correlation ----

    /// Send one delivery report from a session over a loopback UDP socket and
    /// return what arrived, plus the `Call-ID` the sender reported.
    fn sent_report(
        verify: Option<&str>,
        service_routes: &[&str],
        in_reply_to: Option<&str>,
        pcscf_route: bool,
    ) -> (String, Option<String>) {
        let peer = std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let addr = peer.local_addr().unwrap();
        let transport = crate::ims::sip_client::SipTransport::connect(addr, false).unwrap();
        let mut session = crate::ims::test_session_with_sa(Some(transport), addr, verify);
        for r in service_routes {
            session
                .headers
                .push(("Service-Route".to_string(), (*r).to_string()));
        }
        let call_id = send_sms_delivery_report(
            &mut session,
            "sip:ip-sm-gw@mmtel.ims.example.org",
            &[0x02, 0x01, 0x41, 0x02, 0x00, 0x00],
            in_reply_to,
            pcscf_route,
        );
        let mut buf = [0u8; 4096];
        let n = peer.recv(&mut buf).unwrap();
        (String::from_utf8_lossy(&buf[..n]).into_owned(), call_id)
    }

    fn route_lines(msg: &str) -> Vec<&str> {
        msg.lines().filter(|l| l.starts_with("Route:")).collect()
    }

    #[test]
    fn report_names_the_inbound_message_and_returns_its_own_call_id() {
        let inbound = "asbc5lyog6o3.coysc@tas.ims.example.org.23";
        let (msg, call_id) = sent_report(None, &[], Some(inbound), true);
        assert!(
            msg.contains(&format!("\r\nIn-Reply-To: {inbound}\r\n")),
            "{msg}"
        );
        let call_id = call_id.expect("a written report returns its Call-ID");
        assert!(
            msg.contains(&format!("\r\nCall-ID: {call_id}\r\n")),
            "{msg}"
        );
    }

    #[test]
    fn report_without_an_inbound_call_id_has_no_in_reply_to() {
        let (msg, _) = sent_report(None, &[], None, true);
        assert!(!msg.contains("In-Reply-To"), "{msg}");
    }

    #[test]
    fn pcscf_is_the_only_route_when_there_is_no_service_route() {
        let (msg, _) = sent_report(None, &[], None, true);
        assert_eq!(route_lines(&msg).len(), 1, "{msg}");
        assert!(
            route_lines(&msg)[0].starts_with("Route: <sip:127.0.0.1:"),
            "{msg}"
        );
        assert!(route_lines(&msg)[0].ends_with(";lr>"), "{msg}");
    }

    #[test]
    fn pcscf_route_precedes_the_service_route_entries_in_order() {
        let (msg, _) = sent_report(
            None,
            &[
                "<sip:orig@scscf1.example.org;lr>",
                "<sip:orig@scscf2.example.org;lr>",
            ],
            None,
            true,
        );
        let routes = route_lines(&msg);
        assert_eq!(routes.len(), 3, "{msg}");
        assert!(routes[0].starts_with("Route: <sip:127.0.0.1:"), "{msg}");
        assert_eq!(routes[1], "Route: <sip:orig@scscf1.example.org;lr>");
        assert_eq!(routes[2], "Route: <sip:orig@scscf2.example.org;lr>");
    }

    #[test]
    fn with_a_gm_sa_the_route_uses_the_protected_server_port() {
        // `test_session_with_sa` negotiates the P-CSCF's `port-s` as 6001.
        let (msg, _) = sent_report(Some("v"), &[], None, true);
        assert_eq!(
            route_lines(&msg),
            vec!["Route: <sip:127.0.0.1:6001;lr>"],
            "{msg}"
        );
        // Without an SA it is the address registered through.
        let (msg, _) = sent_report(None, &[], None, true);
        assert!(!route_lines(&msg)[0].contains(":6001"), "{msg}");
    }

    #[test]
    fn an_ipv6_pcscf_is_bracketed() {
        let addr: std::net::SocketAddr = "[2001:db8::7]:5060".parse().unwrap();
        let session = crate::ims::test_session_with_sa(None, addr, Some("v"));
        assert_eq!(
            pcscf_route_header(&session),
            "Route: <sip:[2001:db8::7]:6001;lr>"
        );
    }

    #[test]
    fn with_the_setting_off_only_the_service_route_remains() {
        let (msg, _) = sent_report(None, &["<sip:orig@scscf.example.org;lr>"], None, false);
        assert_eq!(
            route_lines(&msg),
            vec!["Route: <sip:orig@scscf.example.org;lr>"],
            "{msg}"
        );
        let (msg, _) = sent_report(None, &[], None, false);
        assert!(route_lines(&msg).is_empty(), "{msg}");
    }

    #[test]
    fn security_headers_are_the_same_in_every_variant() {
        for (verify, pcscf_route, irt) in [
            (Some("sv-echo"), true, Some("x@y")),
            (Some("sv-echo"), false, None),
        ] {
            let (msg, _) = sent_report(verify, &[], irt, pcscf_route);
            assert!(msg.contains("Require: sec-agree\r\n"), "{msg}");
            assert!(msg.contains("Proxy-Require: sec-agree\r\n"), "{msg}");
            assert!(msg.contains("Security-Verify: sv-echo\r\n"), "{msg}");
        }
    }

    /// SC-003: against the pre-change shape, the only differences are the
    /// added `Route` and `In-Reply-To` lines (Call-ID, tag, branch are random
    /// per report and so normalised away).
    #[test]
    fn the_only_wire_differences_are_the_route_and_in_reply_to() {
        fn normalised(msg: &str) -> Vec<String> {
            msg.lines()
                .map(|l| {
                    if l.starts_with("Via:") || l.starts_with("From:") || l.starts_with("Call-ID:")
                    {
                        l.split([';', ':']).next().unwrap().to_string()
                    } else {
                        l.to_string()
                    }
                })
                .collect()
        }
        let svc = ["<sip:orig@scscf.example.org;lr>"];
        let (before, _) = sent_report(Some("sv"), &svc, None, false);
        let (after, _) = sent_report(Some("sv"), &svc, Some("inbound@x"), true);
        let before = normalised(&before);
        let after = normalised(&after);
        let added: Vec<_> = after.iter().filter(|l| !before.contains(l)).collect();
        assert_eq!(added.len(), 2, "{added:?}");
        assert!(added
            .iter()
            .any(|l| l.starts_with("Route: <sip:127.0.0.1:6001;lr>")));
        assert!(added.iter().any(|l| l.as_str() == "In-Reply-To: inbound@x"));
        assert!(before.iter().all(|l| after.contains(l)));
    }
}
