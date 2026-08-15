//! Outbound call signalling: INVITE, the registrar's `302` redirect to
//! whichever port is actually hosting the telephony agent, ACK, and the
//! re-INVITE that follows (contracts/sip-flows.md C-2).
//!
//! The redirect target is **always** taken from the `302`'s own `Contact`
//! header, never from configuration — research.md R3: that port is 5072 only
//! because VoWiFi is enabled on this deployment; it is 5062 for
//! circuit-switched and 5073 for VoLTE.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use gsm_sip_bridge::ims::sip_client::SipResponse;

use crate::error::{SipTestError, SipTestResult};
use crate::media::codec::CodecProfile;
use crate::sdp::{self, SdpAnswer};
use crate::sip::message::{
    build_ack_2xx, build_ack_non_2xx, build_cancel, build_invite, new_branch, Ack2xxParams,
    AckNon2xxParams, CancelParams, InviteParams,
};
use crate::sip::socket::SipSocket;

const RESPONSE_POLL: Duration = Duration::from_secs(2);

pub struct OutboundCallOutcome {
    pub answered: bool,
    pub final_status: u16,
    pub redirect_contact: Option<String>,
    pub redirect_port: Option<u16>,
    pub invite_to_180_ms: Option<u64>,
    pub invite_to_200_ms: Option<u64>,
    pub remote_target: Option<SocketAddr>,
    pub sdp_answer: Option<SdpAnswer>,
    pub refusal_reason: Option<&'static str>,
    /// Enough of the confirmed dialog to send an in-dialog BYE later. `None`
    /// unless `answered` is true.
    pub dialog: Option<ConfirmedDialog>,
}

#[derive(Clone)]
pub struct ConfirmedDialog {
    pub call_id: String,
    pub from_tag: String,
    pub to_tag: String,
    pub from_user: String,
    pub from_host: String,
    pub to_user: String,
    pub to_host: String,
    pub remote_target: SocketAddr,
    pub next_cseq: u32,
}

fn is_valid_destination(destination: &str) -> bool {
    !destination.is_empty()
        && destination
            .chars()
            .all(|c| c.is_ascii_digit() || c == '*' || c == '#' || c == '+')
}

