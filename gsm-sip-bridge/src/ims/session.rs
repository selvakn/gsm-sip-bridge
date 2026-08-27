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
    build_message, build_uas_response, format_sip_addr, random_hex, spawn_gm_server, GmServer,
    MessageRequest, SipMessage, SipRequest, SipSink,
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
    /// Retained so `restart_client_reader` can feed a replacement reader
    /// thread into the same queue after the client transport is swapped —
    /// without this, restarting just the client half would need a new
    /// channel, which would orphan `_server`'s already-spawned accept-loop
    /// threads (they hold their own clone of the original sender).
    tx: mpsc::Sender<(SipMessage, SipSink)>,
    /// Held only for its `Drop`, which shuts the listener down. Replaced
    /// wholesale on re-registration, since a renewal negotiates a fresh SA
    /// on a fresh pair of ports.
    pub(crate) _server: Option<GmServer>,
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
) -> BridgeResult<()> {
    let mut client_reader = session.transport()?.try_clone_reader()?;
    let client_sink = session.transport()?.sink()?;
    std::thread::spawn(move || loop {
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
    });
    Ok(())
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

    spawn_client_reader(session, tx.clone())?;

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
    spawn_client_reader(session, inbound.tx.clone())
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
    /// This line's real access-network type (`ImsRegisterConfig::access_network_info`
    /// — `"3GPP-WLAN"` for VoWiFi, a real E-UTRAN value for VoLTE), echoed
    /// into `P-Access-Network-Info` instead of a hardcoded value
    /// (specs/045 MT-11).
    pub(crate) access_network_info: &'a str,
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
pub(crate) fn subscribe_reg_event(session: &mut super::RegisteredSession, access_network_info: &str) {
    let impu = session
        .default_impu()
        .unwrap_or_else(|| format!("sip:{}", session.public_uri));
    let route_headers = service_route_headers(session);
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
        access_network_info,
    });
    match session.transport_mut().and_then(|t| t.send(&msg)) {
        Ok(()) => tracing::info!(impu = %impu, "sent reg-event SUBSCRIBE"),
        Err(e) => tracing::warn!(error = %e, "failed to send reg-event SUBSCRIBE"),
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
/// Best-effort, like [`subscribe_reg_event`]: the `202 Accepted` comes back
/// asynchronously on the shared transport and is handled by the dispatch
/// loop, and a send failure costs a redelivery, which the caller's dedupe
/// absorbs.
pub(crate) fn send_sms_delivery_report(
    session: &mut super::RegisteredSession,
    ipsmgw_uri: &str,
    rp_ack: &[u8],
) {
    let impu = session
        .default_impu()
        .unwrap_or_else(|| format!("sip:{}", session.public_uri));
    let route_headers = service_route_headers(session);
    let cseq = session.cseq;
    session.cseq += 1;
    let msg = build_message(&MessageRequest {
        target_uri: ipsmgw_uri,
        impu: &impu,
        route_headers: &route_headers,
        local_addr: session.local_addr,
        transport: if session.use_tcp { "TCP" } else { "UDP" },
        call_id: &random_hex(8),
        from_tag: &random_hex(4),
        branch: &format!("z9hG4bK{}", random_hex(6)),
        cseq,
        content_type: "application/vnd.3gpp.sms",
        body: rp_ack,
    });
    match session.transport_mut().and_then(|t| t.send_bytes(&msg)) {
        Ok(()) => tracing::info!(
            ipsmgw = %ipsmgw_uri,
            rp_ack = %rp_ack.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "sent the SMS delivery report"
        ),
        // Never silently swallowed: an undelivered report is exactly what
        // makes a message centre redeliver, and it is invisible otherwise.
        Err(e) => tracing::warn!(
            error = %e,
            ipsmgw = %ipsmgw_uri,
            "failed to send the SMS delivery report; the network may redeliver this message"
        ),
    }
}

/// One `<contact>` element's `state`/`event` attributes from a reg-event
/// NOTIFY body (RFC 3680 §4.1 / TS 24.229 §5.1.1.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReginfoContact {
    pub(crate) state: String,
    pub(crate) event: String,
}

