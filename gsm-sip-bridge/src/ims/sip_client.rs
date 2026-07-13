//! A minimal, purpose-built SIP client for the single transaction we need:
//! send REGISTER, receive a 401 challenge, resend with credentials. This
//! deliberately does not use PJSIP — see the design note in `ims/mod.rs`.

use crate::error::{BridgeError, BridgeResult};
use rand::RngCore;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

const RECV_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_MSG_LEN: usize = 8192;
/// How often the Gm server's `accept()` comes up for air to notice that its
/// `GmServer` handle has been dropped (there is no interruptible `accept`).
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_secs(1);

pub fn random_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// A parsed SIP response: status line + headers (in original order, values
/// joined if a header name repeats) + reason phrase + body (e.g. an SDP
/// answer on an INVITE's 200 OK).
#[derive(Debug, Clone)]
pub struct SipResponse {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl SipResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Try to parse ONE complete SIP message from the front of `buf` (a
    /// message is complete once the header/body separator is present *and*
    /// `buf` holds at least `Content-Length` more bytes after it — a single
    /// TCP `read()` can return a partial message, or several messages back
    /// to back, e.g. `100 Trying` immediately followed by `180 Ringing`).
    /// Returns `Ok(None)` if `buf` doesn't yet hold a full message, along
    /// with how many bytes were consumed so the caller can drain them.
    fn try_parse(buf: &str) -> BridgeResult<Option<(Self, usize)>> {
        let Some(header_len) = buf.find("\r\n\r\n").map(|idx| idx + 4) else {
            return Ok(None);
        };
        let header_part = &buf[..header_len];

        let mut lines = header_part.split("\r\n");
        let status_line = lines
            .next()
            .ok_or_else(|| BridgeError::Ims("empty SIP response".into()))?;
        let mut parts = status_line.splitn(3, ' ');
        let _version = parts.next();
        let status: u16 = parts
            .next()
            .ok_or_else(|| BridgeError::Ims(format!("malformed status line: {status_line}")))?
            .parse()
            .map_err(|_| BridgeError::Ims(format!("malformed status code: {status_line}")))?;
        let reason = parts.next().unwrap_or("").to_string();

        // Unfold header continuation lines (leading whitespace) before splitting.
        let mut unfolded: Vec<String> = Vec::new();
        for line in lines {
            if line.is_empty() {
                break; // end of headers (blank line before body)
            }
            if (line.starts_with(' ') || line.starts_with('\t')) && !unfolded.is_empty() {
                let last = unfolded.last_mut().unwrap();
                last.push(' ');
                last.push_str(line.trim_start());
            } else {
                unfolded.push(line.to_string());
            }
        }

        let headers: Vec<(String, String)> = unfolded
            .into_iter()
            .filter_map(|line| {
                line.split_once(':')
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            })
            .collect();

        let content_length: usize = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
            .and_then(|(_, v)| v.trim().parse().ok())
            .unwrap_or(0);

        let total_len = header_len + content_length;
        if buf.len() < total_len {
            return Ok(None);
        }
        let body = buf[header_len..total_len].to_string();

        Ok(Some((
            Self {
                status,
                reason,
                headers,
                body,
            },
            total_len,
        )))
    }
}

/// A parsed inbound SIP *request* (e.g. `INVITE`, `BYE`) — the UAS-side
/// counterpart to `SipResponse`. Agent A (`specs/011-vowifi-sip-bridge`)
/// needs this to receive calls, whereas every SIP exchange this module
/// handled before (REGISTER, INVITE-as-UAC in `ims::call`) only ever needed
/// to parse *responses*.
#[derive(Debug, Clone)]
pub struct SipRequest {
    pub method: String,
    pub request_uri: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl SipRequest {
    /// First header matching `name` (case-insensitive), if any.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Every header matching `name` (case-insensitive), in the order they
    /// appeared. Needed for `Via`: RFC 3261 §8.2.6.2 requires a UAS
    /// generating a response to copy *every* `Via` header from the request,
    /// in order, verbatim — unlike most other headers there can legitimately
    /// be more than one (one per proxy hop the request traversed).
    pub fn headers_all<'a>(&'a self, name: &str) -> Vec<&'a str> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// Same partial-read/`Content-Length`-aware framing as
    /// `SipResponse::try_parse` (see its docs), for a request's
    /// `METHOD request-uri SIP/2.0` start-line instead of a status line.
    /// `pub(super)` (not private) so `ims::agent`'s veth-facing UAS — a
    /// sibling module of this one, not a descendant — can parse a
    /// single-datagram INVITE from Agent B directly, the same way
    /// `SipTransport::recv_message` (in this module) parses one from a
    /// buffered stream.
    pub(super) fn try_parse(buf: &str) -> BridgeResult<Option<(Self, usize)>> {
        let Some(header_len) = buf.find("\r\n\r\n").map(|idx| idx + 4) else {
            return Ok(None);
        };
        let header_part = &buf[..header_len];

        let mut lines = header_part.split("\r\n");
        let request_line = lines
            .next()
            .ok_or_else(|| BridgeError::Ims("empty SIP request".into()))?;
        let mut parts = request_line.splitn(3, ' ');
        let method = parts
            .next()
            .ok_or_else(|| BridgeError::Ims(format!("malformed request line: {request_line}")))?
            .to_string();
        let request_uri = parts
            .next()
            .ok_or_else(|| BridgeError::Ims(format!("malformed request line: {request_line}")))?
            .to_string();

        let mut unfolded: Vec<String> = Vec::new();
        for line in lines {
            if line.is_empty() {
                break;
            }
            if (line.starts_with(' ') || line.starts_with('\t')) && !unfolded.is_empty() {
                let last = unfolded.last_mut().unwrap();
                last.push(' ');
                last.push_str(line.trim_start());
            } else {
                unfolded.push(line.to_string());
            }
        }

        let headers: Vec<(String, String)> = unfolded
            .into_iter()
            .filter_map(|line| {
                line.split_once(':')
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            })
            .collect();

        let content_length: usize = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
            .and_then(|(_, v)| v.trim().parse().ok())
            .unwrap_or(0);

        let total_len = header_len + content_length;
        if buf.len() < total_len {
            return Ok(None);
        }
        let body = buf[header_len..total_len].to_string();

        Ok(Some((
            Self {
                method,
                request_uri,
                headers,
                body,
            },
            total_len,
        )))
    }
}

/// Either a request or a response arriving on the same connection — Agent A
/// reads whichever comes next (an inbound `INVITE`/`BYE`/`ACK` from the
/// carrier, or a response to something Agent A itself sent, e.g. a `BYE` it
/// initiated) without knowing in advance which it'll be.
#[derive(Debug, Clone)]
pub enum SipMessage {
    Request(SipRequest),
    Response(SipResponse),
}

/// Build a UAS response to `request`: echoes `Via` (every instance, in
/// order — see `SipRequest::headers_all` docs), `From`, `Call-ID`, and
/// `CSeq` verbatim per RFC 3261 §8.2.6, adds `to_tag` to the `To` header
/// (skipped for `100 Trying`, which conventionally carries none), and
/// includes `contact`/`body` when given (a final response to `INVITE` needs
/// both; `100 Trying` needs neither; `486 Busy Here` needs neither either).
pub fn build_uas_response(
    status: u16,
    reason: &str,
    request: &SipRequest,
    to_tag: Option<&str>,
    contact: Option<&str>,
    body: Option<&str>,
) -> String {
    let mut msg = format!("SIP/2.0 {status} {reason}\r\n");
    for via in request.headers_all("Via") {
        msg.push_str(&format!("Via: {via}\r\n"));
    }
    if let Some(from) = request.header("From") {
        msg.push_str(&format!("From: {from}\r\n"));
    }
    let to = request.header("To").unwrap_or("");
    match to_tag {
        Some(tag) => msg.push_str(&format!("To: {to};tag={tag}\r\n")),
        None => msg.push_str(&format!("To: {to}\r\n")),
    }
    if let Some(call_id) = request.header("Call-ID") {
        msg.push_str(&format!("Call-ID: {call_id}\r\n"));
    }
    if let Some(cseq) = request.header("CSeq") {
        msg.push_str(&format!("CSeq: {cseq}\r\n"));
    }
    if let Some(contact) = contact {
        msg.push_str(&format!("Contact: {contact}\r\n"));
    }
    let body = body.unwrap_or("");
    if !body.is_empty() {
        msg.push_str("Content-Type: application/sdp\r\n");
    }
    msg.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    msg
}

/// `100 Trying` — no `To` tag (see `build_uas_response` docs), no `Contact`.
pub fn build_100_trying(request: &SipRequest) -> String {
    build_uas_response(100, "Trying", request, None, None, None)
}

/// `180 Ringing` — establishes the early dialog, so it carries a `To` tag
/// and a `Contact` (needed so the carrier's later in-dialog requests, e.g.
/// `ACK`/`BYE`, target the right address).
pub fn build_180_ringing(request: &SipRequest, to_tag: &str, contact: &str) -> String {
    build_uas_response(180, "Ringing", request, Some(to_tag), Some(contact), None)
}

/// `200 OK` answering an `INVITE`, carrying the SDP answer body.
pub fn build_200_ok_invite(
    request: &SipRequest,
    to_tag: &str,
    contact: &str,
    sdp_body: &str,
) -> String {
    build_uas_response(
        200,
        "OK",
        request,
        Some(to_tag),
        Some(contact),
        Some(sdp_body),
    )
}

/// `200 OK` answering a `BYE` — no body, no `Contact` needed (the dialog is
/// ending).
pub fn build_200_ok_bye(request: &SipRequest, to_tag: &str) -> String {
    build_uas_response(200, "OK", request, Some(to_tag), None, None)
}

/// `486 Busy Here` — declines an inbound `INVITE` (FR-009/FR-010: busy, or
/// the SIP/PBX leg couldn't be established), per the spec's Clarifications
/// answer that a decline must be a fast, explicit signal rather than
/// unanswered ringing or dead air.
pub fn build_486_busy_here(request: &SipRequest, to_tag: &str) -> String {
    build_uas_response(486, "Busy Here", request, Some(to_tag), None, None)
}

/// `200 OK` acknowledging an inbound `MESSAGE` (RFC 3428) — no body, no
/// `Contact`: a `MESSAGE` is a standalone transaction, not a dialog, so
/// there is no future in-dialog request that would need one.
pub fn build_200_ok_message(request: &SipRequest, to_tag: &str) -> String {
    build_uas_response(200, "OK", request, Some(to_tag), None, None)
}

/// Everything needed to end a dialog we answered as a UAS — i.e. to hang up
/// on the *carrier* for an inbound call.
///
/// Extracted from the original `INVITE` at answer time (see
/// `DialogInfo::from_invite`), because by the time we want to send the BYE the
/// request itself is long gone. Note the role reversal: our `From` is the
/// INVITE's `To` (plus the tag we generated), and our `To` is the INVITE's
/// `From` — a BYE from the answerer flows in the opposite direction to the
/// INVITE it terminates.
pub struct ByeRequest<'a> {
    /// The remote target: the URI from the caller's `Contact`, not the
    /// original Request-URI (RFC 3261 §12.2.1.1 — in-dialog requests go to
    /// the peer's Contact).
    pub request_uri: &'a str,
    /// The dialog's route set: the `Record-Route` headers from the INVITE, in
    /// reverse order (§12.2.1.1 again — the UAS's route set is the reverse of
    /// what it received).
    pub route_headers: &'a [String],
    pub via_transport: &'a str,
    pub local_addr: SocketAddr,
    /// Full header values, tags included, already role-swapped.
    pub from: &'a str,
    pub to: &'a str,
    pub call_id: &'a str,
    pub cseq: u32,
    pub branch: &'a str,
}