#[allow(clippy::too_many_arguments)]
pub fn place_call(
    socket: &SipSocket,
    registrar_addr: SocketAddr,
    registrar_host: &str,
    from_user: &str,
    destination: &str,
    codec: CodecProfile,
    rtp_port: u16,
    ring_timeout: Duration,
) -> SipTestResult<OutboundCallOutcome> {
    if !is_valid_destination(destination) {
        return Err(SipTestError::InvalidDestination(destination.to_string()));
    }

    let call_id = crate::sip::message::new_tag();
    let from_tag = crate::sip::message::new_tag();
    let session_id: u64 = rand::random();
    let offer = sdp::build_offer(socket.local_ip, rtp_port, session_id, codec);

    let start = Instant::now();

    // --- Phase 1: INVITE the registrar, expect a 302 (or a refusal). -----
    let branch1 = new_branch();
    let request_uri = format!("sip:{destination}@{registrar_addr}");
    let invite1 = build_invite(&InviteParams {
        request_uri: &request_uri,
        local_addr: socket.local_addr(),
        from_user,
        from_host: registrar_host,
        to_user: destination,
        to_host: registrar_host,
        call_id: &call_id,
        from_tag: &from_tag,
        branch: &branch1,
        cseq: 1,
        sdp_body: &offer,
    });
    socket.send(registrar_addr, &invite1)?;

    let resp1 = wait_final_response(socket, &call_id, ring_timeout)?;
    let Some(resp1) = resp1 else {
        return Ok(timeout_outcome());
    };

    if resp1.status != 302 {
        return Ok(refusal_outcome(resp1.status, &resp1.reason));
    }

    let contact = resp1
        .header("Contact")
        .ok_or_else(|| SipTestError::Config("302 with no Contact header".into()))?
        .to_string();
    let (redirect_user, redirect_addr) = parse_contact_uri(&contact)
        .ok_or_else(|| SipTestError::Config(format!("302 Contact not parseable: {contact}")))?;
    let to_tag_302 = extract_to_tag(&resp1);

    // ACK the 302 — same branch as the INVITE it acknowledges, sent back to
    // the registrar (RFC 3261 §17.1.1.3).
    let ack302 = build_ack_non_2xx(&AckNon2xxParams {
        request_uri: &request_uri,
        local_addr: socket.local_addr(),
        from_user,
        from_host: registrar_host,
        to_user: destination,
        to_host: registrar_host,
        to_tag: to_tag_302.as_deref().unwrap_or(""),
        call_id: &call_id,
        from_tag: &from_tag,
        invite_branch: &branch1,
        cseq: 1,
    });
    socket.send(registrar_addr, &ack302)?;

    // --- Phase 2: re-INVITE the redirect target. --------------------------
    let branch2 = new_branch();
    let request_uri2 = format!("sip:{redirect_user}@{redirect_addr}");
    let invite2 = build_invite(&InviteParams {
        request_uri: &request_uri2,
        local_addr: socket.local_addr(),
        from_user,
        from_host: registrar_host,
        to_user: &redirect_user,
        to_host: registrar_host,
        call_id: &call_id,
        from_tag: &from_tag,
        branch: &branch2,
        cseq: 2,
        sdp_body: &offer,
    });
    let invite2_sent_at = Instant::now();
    socket.send(redirect_addr, &invite2)?;

    let mut invite_to_180_ms = None;
    let mut invite_to_200_ms = None;
    let deadline = start + ring_timeout;

    loop {
        if Instant::now() >= deadline {
            let _ = send_cancel(
                socket,
                redirect_addr,
                &request_uri2,
                from_user,
                registrar_host,
                &redirect_user,
                &call_id,
                &from_tag,
                &branch2,
                3,
            );
            return Ok(OutboundCallOutcome {
                answered: false,
                final_status: 487,
                redirect_contact: Some(contact),
                redirect_port: Some(redirect_addr.port()),
                invite_to_180_ms,
                invite_to_200_ms,
                remote_target: Some(redirect_addr),
                sdp_answer: None,
                refusal_reason: Some("ring_timeout"),
                dialog: None,
            });
        }
        let Some(resp) = socket.recv_response(&call_id, RESPONSE_POLL)? else {
            continue;
        };
        match resp.status {
            100 => continue,
            180 | 183 => {
                if invite_to_180_ms.is_none() {
                    invite_to_180_ms = Some(invite2_sent_at.elapsed().as_millis() as u64);
                }
                continue;
            }
            200 => {
                invite_to_200_ms = Some(invite2_sent_at.elapsed().as_millis() as u64);
                let sdp_answer = sdp::parse_answer(&resp.body)?;
                let to_tag = extract_to_tag(&resp).unwrap_or_default();
                let ack_target = resp
                    .header("Contact")
                    .and_then(parse_contact_uri)
                    .map(|(_, addr)| addr)
                    .unwrap_or(redirect_addr);
                let ack_ruri = format!("sip:{redirect_user}@{ack_target}");
                let branch3 = new_branch();
                let ack2xx = build_ack_2xx(&Ack2xxParams {
                    request_uri: &ack_ruri,
                    local_addr: socket.local_addr(),
                    from_user,
                    from_host: registrar_host,
                    to_user: &redirect_user,
                    to_host: registrar_host,
                    to_tag: &to_tag,
                    call_id: &call_id,
                    from_tag: &from_tag,
                    branch: &branch3,
                    cseq: 2,
                });
                socket.send(ack_target, &ack2xx)?;
                let dialog = ConfirmedDialog {
                    call_id: call_id.clone(),
                    from_tag: from_tag.clone(),
                    to_tag,
                    from_user: from_user.to_string(),
                    from_host: registrar_host.to_string(),
                    to_user: redirect_user.clone(),
                    to_host: registrar_host.to_string(),
                    remote_target: ack_target,
                    next_cseq: 3,
                };
                return Ok(OutboundCallOutcome {
                    answered: true,
                    final_status: 200,
                    redirect_contact: Some(contact),
                    redirect_port: Some(redirect_addr.port()),
                    invite_to_180_ms,
                    invite_to_200_ms,
                    remote_target: Some(ack_target),
                    sdp_answer: Some(sdp_answer),
                    refusal_reason: None,
                    dialog: Some(dialog),
                });
            }
            status => {
                return Ok(OutboundCallOutcome {
                    answered: false,
                    final_status: status,
                    redirect_contact: Some(contact),
                    redirect_port: Some(redirect_addr.port()),
                    invite_to_180_ms,
                    invite_to_200_ms,
                    remote_target: Some(redirect_addr),
                    sdp_answer: None,
                    refusal_reason: refusal_reason_for(status),
                    dialog: None,
                });
            }
        }
    }
}

/// Ends a confirmed dialog. Reuses `ims::sip_client::build_bye` — its
/// `from`/`to` take full, already role-swapped header values, so it works
/// for the UAC role this call is in just as well as the UAS role it was
/// written for.
pub fn send_bye(socket: &SipSocket, dialog: &ConfirmedDialog) -> SipTestResult<()> {
    use gsm_sip_bridge::ims::sip_client::{build_bye, ByeRequest};
    let request_uri = format!("sip:{}@{}", dialog.to_user, dialog.remote_target);
    let branch = new_branch();
    let from = format!(
        "<sip:{}@{}>;tag={}",
        dialog.from_user, dialog.from_host, dialog.from_tag
    );
    let to = format!(
        "<sip:{}@{}>;tag={}",
        dialog.to_user, dialog.to_host, dialog.to_tag
    );
    let msg = build_bye(&ByeRequest {
        request_uri: &request_uri,
        route_headers: &[],
        via_transport: "UDP",
        local_addr: socket.local_addr(),
        from: &from,
        to: &to,
        call_id: &dialog.call_id,
        cseq: dialog.next_cseq,
        branch: &branch,
    });
    socket.send(dialog.remote_target, &msg)
}