/// Every `<contact ...>` element in `body`, as its raw text — the opening
/// tag alone when self-closing, or through the matching `</contact>`
/// otherwise. Not a general XML parser: just enough of reginfo's shape
/// ([`find_own_contact`]'s only caller) to isolate one element at a time so
/// its `state`/`event` attributes can be read without another element's
/// same-named attribute (there are none in this schema, but nothing here
/// assumes that) getting matched first.
fn contact_blocks(body: &str) -> Vec<&str> {
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
        blocks.push(block);
        cursor = next_cursor;
        if cursor >= body.len() {
            break;
        }
    }
    blocks
}

/// The value of `name="..."` anywhere in `tag` — attribute order isn't
/// assumed, only that the value itself contains no `"`.
fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

fn parse_contact_attrs(block: &str) -> Option<ReginfoContact> {
    Some(ReginfoContact {
        state: extract_attr(block, "state")?,
        event: extract_attr(block, "event").unwrap_or_default(),
    })
}

/// Finds the `<contact>` element in a reg-event NOTIFY body that is *ours*,
/// and returns its `state`/`event` attributes.
///
/// Matched by whether the element mentions our own IMEI — every `Contact` we
/// register carries `+sip.instance="<urn:gsma:imei:...>"`, and a registrar's
/// reginfo NOTIFY commonly echoes a contact's own parameters back in an
/// `<unknown-param>` — falling back to "the only contact in the document"
/// (the overwhelmingly common case for these lines) when there is exactly
/// one and none mentions the IMEI at all.
///
/// Deliberately gives up (`None`) rather than guessing when there are
/// multiple contacts and none can be attributed to us: acting on a
/// *different* device's contact — Jio's own reginfo has shown a paired
/// handset's contact alongside ours — would force a re-registration this
/// line never needed.
pub(crate) fn find_own_contact(body: &str, imei: &str) -> Option<ReginfoContact> {
    let contacts = contact_blocks(body);
    if !imei.is_empty() {
        if let Some(block) = contacts.iter().find(|b| b.contains(imei)) {
            return parse_contact_attrs(block);
        }
    }
    match contacts.as_slice() {
        [only] => parse_contact_attrs(only),
        _ => None,
    }
}

/// RFC 3680 §4.1: `state="terminated"` is only a deregistration when paired
/// with one of these events — `expired` is our own scheduled renewal running
/// its course, not the network dropping us, so it is deliberately excluded.
fn contact_reports_deregistration(c: &ReginfoContact) -> bool {
    c.state.eq_ignore_ascii_case("terminated")
        && matches!(
            c.event.to_ascii_lowercase().as_str(),
            "deactivated" | "probation" | "rejected" | "unregistered"
        )
}