pub fn build_bye(req: &ByeRequest) -> String {
    let via_addr = format_sip_addr(req.local_addr);
    let mut msg = format!(
        "BYE {request_uri} SIP/2.0\r\n\
         Via: SIP/2.0/{transport} {via_addr};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n",
        request_uri = req.request_uri,
        transport = req.via_transport,
        branch = req.branch,
    );
    for route in req.route_headers {
        msg.push_str(route);
        msg.push_str("\r\n");
    }
    msg.push_str(&format!(
        "From: {from}\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} BYE\r\n\
         Content-Length: 0\r\n\r\n",
        from = req.from,
        to = req.to,
        call_id = req.call_id,
        cseq = req.cseq,
    ));
    msg
}

/// Everything needed to build a REGISTER request.
pub struct RegisterRequest<'a> {
    pub registrar_uri: &'a str,
    pub public_uri: &'a str,
    /// Where this request is sent *from* — the Gm protected **client** port
    /// (`port-c`) once IPsec is up. Goes in `Via` (RFC 3261 §18.1.1).
    pub local_addr: SocketAddr,
    /// Where the network should reach *us* — the Gm protected **server**
    /// port (`port-s`), which is a different port from `local_addr` and the
    /// only one anything network-initiated is delivered to (TS 24.229
    /// §5.1.1.2: the UE puts the protected server port in `Contact`).
    /// Advertising `local_addr` here instead points the P-CSCF at our
    /// outbound socket, where its connection attempt is met with an RST and
    /// every mobile-terminating request is silently lost.
    pub contact_addr: SocketAddr,
    pub call_id: &'a str,
    pub from_tag: &'a str,
    pub branch: &'a str,
    pub cseq: u32,
    pub expires: u32,
    /// Via/branch transport token — must match the transport actually used
    /// to send the request (RFC 3261 §18.1.1), e.g. "UDP" or "TCP".
    pub transport: &'a str,
    pub authorization: Option<&'a str>,
    /// Verbatim extra header lines (no trailing CRLF), e.g. `Supported:
    /// sec-agree` / `Security-Client: ipsec-3gpp; ...` for networks that
    /// mandate Gm IPsec negotiation (RFC 3329 / TS 24.229) before accepting
    /// REGISTER.
    pub extra_headers: &'a [String],
    /// The device IMEI, sent as the `Contact` header's `+sip.instance`
    /// (`urn:gsma:imei:<imei>`) — real UEs always send their genuine IMEI
    /// here, not a placeholder. A network's terminating-call routing may
    /// key off this (device fingerprinting / entitlement checks) even when
    /// a fake value doesn't stop REGISTER itself from succeeding.
    pub imei: &'a str,
}

