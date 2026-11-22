//! Wire-level helpers for the inbound (UAS) call path (contracts/sip-flows.md
//! C-3). Orchestration — policy, media, reporting — lives in
//! [`crate::call::execute_inbound_call`], mirroring how outbound signalling
//! (`sip::outbound`) is separate from its orchestration (`call::execute_outbound_call`).

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use gsm_sip_bridge::ims::sip_client::{build_uas_response_with_headers, SipRequest};

use crate::call::CallerId;
use crate::sip::socket::SipSocket;

pub const ALLOW: &str = "INVITE, ACK, BYE, CANCEL, OPTIONS";

/// The bridge rewrites `From` and adds `P-Asserted-Identity` /
/// `X-GSM-Caller-ID` (research.md, quickstart.md troubleshooting table) — all
/// three are captured **separately** (FR-015) so a caller-ID propagation bug
/// shows up as a disagreement between them rather than being silently
/// resolved to one value.
pub fn extract_caller_id(req: &SipRequest) -> CallerId {
    CallerId {
        from: req.header("From").map(|s| s.to_string()),
        p_asserted_identity: req.header("P-Asserted-Identity").map(|s| s.to_string()),
        x_gsm_caller_id: req.header("X-GSM-Caller-ID").map(|s| s.to_string()),
    }
}

/// `OPTIONS` keepalive — answered so the bridge never marks us dead
/// (the same reasoning the registrar itself documents for why it answers
/// every request, `sip/server/mod.rs`'s module doc).
pub fn build_options_ok(req: &SipRequest) -> String {
    build_uas_response_with_headers(200, "OK", req, None, None, None, &[("Allow", ALLOW)])
}

/// Any method this UAS does not implement.
pub fn build_405(req: &SipRequest) -> String {
    build_uas_response_with_headers(
        405,
        "Method Not Allowed",
        req,
        None,
        None,
        None,
        &[("Allow", ALLOW)],
    )
}

/// A `Require:` extension we do not support (research.md R10 — never
/// advertise `100rel`/`timer`, and refuse them cleanly if a peer requires
/// them of us).
pub fn build_420(req: &SipRequest, unsupported: &str) -> String {
    build_uas_response_with_headers(
        420,
        "Bad Extension",
        req,
        None,
        None,
        None,
        &[("Unsupported", unsupported)],
    )
}

/// The offer named a payload type we cannot answer with.
pub fn build_488(req: &SipRequest, to_tag: &str) -> String {
    build_uas_response_with_headers(
        488,
        "Not Acceptable Here",
        req,
        Some(to_tag),
        None,
        None,
        &[],
    )
}

/// A second inbound call while one is already active (FR-017).
pub fn build_busy(req: &SipRequest) -> String {
    gsm_sip_bridge::ims::sip_client::build_486_busy_here(req, "busy")
}

/// A rejection at a caller/policy-configured status other than 486, which
/// already has its own builder.
pub fn build_reject(req: &SipRequest, to_tag: &str, status: u16) -> String {
    let reason = reason_phrase(status);
    build_uas_response_with_headers(status, reason, req, Some(to_tag), None, None, &[])
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        486 => "Busy Here",
        480 => "Temporarily Unavailable",
        603 => "Decline",
        403 => "Forbidden",
        _ => "Rejected",
    }
}

/// Handles a request encountered while waiting for something else (a CANCEL,
/// an ACK) during the call whose `Call-ID` is `current_call_id`: a *second*
/// concurrent INVITE gets busied out (FR-017), an OPTIONS keepalive gets
/// answered, anything else is ignored. A retransmission of the INVITE already
/// being handled is also ignored — answering it `486` would end the very call
/// it duplicates.
pub fn handle_stray(socket: &SipSocket, req: &SipRequest, peer: SocketAddr, current_call_id: &str) {
    match req.method.as_str() {
        "OPTIONS" => {
            let _ = socket.send(peer, &build_options_ok(req));
        }
        "INVITE" if req.header("Call-ID") == Some(current_call_id) => {}
        "INVITE" => {
            let _ = socket.send(peer, &build_busy(req));
        }
        _ => {}
    }
}

pub enum WaitOutcome {
    TimedOut,
    Cancelled(SipRequest),
}