fn refusal_reason_for(status: u16) -> Option<&'static str> {
    match status {
        403 => Some("untrusted_source"),
        484 => Some("invalid_destination"),
        503 => Some("no_idle_line"),
        400 => Some("no_user_part"),
        _ => None,
    }
}

fn timeout_outcome() -> OutboundCallOutcome {
    OutboundCallOutcome {
        answered: false,
        final_status: 0,
        redirect_contact: None,
        redirect_port: None,
        invite_to_180_ms: None,
        invite_to_200_ms: None,
        remote_target: None,
        sdp_answer: None,
        refusal_reason: Some("no_response"),
        dialog: None,
    }
}

fn refusal_outcome(status: u16, _reason: &str) -> OutboundCallOutcome {
    OutboundCallOutcome {
        answered: false,
        final_status: status,
        redirect_contact: None,
        redirect_port: None,
        invite_to_180_ms: None,
        invite_to_200_ms: None,
        remote_target: None,
        sdp_answer: None,
        refusal_reason: refusal_reason_for(status),
        dialog: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn send_cancel(
    socket: &SipSocket,
    dst: SocketAddr,
    request_uri: &str,
    from_user: &str,
    from_host: &str,
    to_user: &str,
    call_id: &str,
    from_tag: &str,
    invite_branch: &str,
    cseq: u32,
) -> SipTestResult<()> {
    let msg = build_cancel(&CancelParams {
        request_uri,
        local_addr: socket.local_addr(),
        from_user,
        from_host,
        to_user,
        to_host: from_host,
        call_id,
        from_tag,
        invite_branch,
        cseq,
    });
    socket.send(dst, &msg)
}

fn wait_final_response(
    socket: &SipSocket,
    call_id: &str,
    timeout: Duration,
) -> SipTestResult<Option<SipResponse>> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        if let Some(resp) = socket.recv_response(call_id, RESPONSE_POLL)? {
            if resp.status >= 200 {
                return Ok(Some(resp));
            }
            // provisional (1xx) — keep waiting for the final response.
        }
    }
}

fn extract_to_tag(resp: &SipResponse) -> Option<String> {
    let to = resp.header("To")?;
    to.split(';')
        .find_map(|part| part.trim().strip_prefix("tag="))
        .map(|s| s.to_string())
}

/// Tolerant parser for a `Contact`-style URI: `<sip:user@host:port>` or bare
/// `sip:user@host:port`, optionally followed by `;params`.
fn parse_contact_uri(header_value: &str) -> Option<(String, SocketAddr)> {
    let v = header_value.trim();
    let inner = if let Some(start) = v.find('<') {
        let end = v[start..].find('>').map(|e| start + e)?;
        &v[start + 1..end]
    } else {
        v.split(';').next().unwrap_or(v)
    };
    let inner = inner
        .strip_prefix("sip:")
        .or_else(|| inner.strip_prefix("sips:"))?;
    let (user, hostport) = inner.split_once('@')?;
    let addr: SocketAddr = hostport.parse().ok()?;
    Some((user.to_string(), addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bracketed_contact_with_params() {
        let (user, addr) =
            parse_contact_uri("<sip:+919000000000@192.168.15.10:5072>;+g.3gpp.icsi-ref=\"foo\"")
                .unwrap();
        assert_eq!(user, "+919000000000");
        assert_eq!(addr, "192.168.15.10:5072".parse().unwrap());
    }

    #[test]
    fn parses_bare_contact_without_brackets() {
        let (user, addr) = parse_contact_uri("sip:1002@192.168.15.10:5060").unwrap();
        assert_eq!(user, "1002");
        assert_eq!(addr, "192.168.15.10:5060".parse().unwrap());
    }

    #[test]
    fn invalid_destination_is_refused_before_any_signalling() {
        let socket = SipSocket::bind(
            Some("127.0.0.1".parse().unwrap()),
            0,
            "127.0.0.1:1".parse().unwrap(),
        )
        .unwrap();
        let result = place_call(
            &socket,
            "127.0.0.1:1".parse().unwrap(),
            "gsm-sip-bridge",
            "1002",
            "not-a-number!",
            crate::media::codec::PCMU,
            40000,
            Duration::from_millis(100),
        );
        match result {
            Err(SipTestError::InvalidDestination(_)) => {}
            other => panic!("expected InvalidDestination, got {}", other.is_ok()),
        }
    }
}