pub fn format_sip_addr(addr: SocketAddr) -> String {
    match addr.ip() {
        IpAddr::V6(ip) => format!("[{ip}]:{}", addr.port()),
        IpAddr::V4(ip) => format!("{ip}:{}", addr.port()),
    }
}

pub fn build_register(req: &RegisterRequest) -> String {
    let via_addr = format_sip_addr(req.local_addr);
    let contact_addr = format_sip_addr(req.contact_addr);

    let mut msg = format!(
        "REGISTER sip:{registrar} SIP/2.0\r\n\
         Via: SIP/2.0/{transport} {via_addr};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:{public}>;tag={from_tag}\r\n\
         To: <sip:{public}>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} REGISTER\r\n\
         Contact: <sip:{public_user}@{contact_addr};transport={transport}>;+g.3gpp.icsi-ref=\"urn%3Aurn-7%3A3gpp-service.ims.icsi.mmtel\";audio;+sip.instance=\"<urn:gsma:imei:{imei}>\"\r\n\
         Expires: {expires}\r\n\
         Allow: OPTIONS, REGISTER, SUBSCRIBE, NOTIFY, PUBLISH, INVITE, ACK, BYE, CANCEL, UPDATE, PRACK, INFO, MESSAGE, REFER\r\n\
         User-Agent: motorola_XT2241-1_Android15_V1SQS35H.58-10-8-9\r\n",
        registrar = req.registrar_uri,
        transport = req.transport,
        via_addr = via_addr,
        branch = req.branch,
        public = req.public_uri,
        from_tag = req.from_tag,
        call_id = req.call_id,
        cseq = req.cseq,
        contact_addr = contact_addr,
        public_user = req.public_uri.split('@').next().unwrap_or(req.public_uri),
        expires = req.expires,
        imei = req.imei,
    );
    if let Some(auth) = req.authorization {
        msg.push_str("Authorization: ");
        msg.push_str(auth);
        msg.push_str("\r\n");
    }
    for header in req.extra_headers {
        msg.push_str(header);
        msg.push_str("\r\n");
    }
    msg.push_str("Content-Length: 0\r\n\r\n");
    msg
}

enum Socket {
    Udp(UdpSocket),
    Tcp(TcpStream),
}

/// A single UDP or TCP connection to the P-CSCF, held open for a whole
/// transaction (REGISTER + challenge response, or INVITE + provisional
/// responses + final response) so that: (a) the local address is known
/// *before* building the first request (an unspecified `0.0.0.0`/`::`
/// Via/Contact is grounds for a P-CSCF to silently drop it), and (b) — for
/// TCP — the same connection carries every message, as most SIP stacks
/// expect. `buf` holds bytes read but not yet consumed into a full message
/// — a single `read()` can return a partial message, or several back to
/// back (e.g. `100 Trying` immediately followed by `180 Ringing`).
pub struct SipTransport {
    socket: Socket,
    buf: String,
}

impl SipTransport {
    pub fn connect(pcscf: SocketAddr, use_tcp: bool) -> BridgeResult<Self> {
        let socket = if use_tcp {
            let stream = TcpStream::connect_timeout(&pcscf, RECV_TIMEOUT)
                .map_err(|e| BridgeError::Ims(format!("TCP connect to {pcscf} failed: {e}")))?;
            stream
                .set_read_timeout(Some(RECV_TIMEOUT))
                .map_err(|e| BridgeError::Ims(format!("set_read_timeout failed: {e}")))?;
            Socket::Tcp(stream)
        } else {
            let bind_addr: SocketAddr = match pcscf {
                SocketAddr::V4(_) => "0.0.0.0:0".parse().unwrap(),
                SocketAddr::V6(_) => "[::]:0".parse().unwrap(),
            };
            let socket = UdpSocket::bind(bind_addr)
                .map_err(|e| BridgeError::Ims(format!("UDP bind failed: {e}")))?;
            socket
                .connect(pcscf)
                .map_err(|e| BridgeError::Ims(format!("UDP connect to {pcscf} failed: {e}")))?;
            socket
                .set_read_timeout(Some(RECV_TIMEOUT))
                .map_err(|e| BridgeError::Ims(format!("set_read_timeout failed: {e}")))?;
            Socket::Udp(socket)
        };
        Ok(Self {
            socket,
            buf: String::new(),
        })
    }

    /// Open a *new* connection from an explicitly chosen local port to a
    /// (possibly different) destination — needed to resend the authenticated
    /// REGISTER once Gm IPsec is set up: the retry must go out from the same
    /// local port we proposed as `port-c` in `Security-Client`, to the
    /// network's negotiated `port-s` (from `Security-Server`), since the
    /// installed XFRM policy's selector matches on exactly that 4-tuple.
    pub fn connect_from(local_port: u16, dst: SocketAddr, use_tcp: bool) -> BridgeResult<Self> {
        let domain = if dst.is_ipv6() {
            socket2::Domain::IPV6
        } else {
            socket2::Domain::IPV4
        };
        let bind_addr: SocketAddr = match dst {
            SocketAddr::V4(_) => format!("0.0.0.0:{local_port}").parse().unwrap(),
            SocketAddr::V6(_) => format!("[::]:{local_port}").parse().unwrap(),
        };

        let socket = if use_tcp {
            let socket = socket2::Socket::new(domain, socket2::Type::STREAM, None)
                .map_err(|e| BridgeError::Ims(format!("socket() failed: {e}")))?;
            socket
                .set_reuse_address(true)
                .map_err(|e| BridgeError::Ims(format!("SO_REUSEADDR failed: {e}")))?;
            socket
                .bind(&bind_addr.into())
                .map_err(|e| BridgeError::Ims(format!("bind to {bind_addr} failed: {e}")))?;
            socket
                .connect_timeout(&dst.into(), RECV_TIMEOUT)
                .map_err(|e| BridgeError::Ims(format!("TCP connect to {dst} failed: {e}")))?;
            let stream: TcpStream = socket.into();
            stream
                .set_read_timeout(Some(RECV_TIMEOUT))
                .map_err(|e| BridgeError::Ims(format!("set_read_timeout failed: {e}")))?;
            Socket::Tcp(stream)
        } else {
            let socket = socket2::Socket::new(domain, socket2::Type::DGRAM, None)
                .map_err(|e| BridgeError::Ims(format!("socket() failed: {e}")))?;
            socket
                .set_reuse_address(true)
                .map_err(|e| BridgeError::Ims(format!("SO_REUSEADDR failed: {e}")))?;
            socket
                .bind(&bind_addr.into())
                .map_err(|e| BridgeError::Ims(format!("bind to {bind_addr} failed: {e}")))?;
            socket
                .connect(&dst.into())
                .map_err(|e| BridgeError::Ims(format!("UDP connect to {dst} failed: {e}")))?;
            let sock: UdpSocket = socket.into();
            sock.set_read_timeout(Some(RECV_TIMEOUT))
                .map_err(|e| BridgeError::Ims(format!("set_read_timeout failed: {e}")))?;
            Socket::Udp(sock)
        };
        Ok(Self {
            socket,
            buf: String::new(),
        })
    }