/// Waits up to `duration` for a `CANCEL` matching `call_id`, handling any
/// unrelated request seen along the way via [`handle_stray`]. Only requests
/// from `trusted` are ever seen ([`SipSocket::recv_request_from`]).
pub fn wait_or_cancel(
    socket: &SipSocket,
    trusted: IpAddr,
    call_id: &str,
    duration: Duration,
) -> WaitOutcome {
    let deadline = Instant::now() + duration;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return WaitOutcome::TimedOut;
        }
        let slice = (deadline - now).min(Duration::from_millis(300));
        match socket.recv_request_from(trusted, slice) {
            Ok(Some((req, peer))) => {
                if req.method == "CANCEL" && req.header("Call-ID") == Some(call_id) {
                    return WaitOutcome::Cancelled(req);
                }
                handle_stray(socket, &req, peer, call_id);
            }
            _ => continue,
        }
    }
}

/// Retransmits `resend()`'s result on a T1 ladder (500ms doubling, capped at
/// 4s, abandoned at 64×T1 ≈ 32s — sip-flows.md C-3) until an `ACK` for
/// `call_id` arrives, a duplicate INVITE is seen (the peer's own retransmit,
/// because our 200 was lost — resend rather than wait out the ladder), or the
/// ladder is exhausted.
pub fn wait_for_ack(socket: &SipSocket, trusted: IpAddr, call_id: &str, resend: impl Fn()) -> bool {
    let mut interval = Duration::from_millis(500);
    let mut elapsed = Duration::ZERO;
    let cap = Duration::from_secs(32);
    loop {
        match socket.recv_request_from(trusted, interval) {
            Ok(Some((req, peer))) => {
                if req.method == "ACK" && req.header("Call-ID") == Some(call_id) {
                    return true;
                }
                if req.method == "INVITE" && req.header("Call-ID") == Some(call_id) {
                    resend();
                } else {
                    handle_stray(socket, &req, peer, call_id);
                }
            }
            Ok(None) => {
                elapsed += interval;
                if elapsed >= cap {
                    return false;
                }
                resend();
                interval = (interval * 2).min(Duration::from_secs(4));
            }
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn canned_request(method: &str, extra_headers: &[(&str, &str)]) -> SipRequest {
        let mut headers = vec![
            (
                "Via".to_string(),
                "SIP/2.0/UDP 192.168.15.10:5072;branch=z9hG4bK1".to_string(),
            ),
            (
                "From".to_string(),
                "<sip:+919000000000@192.168.15.10:5060>;tag=callertag".to_string(),
            ),
            (
                "To".to_string(),
                "<sip:1002@192.168.15.10:5065>".to_string(),
            ),
            ("Call-ID".to_string(), "inbound-call-1".to_string()),
            ("CSeq".to_string(), format!("1 {method}")),
            ("Content-Length".to_string(), "0".to_string()),
        ];
        for (k, v) in extra_headers {
            headers.push((k.to_string(), v.to_string()));
        }
        SipRequest {
            method: method.to_string(),
            request_uri: "sip:1002@192.168.15.10:5065".to_string(),
            headers,
            body: String::new(),
            body_bytes: Vec::new(),
        }
    }

    #[test]
    fn a_retransmitted_invite_is_ignored_but_a_different_call_is_busied() {
        let a = SipSocket::bind(
            Some("127.0.0.1".parse().unwrap()),
            0,
            "127.0.0.1:1".parse().unwrap(),
        )
        .unwrap();
        let b = SipSocket::bind(
            Some("127.0.0.1".parse().unwrap()),
            0,
            "127.0.0.1:1".parse().unwrap(),
        )
        .unwrap();
        let duplicate = canned_request("INVITE", &[]); // Call-ID inbound-call-1
        handle_stray(&a, &duplicate, b.local_addr(), "inbound-call-1");
        assert!(
            b.recv_response("inbound-call-1", 1, "INVITE", Duration::from_millis(300))
                .unwrap()
                .is_none(),
            "a retransmission must not be answered"
        );

        handle_stray(&a, &duplicate, b.local_addr(), "some-other-call");
        let busy = b
            .recv_response("inbound-call-1", 1, "INVITE", Duration::from_secs(2))
            .unwrap()
            .expect("a second concurrent call is busied out");
        assert_eq!(busy.status, 486);
    }

    /// The pre-answer waits share the source check: an unauthenticated peer
    /// gets no reply to an `OPTIONS`, and its `CANCEL` cannot end a call
    /// ringing for someone else.
    #[test]
    fn the_pre_answer_waits_hear_only_the_trusted_address() {
        let bind = || {
            SipSocket::bind(
                Some("127.0.0.1".parse().unwrap()),
                0,
                "127.0.0.1:1".parse().unwrap(),
            )
            .unwrap()
        };
        let untrusted: IpAddr = "127.0.0.2".parse().unwrap();
        let trusted: IpAddr = "127.0.0.1".parse().unwrap();

        // Sender is 127.0.0.1; with 127.0.0.2 trusted it is the intruder.
        let (a, b) = (bind(), bind());
        b.send(
            a.local_addr(),
            &gsm_sip_bridge::ims::sip_client::build_options(
                &gsm_sip_bridge::ims::sip_client::OptionsRequest {
                    request_uri: "1002@127.0.0.1",
                    local_addr: b.local_addr(),
                    transport: "UDP",
                    public_uri: "sip:b@127.0.0.1",
                    from_tag: "t",
                    call_id: "probe-1",
                    cseq: 1,
                    branch: "z9hG4bKprobe",
                    security_verify: None,
                },
            ),
        )
        .unwrap();
        assert!(matches!(
            wait_or_cancel(&a, untrusted, "inbound-call-1", Duration::from_millis(400)),
            WaitOutcome::TimedOut
        ));
        assert!(
            b.recv_response("probe-1", 1, "OPTIONS", Duration::from_millis(200))
                .unwrap()
                .is_none(),
            "an untrusted OPTIONS was answered"
        );

        // The same request from the trusted address is answered.
        let (a, b) = (bind(), bind());
        b.send(
            a.local_addr(),
            &gsm_sip_bridge::ims::sip_client::build_options(
                &gsm_sip_bridge::ims::sip_client::OptionsRequest {
                    request_uri: "1002@127.0.0.1",
                    local_addr: b.local_addr(),
                    transport: "UDP",
                    public_uri: "sip:b@127.0.0.1",
                    from_tag: "t",
                    call_id: "probe-2",
                    cseq: 1,
                    branch: "z9hG4bKprobe2",
                    security_verify: None,
                },
            ),
        )
        .unwrap();
        let _ = wait_or_cancel(&a, trusted, "inbound-call-1", Duration::from_millis(400));
        assert!(
            b.recv_response("probe-2", 1, "OPTIONS", Duration::from_secs(2))
                .unwrap()
                .is_some(),
            "a trusted OPTIONS was not answered"
        );
    }

    #[test]
    fn caller_id_captures_all_three_headers_separately() {
        let req = canned_request(
            "INVITE",
            &[
                ("P-Asserted-Identity", "sip:+919000000000@ims.example.org"),
                ("X-GSM-Caller-ID", "+919000000000"),
            ],
        );
        let id = extract_caller_id(&req);
        assert!(id.from.unwrap().contains("+919000000000"));
        assert_eq!(
            id.p_asserted_identity.unwrap(),
            "sip:+919000000000@ims.example.org"
        );
        assert_eq!(id.x_gsm_caller_id.unwrap(), "+919000000000");
    }

    #[test]
    fn caller_id_fields_are_independently_absent_when_not_sent() {
        let req = canned_request("INVITE", &[]);
        let id = extract_caller_id(&req);
        assert!(id.from.is_some());
        assert!(id.p_asserted_identity.is_none());
        assert!(id.x_gsm_caller_id.is_none());
    }

    #[test]
    fn options_is_answered_with_allow() {
        let req = canned_request("OPTIONS", &[]);
        let resp = build_options_ok(&req);
        assert!(resp.starts_with("SIP/2.0 200 OK"));
        assert!(resp.contains("Allow: INVITE, ACK, BYE, CANCEL, OPTIONS"));
    }

    #[test]
    fn unrecognised_method_gets_405_with_allow() {
        let req = canned_request("SUBSCRIBE", &[]);
        let resp = build_405(&req);
        assert!(resp.starts_with("SIP/2.0 405"));
        assert!(resp.contains("Allow:"));
    }
}
