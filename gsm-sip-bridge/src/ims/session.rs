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
    build_uas_response, format_sip_addr, random_hex, spawn_gm_server, GmServer, SipMessage,
    SipRequest, SipSink,
};
use super::ImsRegisterConfig;
use crate::control::protocol::RegistrationStatus;
use crate::error::{BridgeError, BridgeResult};
use std::net::SocketAddr;
use std::sync::mpsc;
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
    /// Held only for its `Drop`, which shuts the listener down. Replaced
    /// wholesale on re-registration, since a renewal negotiates a fresh SA
    /// on a fresh pair of ports.
    pub(crate) _server: Option<GmServer>,
}

/// Start reading both halves of the Gm association for `session`:
///
/// - the **client** connection we registered over, which carries responses
///   to requests *we* originate (e.g. the reg-event SUBSCRIBE); and
/// - the **protected server port** (`port-s`), which is the only place the
///   network delivers anything it originates — including inbound `INVITE`s.
///   Without it a registration looks healthy but is unreachable; see
///   `sip_client::spawn_gm_server`.
pub(crate) fn start_inbound(session: &super::RegisteredSession) -> BridgeResult<Inbound> {
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
) -> BridgeResult<super::RegisteredSession> {
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
}

pub(crate) fn build_subscribe(p: &SubscribeParts) -> String {
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
pub(crate) fn subscribe_reg_event(session: &mut super::RegisteredSession) {
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
pub(crate) fn handle_notify(sink: &SipSink, req: &SipRequest) {
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

/// Answers on the connection a request arrived on, logging rather than
/// propagating a send failure — a broken connection is already terminal for
/// that dialog, and every caller is on a path where there is nothing better
/// to do about it.
pub(crate) fn respond(sink: &SipSink, what: &str, message: &str) {
    if let Err(e) = sink.send(message) {
        tracing::warn!(error = %e, response = %what, "failed to send SIP response");
    }
}

pub(crate) fn extract_caller(req: &SipRequest) -> String {
    req.header("From")
        .and_then(|f| f.split("sip:").nth(1))
        .and_then(|rest| rest.split(['@', ';', '>']).next())
        .unwrap_or("unknown")
        .to_string()
}