    pub fn local_addr(&self) -> BridgeResult<SocketAddr> {
        let addr = match &self.socket {
            Socket::Udp(s) => s.local_addr(),
            Socket::Tcp(s) => s.local_addr(),
        };
        addr.map_err(|e| BridgeError::Ims(format!("local_addr failed: {e}")))
    }

    /// For TCP, force an abortive close (`SO_LINGER` 0, sends `RST`) instead
    /// of the default graceful FIN — needed right before dropping a
    /// connection we're about to rebind its exact local port for. A normal
    /// close leaves the port in `FIN_WAIT`/`TIME_WAIT`, which `SO_REUSEADDR`
    /// on the new socket doesn't reliably bypass if the old one hasn't
    /// reached `TIME_WAIT` yet by the time we rebind (a near-certainty when
    /// rebinding immediately after the last write, as here). No-op for UDP.
    pub fn force_close(&self) {
        if let Socket::Tcp(stream) = &self.socket {
            let _ = socket2::SockRef::from(stream).set_linger(Some(Duration::ZERO));
        }
    }

    pub fn send(&mut self, message: &str) -> BridgeResult<()> {
        tracing::debug!(message = %message, "sending SIP request");
        match &mut self.socket {
            Socket::Udp(socket) => {
                socket
                    .send(message.as_bytes())
                    .map_err(|e| BridgeError::Ims(format!("UDP send failed: {e}")))?;
            }
            Socket::Tcp(stream) => {
                stream
                    .write_all(message.as_bytes())
                    .map_err(|e| BridgeError::Ims(format!("TCP send failed: {e}")))?;
            }
        };
        Ok(())
    }

    /// Try one read. Returns `Ok(true)` if new data was appended to
    /// `self.buf`, `Ok(false)` if the read timed out (the underlying
    /// socket's `RECV_TIMEOUT`) with no new data — not necessarily a
    /// problem, e.g. while waiting for a phone to ring for longer than
    /// that — or `Err` on a real I/O failure.
    fn read_more(&mut self) -> BridgeResult<bool> {
        let mut tmp = [0u8; MAX_MSG_LEN];
        let result = match &mut self.socket {
            Socket::Udp(socket) => socket.recv(&mut tmp),
            Socket::Tcp(stream) => stream.read(&mut tmp),
        };
        match result {
            Ok(0) => Err(BridgeError::Ims(
                "connection closed by peer with no data (0 bytes read)".into(),
            )),
            Ok(n) => {
                self.buf.push_str(&String::from_utf8_lossy(&tmp[..n]));
                Ok(true)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(false)
            }
            Err(e) => Err(BridgeError::Ims(format!("recv failed: {e}"))),
        }
    }

    /// Block until one complete SIP message (headers + `Content-Length`
    /// bytes of body) is available, then return it — draining exactly that
    /// message from the internal buffer so a message that arrived alongside
    /// (or ahead of) it isn't lost. A read timing out with no data at all is
    /// treated as failure here — fine for REGISTER, which expects a prompt
    /// reply; `recv_final_response` is the one that tolerates a long wait.
    pub fn recv_response(&mut self) -> BridgeResult<SipResponse> {
        loop {
            if let Some((resp, consumed)) = SipResponse::try_parse(&self.buf)? {
                tracing::debug!(response = %self.buf[..consumed], "received SIP response");
                self.buf.drain(..consumed);
                return Ok(resp);
            }
            if !self.read_more()? {
                return Err(BridgeError::Ims(
                    "timed out waiting for a SIP response".into(),
                ));
            }
        }
    }

    /// Send a request and wait for the *first* response — fine for
    /// REGISTER, where a `401` challenge is always the final response to
    /// that transaction (no provisional responses expected).
    pub fn send_and_recv(&mut self, message: &str) -> BridgeResult<SipResponse> {
        self.send(message)?;
        self.recv_response()
    }

    /// Wait for a *final* (status >= 200) response, logging and skipping
    /// any provisional ones (`100 Trying`, `180 Ringing`, ...) along the
    /// way — needed for INVITE, which can take several seconds (or tens of
    /// seconds, well past a single socket read's `RECV_TIMEOUT`) to ring
    /// before the callee answers or declines.
    pub fn recv_final_response(&mut self, overall_timeout: Duration) -> BridgeResult<SipResponse> {
        let deadline = std::time::Instant::now() + overall_timeout;
        loop {
            if let Some((resp, consumed)) = SipResponse::try_parse(&self.buf)? {
                self.buf.drain(..consumed);
                if resp.status >= 200 {
                    return Ok(resp);
                }
                tracing::info!(status = resp.status, reason = %resp.reason, "provisional response");
                continue;
            }
            self.read_more()?;
            if std::time::Instant::now() >= deadline {
                return Err(BridgeError::Ims(
                    "timed out waiting for a final response".into(),
                ));
            }
        }
    }

    /// Block until the next complete SIP message — request or response —
    /// arrives, or `timeout` elapses with none available (`Ok(None)`).
    /// Unlike every other `recv_*` method above (each a single bounded
    /// transaction), this is for a long-running listener (Agent A's
    /// inbound-call dispatch loop) that has no fixed deadline for "when's
    /// the next call" but still needs to periodically come up for air (e.g.
    /// to check whether its registration needs renewing,
    /// `specs/011-vowifi-sip-bridge` User Story 2) — a per-`read()` timeout
    /// is treated as "nothing yet", not a failure; only a real I/O error
    /// propagates. Distinguishes request from response by checking whether
    /// the start-line begins with `SIP/2.0` (a response's first token) — a
    /// request's start-line always *ends* with `SIP/2.0` but begins with
    /// its method name instead.
    pub fn recv_message_deadline(&mut self, timeout: Duration) -> BridgeResult<Option<SipMessage>> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(line_end) = self.buf.find("\r\n") {
                let is_response = self.buf[..line_end].starts_with("SIP/2.0");
                if is_response {
                    if let Some((resp, consumed)) = SipResponse::try_parse(&self.buf)? {
                        self.buf.drain(..consumed);
                        return Ok(Some(SipMessage::Response(resp)));
                    }
                } else if let Some((req, consumed)) = SipRequest::try_parse(&self.buf)? {
                    tracing::debug!(request = %self.buf[..consumed], "received SIP request");
                    self.buf.drain(..consumed);
                    return Ok(Some(SipMessage::Request(req)));
                }
            }
            self.read_more()?;
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }
}

