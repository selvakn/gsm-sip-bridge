//! A minimal, purpose-built SIP client for the single transaction we need:
//! send REGISTER, receive a 401 challenge, resend with credentials. This
//! deliberately does not use PJSIP — see the design note in `ims/mod.rs`.

use crate::error::{BridgeError, BridgeResult};
use rand::RngCore;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::time::Duration;

const RECV_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_MSG_LEN: usize = 8192;

pub fn random_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// A parsed SIP response: status line + headers (in original order, values
/// joined if a header name repeats) + reason phrase.
#[derive(Debug, Clone)]
pub struct SipResponse {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
}

impl SipResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn parse(raw: &str) -> BridgeResult<Self> {
        let mut lines = raw.split("\r\n");
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

        let headers = unfolded
            .into_iter()
            .filter_map(|line| {
                line.split_once(':')
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            })
            .collect();

        Ok(Self {
            status,
            reason,
            headers,
        })
    }
}

/// Everything needed to build a REGISTER request.
pub struct RegisterRequest<'a> {
    pub registrar_uri: &'a str,
    pub impi_uri: &'a str,
    pub local_addr: SocketAddr,
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
}

pub fn format_sip_addr(addr: SocketAddr) -> String {
    match addr.ip() {
        IpAddr::V6(ip) => format!("[{ip}]:{}", addr.port()),
        IpAddr::V4(ip) => format!("{ip}:{}", addr.port()),
    }
}

pub fn build_register(req: &RegisterRequest) -> String {
    let via_addr = format_sip_addr(req.local_addr);
    let contact_addr = format_sip_addr(req.local_addr);

    let mut msg = format!(
        "REGISTER sip:{registrar} SIP/2.0\r\n\
         Via: SIP/2.0/{transport} {via_addr};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:{impi}>;tag={from_tag}\r\n\
         To: <sip:{impi}>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} REGISTER\r\n\
         Contact: <sip:{impi_user}@{contact_addr};transport={transport}>;+g.3gpp.icsi-ref=\"urn%3Aurn-7%3A3gpp-service.ims.icsi.mmtel\";audio;+sip.instance=\"<urn:gsma:imei:000000000000000>\"\r\n\
         Expires: {expires}\r\n\
         Allow: OPTIONS, REGISTER, SUBSCRIBE, NOTIFY, PUBLISH, INVITE, ACK, BYE, CANCEL, UPDATE, PRACK, INFO, MESSAGE, REFER\r\n\
         User-Agent: motorola_XT2241-1_Android15_V1SQS35H.58-10-8-9\r\n",
        registrar = req.registrar_uri,
        transport = req.transport,
        via_addr = via_addr,
        branch = req.branch,
        impi = req.impi_uri,
        from_tag = req.from_tag,
        call_id = req.call_id,
        cseq = req.cseq,
        contact_addr = contact_addr,
        impi_user = req.impi_uri.split('@').next().unwrap_or(req.impi_uri),
        expires = req.expires,
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

/// A single UDP or TCP connection to the P-CSCF, held open for the whole
/// REGISTER transaction (initial request + challenge response) so that:
/// (a) the local address is known *before* building the first request (an
/// unspecified `0.0.0.0`/`::` Via/Contact in the first REGISTER is grounds
/// for a P-CSCF to silently drop it), and (b) — for TCP — the same
/// connection carries both requests, as most SIP stacks expect.
pub enum SipTransport {
    Udp(UdpSocket),
    Tcp(TcpStream),
}

impl SipTransport {
    pub fn connect(pcscf: SocketAddr, use_tcp: bool) -> BridgeResult<Self> {
        if use_tcp {
            let stream = TcpStream::connect_timeout(&pcscf, RECV_TIMEOUT)
                .map_err(|e| BridgeError::Ims(format!("TCP connect to {pcscf} failed: {e}")))?;
            stream
                .set_read_timeout(Some(RECV_TIMEOUT))
                .map_err(|e| BridgeError::Ims(format!("set_read_timeout failed: {e}")))?;
            Ok(Self::Tcp(stream))
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
            Ok(Self::Udp(socket))
        }
    }

    pub fn local_addr(&self) -> BridgeResult<SocketAddr> {
        let addr = match self {
            Self::Udp(s) => s.local_addr(),
            Self::Tcp(s) => s.local_addr(),
        };
        addr.map_err(|e| BridgeError::Ims(format!("local_addr failed: {e}")))
    }

    pub fn send_and_recv(&mut self, message: &str) -> BridgeResult<SipResponse> {
        tracing::debug!(message = %message, "sending SIP request");
        let mut buf = [0u8; MAX_MSG_LEN];
        let n = match self {
            Self::Udp(socket) => {
                socket
                    .send(message.as_bytes())
                    .map_err(|e| BridgeError::Ims(format!("UDP send failed: {e}")))?;
                socket
                    .recv(&mut buf)
                    .map_err(|e| BridgeError::Ims(format!("UDP recv failed/timed out: {e}")))?
            }
            Self::Tcp(stream) => {
                stream
                    .write_all(message.as_bytes())
                    .map_err(|e| BridgeError::Ims(format!("TCP send failed: {e}")))?;
                stream
                    .read(&mut buf)
                    .map_err(|e| BridgeError::Ims(format!("TCP recv failed/timed out: {e}")))?
            }
        };
        if n == 0 {
            return Err(BridgeError::Ims(
                "connection closed by peer with no data (0 bytes read)".into(),
            ));
        }
        let text = String::from_utf8_lossy(&buf[..n]).to_string();
        tracing::debug!(bytes = n, response = %text, "received SIP response");
        SipResponse::parse(&text)
    }
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
        let resp = SipResponse::parse(raw).unwrap();
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
        let resp = SipResponse::parse(raw).unwrap();
        assert_eq!(
            resp.header("Contact").unwrap(),
            "<sip:foo@bar> ;expires=600"
        );
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
            impi_uri: "404438083996440@ims.mnc043.mcc404.3gppnetwork.org",
            local_addr: addr,
            call_id: "callid123",
            from_tag: "tag123",
            branch: "z9hG4bKbranch",
            cseq: 1,
            expires: 600,
            transport: "UDP",
            authorization: None,
            extra_headers: &[],
        };
        let msg = build_register(&req);
        assert!(msg.starts_with("REGISTER sip:ims.mnc043.mcc404.3gppnetwork.org SIP/2.0\r\n"));
        assert!(msg.contains("[2402:8100::1]:5060"));
        assert!(msg.contains("CSeq: 1 REGISTER"));
        assert!(msg.ends_with("Content-Length: 0\r\n\r\n"));
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
            impi_uri: "user@example.org",
            local_addr: addr,
            call_id: "callid",
            from_tag: "tag",
            branch: "branch",
            cseq: 1,
            expires: 600,
            transport: "UDP",
            authorization: None,
            extra_headers: &extra,
        };
        let msg = build_register(&req);
        assert!(msg.contains("Supported: sec-agree\r\n"));
        assert!(msg.contains("Security-Client: ipsec-3gpp; alg=hmac-sha-1-96; ealg=null\r\n"));
        // extra headers must come before the terminating Content-Length/blank line
        let extra_pos = msg.find("Security-Client:").unwrap();
        let cl_pos = msg.find("Content-Length:").unwrap();
        assert!(extra_pos < cl_pos);
    }
}