/// Acknowledges a `NOTIFY` and surfaces its payload. For `Event: reg` the
/// body is the network's reginfo XML — logged in full because it is the
/// ground truth for whether our binding is actually active for terminating
/// calls. The `To` header already carries our tag (it echoes our SUBSCRIBE's
/// `From` tag), so no tag is added.
///
/// Returns `true` when our own contact was reported deregistered — `state=
/// "terminated"` with `event` one of `deactivated`/`probation`/`rejected`/
/// `unregistered` (RFC 3680 §4.1) — so the caller can force an immediate
/// re-registration. Before this, any such NOTIFY was only logged: the line
/// would keep reporting itself `Registered` and silently receive nothing
/// until the next scheduled renewal, up to an hour later (specs/041
/// conformance review, MT-09).
///
/// This bridge always retries rather than giving up outright — the
/// same philosophy `GmConnectionState::Failed` already follows (not
/// terminal, keeps retrying on backoff) — so `rejected`/`unregistered` are
/// handled the same as `deactivated`/`probation`: force a fresh attempt now,
/// and let the existing renewal-failure backoff take over if the network
/// really does keep refusing it.
pub(crate) fn handle_notify(
    session: &super::RegisteredSession,
    sink: &SipSink,
    req: &SipRequest,
) -> bool {
    let _ = sink.send(&build_uas_response(200, "OK", req, None, None, None));
    let event = req.header("Event").unwrap_or("?").to_string();
    let sub_state = req.header("Subscription-State").unwrap_or("?").to_string();

    let own_contact = event
        .eq_ignore_ascii_case("reg")
        .then(|| find_own_contact(&req.body, &session.imei))
        .flatten();
    let deregistered = own_contact
        .as_ref()
        .is_some_and(contact_reports_deregistration);

    if let Some(c) = &own_contact {
        if deregistered {
            tracing::warn!(
                event = %event,
                subscription_state = %sub_state,
                contact_state = %c.state,
                contact_event = %c.event,
                "reg-event NOTIFY reports our own binding was deregistered; forcing an \
                 immediate re-registration"
            );
        } else {
            tracing::info!(
                event = %event,
                subscription_state = %sub_state,
                contact_state = %c.state,
                contact_event = %c.event,
                "received reg-event NOTIFY for our own binding"
            );
        }
    } else if req.body.contains("terminated") {
        // Either not a reg-event NOTIFY, an unparseable body, or a document
        // with more than one contact and none attributable to us — still
        // worth a warning, but nothing to act on with confidence.
        tracing::warn!(
            event = %event,
            subscription_state = %sub_state,
            body = %req.body,
            "NOTIFY body mentions a terminated state, but not attributably to our own contact"
        );
    } else {
        tracing::info!(event = %event, subscription_state = %sub_state, body = %req.body, "received NOTIFY");
    }

    deregistered
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

/// The user part of a header's URI, the same shape `extract_caller` has
/// always used for `From` — extracted here so `P-Asserted-Identity` can be
/// read with the exact same parsing (specs/045 MT-12).
fn header_user_part(req: &SipRequest, name: &str) -> Option<String> {
    req.header(name)
        .and_then(|f| f.split("sip:").nth(1))
        .and_then(|rest| rest.split(['@', ';', '>']).next())
        .map(str::to_string)
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
pub(crate) fn extract_caller(req: &SipRequest) -> String {
    header_user_part(req, "P-Asserted-Identity")
        .or_else(|| header_user_part(req, "From"))
        .unwrap_or_else(|| "unknown".to_string())
}

/// The **whole URI** named by a header, where [`extract_caller`] wants only
/// the user part — for addressing a new request back at whoever sent this
/// one. RFC 3261 §20 allows either form: a `name-addr`, where the URI is
/// inside `<...>` and any `;` after it separates *header* parameters, or a
/// bare `addr-spec`, where a `;` belongs to the URI itself. The brackets are
/// what tells the two apart, so they decide where the cut goes.
///
/// `None` for a header that is absent or names no URI at all.
pub(crate) fn header_uri(req: &SipRequest, name: &str) -> Option<String> {
    let value = req.header(name)?.trim();
    let uri = match value.split_once('<') {
        Some((_, rest)) => rest.split('>').next()?,
        // No brackets: parameters after the URI, if any, are the URI's own,
        // so only a `,` (the next header value) can end it.
        None => value.split(',').next()?,
    }
    .trim();
    // A display name with no URI at all ("Anonymous"), or an empty header,
    // is not something a request can be addressed to.
    uri.contains(':').then(|| uri.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let contact = find_own_contact(body, "860000000000000").expect("the sole contact");
        assert_eq!(contact.state, "active");
        assert_eq!(contact.event, "registered");
        assert!(!contact_reports_deregistration(&contact));
    }

    /// Multiple contacts, ours identified by its `+sip.instance` IMEI —
    /// modelled on the Jio reginfo (a paired handset's contact alongside
    /// ours) that motivated matching by IMEI instead of always taking "the"
    /// contact.
    #[test]
    fn our_contact_is_picked_out_by_imei_among_several() {
        let body = r#"<?xml version="1.0"?>
<reginfo xmlns="urn:ietf:params:xml:ns:reginfo" version="0" state="full">
  <registration aor="sip:+919000000000@ims.example" id="a1" state="active">
    <contact id="watch" state="active" event="registered" expires="3600">
      <uri>sip:+919000000000@192.168.1.50:5060</uri>
      <unknown-param name="+sip.instance">"&lt;urn:gsma:imei:490154203237518&gt;"</unknown-param>
    </contact>
    <contact id="phone" state="terminated" event="deactivated" expires="0">
      <uri>sip:+919000000000@10.0.0.1:5060;transport=tcp</uri>
      <unknown-param name="+sip.instance">"&lt;urn:gsma:imei:860000000000000&gt;"</unknown-param>
    </contact>
  </registration>
</reginfo>"#;
        let contact = find_own_contact(body, "860000000000000").expect("our contact, by IMEI");
        assert_eq!(contact.state, "terminated");
        assert_eq!(contact.event, "deactivated");
        assert!(contact_reports_deregistration(&contact));

        // The other device's own binding is untouched: a NOTIFY about it must
        // never be read as ours.
        let watch = find_own_contact(body, "490154203237518").expect("the watch's own contact");
        assert_eq!(watch.state, "active");
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
        assert!(find_own_contact(body, "860000000000000").is_none());
    }

    /// A self-closing `<contact/>` (no children at all) must still parse —
    /// RFC 3680's schema allows it, even though every real capture seen so
    /// far carries a `<uri>` child.
    #[test]
    fn a_self_closing_contact_element_is_parsed() {
        let body = r#"<reginfo><registration><contact id="c1" state="terminated" event="probation"/></registration></reginfo>"#;
        let contact = find_own_contact(body, "").expect("the sole, self-closing contact");
        assert_eq!(contact.state, "terminated");
        assert_eq!(contact.event, "probation");
        assert!(contact_reports_deregistration(&contact));
    }

    /// `expired` is our own scheduled renewal running its course, not the
    /// network dropping us — it must not trigger the same forced
    /// re-registration as a genuine deregistration.
    #[test]
    fn an_expired_event_is_not_treated_as_a_deregistration() {
        let contact = ReginfoContact {
            state: "terminated".to_string(),
            event: "expired".to_string(),
        };
        assert!(!contact_reports_deregistration(&contact));
    }

    /// `state="active"` is never a deregistration, whatever `event` says —
    /// the state is the gate, not just the event name.
    #[test]
    fn an_active_contact_is_never_a_deregistration_even_with_a_deregistration_style_event() {
        let contact = ReginfoContact {
            state: "active".to_string(),
            event: "deactivated".to_string(),
        };
        assert!(!contact_reports_deregistration(&contact));
    }

    /// Every event RFC 3680 §4.1 pairs with `state="terminated"` to mean the
    /// binding is genuinely gone, not just refreshed/shortened.
    #[test]
    fn every_deregistration_event_is_recognized() {
        for event in ["deactivated", "probation", "rejected", "unregistered"] {
            let contact = ReginfoContact {
                state: "terminated".to_string(),
                event: event.to_string(),
            };
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
        assert!(find_own_contact("<reginfo></reginfo>", "860000000000000").is_none());
        assert!(find_own_contact("", "860000000000000").is_none());
    }

    /// A `<contact` opening tag that never closes must not hang or panic —
    /// this is untrusted network input.
    #[test]
    fn a_truncated_contact_tag_does_not_panic() {
        assert!(find_own_contact("<reginfo><contact state=\"terminated\"", "").is_none());
    }
}