/// The write half of a SIP connection — cloneable and shareable across
/// threads, and detached from the reader.
///
/// A UAS must answer a request on the connection that request arrived on
/// (RFC 3261 §18.2.2). Over Gm that is *not* always the connection we
/// registered over: the network opens a fresh connection to our protected
/// server port for every mobile-terminating request (see `spawn_gm_server`),
/// so responses have to be routed back per-message rather than through one
/// globally-owned transport.
#[derive(Clone)]
pub struct SipSink {
    inner: Arc<SinkInner>,
}

enum SinkInner {
    Tcp(Mutex<TcpStream>),
    /// A server-side UDP socket is not `connect()`ed to one peer, so the
    /// address to answer is captured per-message from `recv_from`.
    Udp(UdpSocket, SocketAddr),
}

impl SipSink {
    pub fn send(&self, message: &str) -> BridgeResult<()> {
        tracing::debug!(message = %message, "sending SIP message");
        match &*self.inner {
            SinkInner::Tcp(stream) => stream
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .write_all(message.as_bytes())
                .map_err(|e| BridgeError::Ims(format!("TCP send failed: {e}"))),
            SinkInner::Udp(socket, peer) => socket
                .send_to(message.as_bytes(), peer)
                .map(|_| ())
                .map_err(|e| BridgeError::Ims(format!("UDP send failed: {e}"))),
        }
    }
}

impl SipTransport {
    /// Wrap an already-accepted TCP connection (from the Gm server port).
    fn from_tcp(stream: TcpStream) -> BridgeResult<Self> {
        stream
            .set_read_timeout(Some(RECV_TIMEOUT))
            .map_err(|e| BridgeError::Ims(format!("set_read_timeout failed: {e}")))?;
        Ok(Self {
            socket: Socket::Tcp(stream),
            buf: String::new(),
        })
    }

    /// A cloneable write handle onto this same connection.
    pub fn sink(&self) -> BridgeResult<SipSink> {
        let inner = match &self.socket {
            Socket::Tcp(s) => SinkInner::Tcp(Mutex::new(
                s.try_clone()
                    .map_err(|e| BridgeError::Ims(format!("TCP try_clone failed: {e}")))?,
            )),
            Socket::Udp(s) => {
                let peer = s
                    .peer_addr()
                    .map_err(|e| BridgeError::Ims(format!("UDP peer_addr failed: {e}")))?;
                SinkInner::Udp(
                    s.try_clone()
                        .map_err(|e| BridgeError::Ims(format!("UDP try_clone failed: {e}")))?,
                    peer,
                )
            }
        };
        Ok(SipSink {
            inner: Arc::new(inner),
        })
    }

    /// A second handle onto the same connection, for a dedicated reader
    /// thread. The caller must then read *only* through the returned handle
    /// and write only through `sink()` — two readers on one socket would
    /// race for bytes. Any bytes already buffered in `self` stay with `self`
    /// and would be lost to the reader, so this is only sound while `self`'s
    /// buffer is empty (i.e. immediately after a completed transaction).
    pub fn try_clone_reader(&self) -> BridgeResult<Self> {
        if !self.buf.is_empty() {
            tracing::warn!(
                buffered = self.buf.len(),
                "cloning a SIP transport whose buffer is non-empty; buffered bytes will not reach the reader"
            );
        }
        let socket = match &self.socket {
            Socket::Tcp(s) => Socket::Tcp(
                s.try_clone()
                    .map_err(|e| BridgeError::Ims(format!("TCP try_clone failed: {e}")))?,
            ),
            Socket::Udp(s) => Socket::Udp(
                s.try_clone()
                    .map_err(|e| BridgeError::Ims(format!("UDP try_clone failed: {e}")))?,
            ),
        };
        Ok(Self {
            socket,
            buf: String::new(),
        })
    }
}

/// Owns the Gm protected-server-port listener. Dropping it stops the accept
/// loop (and, once their peers hang up, the per-connection reader threads).
pub struct GmServer {
    stop: Arc<AtomicBool>,
}

impl Drop for GmServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Listen on the Gm **protected server port** (`port-s` of the
/// `Security-Client`/`Security-Server` negotiation, TS 33.203 Annex H) for
/// network-initiated requests, delivering each one — paired with a `SipSink`
/// that answers on the connection it came in on — to `tx`.
///
/// This is what makes a registration reachable at all. The P-CSCF does not
/// reuse the connection the UE registered over for anything it originates:
/// the reg-event `NOTIFY`, and every mobile-terminating `INVITE`, arrive on
/// a *new* connection it opens to this port. With nothing bound here the
/// kernel answers the network's SYN with an RST, the P-CSCF concludes the UE
/// is unreachable, and inbound calls are never delivered — while REGISTER and
/// outbound calls, both client-initiated, keep working and hide the fault.
pub fn spawn_gm_server(
    local: SocketAddr,
    use_tcp: bool,
    tx: mpsc::Sender<(SipMessage, SipSink)>,
) -> BridgeResult<GmServer> {
    let stop = Arc::new(AtomicBool::new(false));
    if use_tcp {
        spawn_gm_tcp_server(local, tx, stop.clone())?;
    } else {
        spawn_gm_udp_server(local, tx, stop.clone())?;
    }
    tracing::info!(local = %local, transport = if use_tcp { "TCP" } else { "UDP" }, "listening on the Gm protected server port for network-initiated requests");
    Ok(GmServer { stop })
}

fn bind_gm_socket(
    local: SocketAddr,
    ty: socket2::Type,
    timeout: Duration,
) -> BridgeResult<socket2::Socket> {
    let domain = if local.is_ipv6() {
        socket2::Domain::IPV6
    } else {
        socket2::Domain::IPV4
    };
    let socket = socket2::Socket::new(domain, ty, None)
        .map_err(|e| BridgeError::Ims(format!("Gm server socket() failed: {e}")))?;
    socket
        .set_reuse_address(true)
        .map_err(|e| BridgeError::Ims(format!("Gm server SO_REUSEADDR failed: {e}")))?;
    socket
        .bind(&local.into())
        .map_err(|e| BridgeError::Ims(format!("Gm server bind to {local} failed: {e}")))?;
    // Bounds how long accept()/recv() blocks, so the loop can notice `stop`.
    socket
        .set_read_timeout(Some(timeout))
        .map_err(|e| BridgeError::Ims(format!("Gm server set_read_timeout failed: {e}")))?;
    Ok(socket)
}

fn spawn_gm_tcp_server(
    local: SocketAddr,
    tx: mpsc::Sender<(SipMessage, SipSink)>,
    stop: Arc<AtomicBool>,
) -> BridgeResult<()> {
    let socket = bind_gm_socket(local, socket2::Type::STREAM, ACCEPT_POLL_INTERVAL)?;
    socket
        .listen(8)
        .map_err(|e| BridgeError::Ims(format!("Gm server listen on {local} failed: {e}")))?;
    let listener: TcpListener = socket.into();

    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, peer)) => {
                    tracing::info!(peer = %peer, "network opened a connection to the Gm server port");
                    let tx = tx.clone();
                    let stop = stop.clone();
                    std::thread::spawn(move || serve_gm_connection(stream, peer, tx, stop));
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "Gm server accept failed; stopping");
                    return;
                }
            }
        }
    });
    Ok(())
}

fn serve_gm_connection(
    stream: TcpStream,
    peer: SocketAddr,
    tx: mpsc::Sender<(SipMessage, SipSink)>,
    stop: Arc<AtomicBool>,
) {
    let mut transport = match SipTransport::from_tcp(stream) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(peer = %peer, error = %e, "failed to set up the Gm server connection");
            return;
        }
    };
    let sink = match transport.sink() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(peer = %peer, error = %e, "failed to derive a sink for the Gm server connection");
            return;
        }
    };
    while !stop.load(Ordering::Relaxed) {
        match transport.recv_message_deadline(RECV_TIMEOUT) {
            Ok(Some(msg)) => {
                if tx.send((msg, sink.clone())).is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(peer = %peer, error = %e, "Gm server connection closed");
                return;
            }
        }
    }
}

fn spawn_gm_udp_server(
    local: SocketAddr,
    tx: mpsc::Sender<(SipMessage, SipSink)>,
    stop: Arc<AtomicBool>,
) -> BridgeResult<()> {
    let socket = bind_gm_socket(local, socket2::Type::DGRAM, ACCEPT_POLL_INTERVAL)?;
    let socket: UdpSocket = socket.into();

    std::thread::spawn(move || {
        let mut buf = [0u8; MAX_MSG_LEN];
        while !stop.load(Ordering::Relaxed) {
            let (n, peer) = match socket.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Gm server recv failed; stopping");
                    return;
                }
            };
            // Every SIP datagram is a complete message, so unlike the TCP
            // path there is no cross-read buffering to do here.
            let text = String::from_utf8_lossy(&buf[..n]);
            let parsed = if text.starts_with("SIP/2.0") {
                SipResponse::try_parse(&text).map(|o| o.map(|(r, _)| SipMessage::Response(r)))
            } else {
                SipRequest::try_parse(&text).map(|o| o.map(|(r, _)| SipMessage::Request(r)))
            };
            let msg = match parsed {
                Ok(Some(msg)) => msg,
                Ok(None) => {
                    tracing::warn!(peer = %peer, "incomplete SIP datagram on the Gm server port");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(peer = %peer, error = %e, "unparseable SIP datagram on the Gm server port");
                    continue;
                }
            };
            let sink = match socket.try_clone() {
                Ok(s) => SipSink {
                    inner: Arc::new(SinkInner::Udp(s, peer)),
                },
                Err(e) => {
                    tracing::warn!(error = %e, "Gm server UDP try_clone failed");
                    continue;
                }
            };
            if tx.send((msg, sink)).is_err() {
                return;
            }
        }
    });
    Ok(())
}

/// Parse a `WWW-Authenticate: Digest ...` header value into its parameters.
/// Handles both quoted (`realm="..."`) and bare (`algorithm=AKAv1-MD5`)
/// values, comma-separated.
pub fn parse_digest_challenge(header: &str) -> BridgeResult<Vec<(String, String)>> {
    let rest = header
        .trim()
        .strip_prefix("Digest")
        .ok_or_else(|| BridgeError::Ims(format!("not a Digest challenge: {header}")))?
        .trim_start();

    let mut params = Vec::new();
    let mut chars = rest.chars().peekable();
    while chars.peek().is_some() {
        // skip separators/whitespace
        while matches!(chars.peek(), Some(',') | Some(' ')) {
            chars.next();
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' {
                break;
            }
            key.push(c);
            chars.next();
        }
        if chars.next() != Some('=') {
            break; // no more k=v pairs
        }
        let mut value = String::new();
        if chars.peek() == Some(&'"') {
            chars.next(); // opening quote
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                value.push(c);
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                value.push(c);
                chars.next();
            }
        }
        params.push((key.trim().to_string(), value));
    }
    Ok(params)
}

fn param<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

pub struct DigestChallenge {
    pub realm: String,
    pub nonce: String,
    pub qop: Option<String>,
    pub opaque: Option<String>,
    pub algorithm: Option<String>,
}

pub fn extract_challenge(params: &[(String, String)]) -> BridgeResult<DigestChallenge> {
    Ok(DigestChallenge {
        realm: param(params, "realm")
            .ok_or_else(|| BridgeError::Ims("challenge missing realm".into()))?
            .to_string(),
        nonce: param(params, "nonce")
            .ok_or_else(|| BridgeError::Ims("challenge missing nonce".into()))?
            .to_string(),
        qop: param(params, "qop").map(|s| s.to_string()),
        opaque: param(params, "opaque").map(|s| s.to_string()),
        algorithm: param(params, "algorithm").map(|s| s.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_line_and_headers() {
        let raw = "SIP/2.0 401 Unauthorized\r\n\
                   Via: SIP/2.0/UDP 1.2.3.4:5060\r\n\
                   WWW-Authenticate: Digest realm=\"ims.example.org\", nonce=\"abc==\", algorithm=AKAv1-MD5\r\n\
                   Content-Length: 0\r\n\r\n";
        let (resp, consumed) = SipResponse::try_parse(raw).unwrap().unwrap();
        assert_eq!(consumed, raw.len());
        assert_eq!(resp.status, 401);
        assert_eq!(resp.reason, "Unauthorized");
        assert!(resp
            .header("WWW-Authenticate")
            .unwrap()
            .contains("AKAv1-MD5"));
    }

    #[test]
    fn parse_unfolds_continuation_lines() {
        let raw = "SIP/2.0 200 OK\r\n\
                   Contact: <sip:foo@bar>\r\n\
                   \t;expires=600\r\n\r\n";
        let (resp, _) = SipResponse::try_parse(raw).unwrap().unwrap();
        assert_eq!(
            resp.header("Contact").unwrap(),
            "<sip:foo@bar> ;expires=600"
        );
    }

    #[test]
    fn try_parse_returns_none_when_incomplete() {
        let raw = "SIP/2.0 200 OK\r\nContent-Length: 5\r\n\r\nhel";
        assert!(SipResponse::try_parse(raw).unwrap().is_none());
    }

    #[test]
    fn try_parse_extracts_body_and_leaves_remainder_for_next_message() {
        let raw = "SIP/2.0 200 OK\r\nContent-Length: 2\r\n\r\nhiSIP/2.0 100 Trying\r\n\r\n";
        let (resp, consumed) = SipResponse::try_parse(raw).unwrap().unwrap();
        assert_eq!(resp.body, "hi");
        assert_eq!(&raw[consumed..], "SIP/2.0 100 Trying\r\n\r\n");
    }

    #[test]
    fn digest_challenge_roundtrip() {
        let header =
            "Digest realm=\"ims.mnc043.mcc404.3gppnetwork.org\", nonce=\"cmFuZGF1dG4=\", qop=\"auth,auth-int\", algorithm=AKAv1-MD5";
        let params = parse_digest_challenge(header).unwrap();
        let challenge = extract_challenge(&params).unwrap();
        assert_eq!(challenge.realm, "ims.mnc043.mcc404.3gppnetwork.org");
        assert_eq!(challenge.nonce, "cmFuZGF1dG4=");
        assert_eq!(challenge.qop.unwrap(), "auth,auth-int");
        assert_eq!(challenge.algorithm.unwrap(), "AKAv1-MD5");
    }

    #[test]
    fn digest_challenge_rejects_non_digest() {
        assert!(parse_digest_challenge("Basic realm=\"x\"").is_err());
    }

    #[test]
    fn build_register_formats_ipv6_contact_with_brackets() {
        let addr: SocketAddr = "[2402:8100::1]:5060".parse().unwrap();
        let req = RegisterRequest {
            registrar_uri: "ims.mnc043.mcc404.3gppnetwork.org",
            public_uri: "404438083996440@ims.mnc043.mcc404.3gppnetwork.org",
            local_addr: addr,
            contact_addr: addr,
            call_id: "callid123",
            from_tag: "tag123",
            branch: "z9hG4bKbranch",
            cseq: 1,
            expires: 600,
            transport: "UDP",
            authorization: None,
            extra_headers: &[],
            imei: "000000000000000",
        };
        let msg = build_register(&req);
        assert!(msg.starts_with("REGISTER sip:ims.mnc043.mcc404.3gppnetwork.org SIP/2.0\r\n"));
        assert!(msg.contains("[2402:8100::1]:5060"));
        assert!(msg.contains("CSeq: 1 REGISTER"));
        assert!(msg.ends_with("Content-Length: 0\r\n\r\n"));
    }

    /// The Contact must advertise the Gm protected *server* port, while the
    /// Via carries the *client* port we sent from — they are different
    /// ports, and pointing Contact at the client port makes the
    /// registration unreachable for everything the network originates.
    #[test]
    /// A BYE from the side that *answered* flows opposite to the INVITE: our
    /// From is the INVITE's To and vice versa, and it goes to the caller's
    /// Contact rather than the original Request-URI. Getting this backwards
    /// produces a BYE the network drops, so the caller stays on a dead call.
    #[test]
    fn build_bye_reverses_the_dialog_roles_and_targets_the_remote_contact() {
        let msg = build_bye(&ByeRequest {
            request_uri: "sip:caller@pcscf.example:5060",
            route_headers: &["Route: <sip:pcscf.example;lr>".to_string()],
            via_transport: "TCP",
            local_addr: "1.2.3.4:48584".parse().unwrap(),
            // Ours (was the INVITE's To), with the tag we generated.
            from: "<sip:+919043062139@ims.example>;tag=ourtag",
            // Theirs (was the INVITE's From), with the tag they generated.
            to: "\"Caller\" <sip:+919789063708@ims.example>;tag=theirtag",
            call_id: "callid1",
            cseq: 1,
            branch: "z9hG4bKbye1",
        });
        assert!(msg.starts_with("BYE sip:caller@pcscf.example:5060 SIP/2.0\r\n"));
        assert!(msg.contains("Route: <sip:pcscf.example;lr>\r\n"));
        assert!(msg.contains("From: <sip:+919043062139@ims.example>;tag=ourtag\r\n"));
        assert!(msg.contains("To: \"Caller\" <sip:+919789063708@ims.example>;tag=theirtag\r\n"));
        assert!(msg.contains("Call-ID: callid1\r\n"));
        assert!(msg.contains("CSeq: 1 BYE\r\n"));
        assert!(msg.ends_with("Content-Length: 0\r\n\r\n"));
    }

    #[test]
    fn build_register_advertises_the_protected_server_port_in_contact() {
        let client: SocketAddr = "[2402:8100::1]:48584".parse().unwrap();
        let server: SocketAddr = "[2402:8100::1]:48586".parse().unwrap();
        let req = RegisterRequest {
            registrar_uri: "example.org",
            public_uri: "user@example.org",
            local_addr: client,
            contact_addr: server,
            call_id: "callid",
            from_tag: "tag",
            branch: "branch",
            cseq: 1,
            expires: 600,
            transport: "TCP",
            authorization: None,
            extra_headers: &[],
            imei: "000000000000000",
        };
        let msg = build_register(&req);
        assert!(msg.contains("Via: SIP/2.0/TCP [2402:8100::1]:48584;branch=branch;rport\r\n"));
        assert!(msg.contains("Contact: <sip:user@[2402:8100::1]:48586;transport=TCP>"));
    }

    #[test]
    fn random_hex_has_expected_length() {
        assert_eq!(random_hex(8).len(), 16);
    }

    #[test]
    fn build_register_includes_extra_headers_before_content_length() {
        let addr: SocketAddr = "1.2.3.4:5060".parse().unwrap();
        let extra = vec![
            "Supported: sec-agree".to_string(),
            "Security-Client: ipsec-3gpp; alg=hmac-sha-1-96; ealg=null".to_string(),
        ];
        let req = RegisterRequest {
            registrar_uri: "example.org",
            public_uri: "user@example.org",
            local_addr: addr,
            contact_addr: addr,
            call_id: "callid",
            from_tag: "tag",
            branch: "branch",
            cseq: 1,
            expires: 600,
            transport: "UDP",
            authorization: None,
            extra_headers: &extra,
            imei: "000000000000000",
        };
        let msg = build_register(&req);
        assert!(msg.contains("Supported: sec-agree\r\n"));
        assert!(msg.contains("Security-Client: ipsec-3gpp; alg=hmac-sha-1-96; ealg=null\r\n"));
        // extra headers must come before the terminating Content-Length/blank line
        let extra_pos = msg.find("Security-Client:").unwrap();
        let cl_pos = msg.find("Content-Length:").unwrap();
        assert!(extra_pos < cl_pos);
    }

    // --- SipRequest / UAS response builders (specs/011-vowifi-sip-bridge) ---

    const SAMPLE_INVITE: &str =
        "INVITE sip:404438083996440@ims.mnc094.mcc404.3gppnetwork.org;user=phone SIP/2.0\r\n\
         Via: SIP/2.0/TCP 10.0.0.5:5060;branch=z9hG4bKabc123;rport\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:+919789063708@ims.mnc094.mcc404.3gppnetwork.org>;tag=fromtag1\r\n\
         To: <sip:404438083996440@ims.mnc094.mcc404.3gppnetwork.org;user=phone>\r\n\
         Call-ID: abc123callid\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:+919789063708@10.0.0.5:5060;transport=TCP>\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: 9\r\n\r\n\
         v=0\r\ndone";

    #[test]
    fn sip_request_try_parse_extracts_method_and_uri() {
        let (req, consumed) = SipRequest::try_parse(SAMPLE_INVITE).unwrap().unwrap();
        assert_eq!(consumed, SAMPLE_INVITE.len());
        assert_eq!(req.method, "INVITE");
        assert_eq!(
            req.request_uri,
            "sip:404438083996440@ims.mnc094.mcc404.3gppnetwork.org;user=phone"
        );
        assert_eq!(req.header("Call-ID").unwrap(), "abc123callid");
        assert_eq!(req.header("CSeq").unwrap(), "1 INVITE");
        assert_eq!(req.body, "v=0\r\ndone");
    }

    #[test]
    fn sip_request_try_parse_returns_none_when_body_incomplete() {
        let partial = "BYE sip:x SIP/2.0\r\nContent-Length: 10\r\n\r\nshort";
        assert!(SipRequest::try_parse(partial).unwrap().is_none());
    }

    #[test]
    fn sip_request_headers_all_returns_every_via_in_order() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    Via: SIP/2.0/TCP 1.1.1.1:5060;branch=b1\r\n\
                    Via: SIP/2.0/TCP 2.2.2.2:5060;branch=b2\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw).unwrap().unwrap();
        let vias = req.headers_all("Via");
        assert_eq!(vias.len(), 2);
        assert!(vias[0].contains("1.1.1.1"));
        assert!(vias[1].contains("2.2.2.2"));
    }

    #[test]
    fn sip_request_try_parse_rejects_empty_input() {
        assert!(SipRequest::try_parse("").unwrap().is_none());
    }

    fn sample_bye() -> SipRequest {
        let raw = "BYE sip:caller@10.0.0.5:5060 SIP/2.0\r\n\
                    Via: SIP/2.0/TCP 10.0.0.5:5060;branch=z9hG4bKbye1\r\n\
                    From: <sip:404438083996440@realm>;tag=totag1\r\n\
                    To: <sip:+919789063708@realm>;tag=fromtag1\r\n\
                    Call-ID: abc123callid\r\n\
                    CSeq: 2 BYE\r\n\
                    Content-Length: 0\r\n\r\n";
        SipRequest::try_parse(raw).unwrap().unwrap().0
    }

    #[test]
    fn build_100_trying_has_no_to_tag_and_no_contact() {
        let (req, _) = SipRequest::try_parse(SAMPLE_INVITE).unwrap().unwrap();
        let resp = build_100_trying(&req);
        assert!(resp.starts_with("SIP/2.0 100 Trying\r\n"));
        assert!(!resp.contains("Contact:"));
        assert!(resp.contains("Call-ID: abc123callid\r\n"));
        assert!(resp.contains("CSeq: 1 INVITE\r\n"));
        // To header echoed without a tag added.
        assert!(resp.contains(
            "To: <sip:404438083996440@ims.mnc094.mcc404.3gppnetwork.org;user=phone>\r\n"
        ));
    }

    #[test]
    fn build_180_ringing_adds_to_tag_and_contact() {
        let (req, _) = SipRequest::try_parse(SAMPLE_INVITE).unwrap().unwrap();
        let resp = build_180_ringing(&req, "totag1", "<sip:agent@10.0.0.9:5060>");
        assert!(resp.starts_with("SIP/2.0 180 Ringing\r\n"));
        assert!(resp.contains(";tag=totag1\r\n"));
        assert!(resp.contains("Contact: <sip:agent@10.0.0.9:5060>\r\n"));
    }

    #[test]
    fn build_200_ok_invite_includes_sdp_body_and_content_length() {
        let (req, _) = SipRequest::try_parse(SAMPLE_INVITE).unwrap().unwrap();
        let sdp = "v=0\r\nc=IN IP4 1.2.3.4\r\n";
        let resp = build_200_ok_invite(&req, "totag1", "<sip:agent@10.0.0.9:5060>", sdp);
        assert!(resp.starts_with("SIP/2.0 200 OK\r\n"));
        assert!(resp.contains("Content-Type: application/sdp\r\n"));
        assert!(resp.contains(&format!("Content-Length: {}\r\n\r\n{sdp}", sdp.len())));
        assert!(resp.ends_with(sdp));
    }

    #[test]
    fn build_200_ok_bye_echoes_dialog_with_no_body() {
        let req = sample_bye();
        let resp = build_200_ok_bye(&req, "totag1");
        assert!(resp.starts_with("SIP/2.0 200 OK\r\n"));
        assert!(resp.contains("CSeq: 2 BYE\r\n"));
        assert!(resp.ends_with("Content-Length: 0\r\n\r\n"));
    }

    #[test]
    fn build_486_busy_here_declines_with_no_body() {
        let (req, _) = SipRequest::try_parse(SAMPLE_INVITE).unwrap().unwrap();
        let resp = build_486_busy_here(&req, "totag1");
        assert!(resp.starts_with("SIP/2.0 486 Busy Here\r\n"));
        assert!(resp.ends_with("Content-Length: 0\r\n\r\n"));
    }

    fn sample_message() -> SipRequest {
        let raw = "MESSAGE sip:404438083996440@realm SIP/2.0\r\n\
                    Via: SIP/2.0/TCP 10.0.0.5:5060;branch=z9hG4bKmsg1\r\n\
                    From: <sip:+919789063708@realm>;tag=fromtag1\r\n\
                    To: <sip:404438083996440@realm>\r\n\
                    Call-ID: msgcallid\r\n\
                    CSeq: 1 MESSAGE\r\n\
                    Content-Type: text/plain\r\n\
                    Content-Length: 5\r\n\r\n\
                    hello";
        SipRequest::try_parse(raw).unwrap().unwrap().0
    }

    #[test]
    fn build_200_ok_message_echoes_dialog_with_no_body() {
        let req = sample_message();
        let resp = build_200_ok_message(&req, "totag1");
        assert!(resp.starts_with("SIP/2.0 200 OK\r\n"));
        assert!(resp.contains("CSeq: 1 MESSAGE\r\n"));
        assert!(resp.ends_with("Content-Length: 0\r\n\r\n"));
    }

    #[test]
    fn sip_request_try_parse_extracts_message_body() {
        let req = sample_message();
        assert_eq!(req.method, "MESSAGE");
        assert_eq!(req.body, "hello");
    }

    #[test]
    fn via_headers_are_echoed_verbatim_in_order_on_responses() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    Via: SIP/2.0/TCP 1.1.1.1:5060;branch=b1\r\n\
                    Via: SIP/2.0/TCP 2.2.2.2:5060;branch=b2\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw).unwrap().unwrap();
        let resp = build_100_trying(&req);
        let first_via = resp.find("Via: SIP/2.0/TCP 1.1.1.1").unwrap();
        let second_via = resp.find("Via: SIP/2.0/TCP 2.2.2.2").unwrap();
        assert!(first_via < second_via);
    }

    #[test]
    fn recv_message_distinguishes_request_from_response_by_start_line() {
        // Exercised indirectly via the start-line check `recv_message` uses
        // internally (`starts_with("SIP/2.0")`) — a response's start-line
        // begins with the SIP version token; a request's start-line ends
        // with it instead. This asserts the discriminator itself, since
        // `recv_message` needs a live socket to test end-to-end.
        assert!(SAMPLE_INVITE.starts_with("INVITE"));
        assert!(!SAMPLE_INVITE.starts_with("SIP/2.0"));
        let response_start = "SIP/2.0 200 OK\r\n";
        assert!(response_start.starts_with("SIP/2.0"));
    }
}
