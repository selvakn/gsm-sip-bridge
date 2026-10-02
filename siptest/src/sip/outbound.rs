//! Outbound call signalling: INVITE, then either the registrar's relayed
//! answer (proxy mode, the bridge's default) or its `302` redirect to
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
    /// The user part of the peer's `Contact` — the Request-URI for in-dialog
    /// requests (RFC 3261 §12.2.1.1). Distinct from `to_user`, the dialled
    /// number the `To` header keeps.
    pub target_user: String,
    pub next_cseq: u32,
}

fn is_valid_destination(destination: &str) -> bool {
    !destination.is_empty()
        && destination
            .chars()
            .all(|c| c.is_ascii_digit() || c == '*' || c == '#' || c == '+')
}

/// Who is calling whom on one call, shared by every helper below.
struct CallCtx<'a> {
    socket: &'a SipSocket,
    from_user: &'a str,
    /// Host for the `From`/`To` URIs (the registrar's name, not its address).
    host: &'a str,
    call_id: &'a str,
    from_tag: &'a str,
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
    let ctx = CallCtx {
        socket,
        from_user,
        host: registrar_host,
        call_id: &call_id,
        from_tag: &from_tag,
    };

    // Every timing below is measured from this first INVITE — what the person
    // dialling experiences — in both proxy and redirect mode, so the two are
    // comparable (redirect mode's include the 302 round trip).
    let start = Instant::now();
    let deadline = start + ring_timeout;

    // --- Phase 1: INVITE the registrar. -----------------------------------
    // Proxy mode (the bridge's default) answers with the relayed call's own
    // responses; redirect mode answers `302`; a refusal is a 3xx–6xx.
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

    let awaited = await_invite_final(&ctx, 1, deadline, start)?;
    let Some(resp1) = awaited.final_response else {
        // In proxy mode this INVITE is live at the dial-out account; it must
        // be cancelled and any answer that races the CANCEL cleaned up.
        abandon_invite(
            &ctx,
            registrar_addr,
            &request_uri,
            destination,
            &branch1,
            1,
            None,
        );
        return Ok(timeout_outcome());
    };

    if (200..300).contains(&resp1.status) {
        // The relayed call's answer: the dialog already exists.
        return answered_outcome(
            &ctx,
            &resp1,
            Answer {
                to_user: destination,
                cseq: 1,
                fallback: None,
                ringing_ms: awaited.ringing_ms,
                start,
                redirect: None,
            },
        );
    }

    if resp1.status != 302 {
        ack_non_2xx(
            &ctx,
            registrar_addr,
            &request_uri,
            destination,
            &resp1,
            &branch1,
            1,
        )?;
        return Ok(refusal_outcome(resp1.status, &resp1.reason));
    }

    let contact = resp1
        .header("Contact")
        .ok_or_else(|| SipTestError::Config("302 with no Contact header".into()))?
        .to_string();
    let (redirect_user, redirect_addr) = parse_redirect_contact(&contact)
        .ok_or_else(|| SipTestError::Config(format!("302 Contact not parseable: {contact}")))?;
    // ACK the 302 — same branch as the INVITE it acknowledges, sent back to
    // the registrar (RFC 3261 §17.1.1.3).
    ack_non_2xx(
        &ctx,
        registrar_addr,
        &request_uri,
        destination,
        &resp1,
        &branch1,
        1,
    )?;

    // --- Phase 2: re-INVITE the redirect target. --------------------------
    let branch2 = new_branch();
    let request_uri2 = sip_uri(&redirect_user, redirect_addr);
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
    socket.send(redirect_addr, &invite2)?;

    let awaited = await_invite_final(&ctx, 2, deadline, start)?;
    let redirect = Some((contact.clone(), redirect_addr.port()));
    let Some(resp2) = awaited.final_response else {
        abandon_invite(
            &ctx,
            redirect_addr,
            &request_uri2,
            &redirect_user,
            &branch2,
            2,
            Some((redirect_user.clone(), redirect_addr)),
        );
        return Ok(OutboundCallOutcome {
            answered: false,
            final_status: 487,
            redirect_contact: Some(contact),
            redirect_port: Some(redirect_addr.port()),
            invite_to_180_ms: awaited.ringing_ms,
            invite_to_200_ms: None,
            remote_target: Some(redirect_addr),
            sdp_answer: None,
            refusal_reason: Some("ring_timeout"),
            dialog: None,
        });
    };

    if (200..300).contains(&resp2.status) {
        return answered_outcome(
            &ctx,
            &resp2,
            Answer {
                to_user: &redirect_user,
                cseq: 2,
                fallback: Some((redirect_user.clone(), redirect_addr)),
                ringing_ms: awaited.ringing_ms,
                start,
                redirect,
            },
        );
    }

    ack_non_2xx(
        &ctx,
        redirect_addr,
        &request_uri2,
        &redirect_user,
        &resp2,
        &branch2,
        2,
    )?;
    Ok(OutboundCallOutcome {
        answered: false,
        final_status: resp2.status,
        redirect_contact: Some(contact),
        redirect_port: Some(redirect_addr.port()),
        invite_to_180_ms: awaited.ringing_ms,
        invite_to_200_ms: None,
        remote_target: Some(redirect_addr),
        sdp_answer: None,
        refusal_reason: refusal_reason_for(resp2.status),
        dialog: None,
    })
}

struct Awaited {
    final_response: Option<SipResponse>,
    /// Time from the first INVITE to the first `180 Ringing` — only a 180: a
    /// `183 Session Progress` is early media, not ringing.
    ringing_ms: Option<u64>,
}

/// Waits for the final response to the INVITE with `cseq`, or `deadline`.
/// Every 1xx is provisional.
fn await_invite_final(
    ctx: &CallCtx,
    cseq: u32,
    deadline: Instant,
    since: Instant,
) -> SipTestResult<Awaited> {
    let mut ringing_ms = None;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(Awaited {
                final_response: None,
                ringing_ms,
            });
        }
        let slice = (deadline - now).min(RESPONSE_POLL);
        let Some(resp) = ctx
            .socket
            .recv_response(ctx.call_id, cseq, "INVITE", slice)?
        else {
            continue;
        };
        if resp.status >= 200 {
            return Ok(Awaited {
                final_response: Some(resp),
                ringing_ms,
            });
        }
        if resp.status == 180 && ringing_ms.is_none() {
            ringing_ms = Some(since.elapsed().as_millis() as u64);
        }
    }
}

/// ACK for a 3xx–6xx final response: hop-by-hop, same branch as the INVITE it
/// closes, sent where that INVITE went (RFC 3261 §17.1.1.3).
fn ack_non_2xx(
    ctx: &CallCtx,
    dst: SocketAddr,
    request_uri: &str,
    to_user: &str,
    resp: &SipResponse,
    invite_branch: &str,
    cseq: u32,
) -> SipTestResult<()> {
    let to_tag = extract_to_tag(resp);
    let ack = build_ack_non_2xx(&AckNon2xxParams {
        request_uri,
        local_addr: ctx.socket.local_addr(),
        from_user: ctx.from_user,
        from_host: ctx.host,
        to_user,
        to_host: ctx.host,
        to_tag: to_tag.as_deref().unwrap_or(""),
        call_id: ctx.call_id,
        from_tag: ctx.from_tag,
        invite_branch,
        cseq,
    });
    ctx.socket.send(dst, &ack)
}

/// ACKs a 2xx and returns the confirmed dialog. The ACK goes to the peer's own
/// `Contact`, with the Contact's user as Request-URI (RFC 3261 §12.2.1.1) —
/// not the dialled number, not the registrar. If the Contact cannot be read
/// and there is no `fallback`, that is an error: in proxy mode the response's
/// own source is the registrar, so there is nothing safe to guess.
fn confirm_2xx(
    ctx: &CallCtx,
    resp: &SipResponse,
    to_user: &str,
    cseq: u32,
    fallback: Option<(String, SocketAddr)>,
) -> SipTestResult<ConfirmedDialog> {
    let to_tag = extract_to_tag(resp).unwrap_or_default();
    let (target_user, target_addr) = match resp.header("Contact").and_then(parse_contact) {
        Some(parsed) => parsed,
        None => fallback.ok_or_else(|| {
            SipTestError::Config(format!(
                "{} OK Contact not parseable: {:?}",
                resp.status,
                resp.header("Contact")
            ))
        })?,
    };
    let ack = build_ack_2xx(&Ack2xxParams {
        request_uri: &sip_uri(&target_user, target_addr),
        local_addr: ctx.socket.local_addr(),
        from_user: ctx.from_user,
        from_host: ctx.host,
        to_user,
        to_host: ctx.host,
        to_tag: &to_tag,
        call_id: ctx.call_id,
        from_tag: ctx.from_tag,
        branch: &new_branch(),
        cseq,
    });
    ctx.socket.send(target_addr, &ack)?;
    Ok(ConfirmedDialog {
        call_id: ctx.call_id.to_string(),
        from_tag: ctx.from_tag.to_string(),
        to_tag,
        from_user: ctx.from_user.to_string(),
        from_host: ctx.host.to_string(),
        to_user: to_user.to_string(),
        to_host: ctx.host.to_string(),
        remote_target: target_addr,
        target_user,
        next_cseq: cseq + 1,
    })
}

struct Answer<'a> {
    to_user: &'a str,
    cseq: u32,
    fallback: Option<(String, SocketAddr)>,
    ringing_ms: Option<u64>,
    start: Instant,
    /// `(Contact, port)` of the 302 that led here, in redirect mode.
    redirect: Option<(String, u16)>,
}

fn answered_outcome(
    ctx: &CallCtx,
    resp: &SipResponse,
    answer: Answer,
) -> SipTestResult<OutboundCallOutcome> {
    let sdp_answer = sdp::parse_answer(&resp.body)?;
    let dialog = confirm_2xx(ctx, resp, answer.to_user, answer.cseq, answer.fallback)?;
    let (redirect_contact, redirect_port) = match answer.redirect {
        Some((contact, port)) => (Some(contact), Some(port)),
        None => (None, None),
    };
    Ok(OutboundCallOutcome {
        answered: true,
        final_status: resp.status,
        redirect_contact,
        redirect_port,
        invite_to_180_ms: answer.ringing_ms,
        invite_to_200_ms: Some(answer.start.elapsed().as_millis() as u64),
        remote_target: Some(dialog.remote_target),
        sdp_answer: Some(sdp_answer),
        refusal_reason: None,
        dialog: Some(dialog),
    })
}

/// How long to keep listening after a CANCEL for the INVITE's own final
/// response, and how often to retransmit the CANCEL until something answers.
const CANCEL_GRACE: Duration = Duration::from_secs(3);
const CANCEL_RETRANSMIT: Duration = Duration::from_millis(500);
const CANCEL_RETRANSMITS: u32 = 3;

/// Gives up on an INVITE: CANCEL it (same CSeq number — RFC 3261 §9.1) and
/// then deal with whatever the race produces, so no call is left up with
/// nobody on it. A `200 OK` that crossed the CANCEL is ACKed and immediately
/// BYEd; a `487` (or any other 3xx–6xx) is ACKed; the CANCEL's own `200` is
/// consumed. Best-effort: nothing here can fail the call that already failed.
fn abandon_invite(
    ctx: &CallCtx,
    dst: SocketAddr,
    request_uri: &str,
    to_user: &str,
    invite_branch: &str,
    cseq: u32,
    fallback: Option<(String, SocketAddr)>,
) {
    let send = || {
        let _ = send_cancel(
            ctx.socket,
            dst,
            request_uri,
            ctx.from_user,
            ctx.host,
            to_user,
            ctx.call_id,
            ctx.from_tag,
            invite_branch,
            cseq,
        );
    };
    send();
    let deadline = Instant::now() + CANCEL_GRACE;
    let mut retransmits = 0;
    let mut cancel_answered = false;
    while Instant::now() < deadline {
        match ctx
            .socket
            .recv_response(ctx.call_id, cseq, "INVITE", CANCEL_RETRANSMIT)
        {
            Ok(Some(resp)) if resp.status >= 200 => {
                if (200..300).contains(&resp.status) {
                    // Answered just before the CANCEL took effect.
                    if let Ok(dialog) = confirm_2xx(ctx, &resp, to_user, cseq, fallback) {
                        let _ = send_bye(ctx.socket, &dialog);
                    }
                } else {
                    let _ = ack_non_2xx(ctx, dst, request_uri, to_user, &resp, invite_branch, cseq);
                }
                return;
            }
            Ok(Some(_)) => continue, // a late provisional
            _ => {}
        }
        if !cancel_answered {
            cancel_answered = matches!(
                ctx.socket
                    .recv_response(ctx.call_id, cseq, "CANCEL", Duration::ZERO),
                Ok(Some(_))
            );
        }
        if !cancel_answered && retransmits < CANCEL_RETRANSMITS {
            retransmits += 1;
            send();
        }
    }
}

/// Ends a confirmed dialog. Reuses `ims::sip_client::build_bye` — its
/// `from`/`to` take full, already role-swapped header values, so it works
/// for the UAC role this call is in just as well as the UAS role it was
/// written for.
pub fn send_bye(socket: &SipSocket, dialog: &ConfirmedDialog) -> SipTestResult<()> {
    use gsm_sip_bridge::ims::sip_client::{build_bye, ByeRequest};
    let request_uri = sip_uri(&dialog.target_user, dialog.remote_target);
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

fn extract_to_tag(resp: &SipResponse) -> Option<String> {
    let to = resp.header("To")?;
    to.split(';')
        .find_map(|part| part.trim().strip_prefix("tag="))
        .map(|s| s.to_string())
}

/// `sip:user@addr` (or `sip:addr` when there is no user part).
fn sip_uri(user: &str, addr: SocketAddr) -> String {
    if user.is_empty() {
        format!("sip:{addr}")
    } else {
        format!("sip:{user}@{addr}")
    }
}

/// Tolerant parser for a `Contact`-style value: `<sip:user@host:port;params>`,
/// bare `sip:user@host:port`, with or without a display name, a port, a user,
/// or URI parameters; header parameters after the `>` are ignored. `host` may
/// be an IP literal or a name (resolved), and the port defaults to 5060.
pub(crate) fn parse_contact(header_value: &str) -> Option<(String, SocketAddr)> {
    parse_contact_inner(header_value).map(|(user, addr, _)| (user, addr))
}

/// [`parse_contact`] for a `302`'s Contact, which must name its port: that
/// port is the real dial-out account's (5072, 5062, 5073 depending on the
/// deployment — research.md R3), so defaulting it would silently dial the
/// wrong place.
fn parse_redirect_contact(header_value: &str) -> Option<(String, SocketAddr)> {
    parse_contact_inner(header_value)
        .filter(|(_, _, explicit_port)| *explicit_port)
        .map(|(user, addr, _)| (user, addr))
}

/// `(user, address, whether the URI named a port)`.
fn parse_contact_inner(header_value: &str) -> Option<(String, SocketAddr, bool)> {
    let v = header_value.trim();
    let uri = if let Some(start) = v.find('<') {
        let end = v[start..].find('>').map(|e| start + e)?;
        &v[start + 1..end]
    } else {
        // Bare form: anything after `;` belongs to the header, not the URI.
        v.split(',').next()?.split(';').next()?.trim()
    };
    let rest = uri
        .strip_prefix("sip:")
        .or_else(|| uri.strip_prefix("sips:"))?;
    // URI parameters and headers sit after the host[:port].
    let (user, hostport) = match rest.rsplit_once('@') {
        Some((user, hostport)) => (user, hostport),
        None => ("", rest),
    };
    let hostport = hostport.split([';', '?']).next()?.trim();
    if hostport.is_empty() {
        return None;
    }
    let (addr, explicit_port) = if let Ok(addr) = hostport.parse::<SocketAddr>() {
        (addr, true)
    } else if let Ok(ip) = hostport
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
    {
        // An IP literal (v4, or `[v6]`) with no port.
        (SocketAddr::new(ip, 5060), false)
    } else {
        // A hostname, with or without a port.
        let explicit = hostport.contains(':');
        let with_port = if explicit {
            hostport.to_string()
        } else {
            format!("{hostport}:5060")
        };
        let addr = std::net::ToSocketAddrs::to_socket_addrs(&with_port.as_str())
            .ok()?
            .next()?;
        (addr, explicit)
    };
    Some((user.to_string(), addr, explicit_port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bracketed_contact_with_params() {
        let (user, addr) =
            parse_contact("<sip:+919000000000@192.168.15.10:5072>;+g.3gpp.icsi-ref=\"foo\"")
                .unwrap();
        assert_eq!(user, "+919000000000");
        assert_eq!(addr, "192.168.15.10:5072".parse().unwrap());
    }

    #[test]
    fn parses_bare_contact_without_brackets() {
        let (user, addr) = parse_contact("sip:1002@192.168.15.10:5060").unwrap();
        assert_eq!(user, "1002");
        assert_eq!(addr, "192.168.15.10:5060".parse().unwrap());
    }

    #[test]
    fn uri_parameters_inside_the_brackets_do_not_break_the_address() {
        let (user, addr) = parse_contact("<sip:agentb@10.0.0.5:5072;transport=udp>").unwrap();
        assert_eq!(user, "agentb");
        assert_eq!(addr, "10.0.0.5:5072".parse().unwrap());
        let (_, addr) =
            parse_contact("<sip:agentb@10.0.0.5:5072;transport=udp;ob>;expires=60").unwrap();
        assert_eq!(addr, "10.0.0.5:5072".parse().unwrap());
        let (_, addr) = parse_contact("<sip:agentb@10.0.0.5:5072?Replaces=x>").unwrap();
        assert_eq!(addr, "10.0.0.5:5072".parse().unwrap());
    }

    #[test]
    fn display_names_missing_ports_and_missing_users_are_accepted() {
        let (user, addr) = parse_contact("\"Agent B\" <sip:agentb@10.0.0.5>").unwrap();
        assert_eq!(
            (user.as_str(), addr),
            ("agentb", "10.0.0.5:5060".parse().unwrap())
        );
        let (user, addr) = parse_contact("<sip:10.0.0.5:5072>").unwrap();
        assert_eq!(
            (user.as_str(), addr),
            ("", "10.0.0.5:5072".parse().unwrap())
        );
        let (_, addr) = parse_contact("<sip:agentb@[::1]:5072>").unwrap();
        assert_eq!(addr, "[::1]:5072".parse().unwrap());
        let (_, addr) = parse_contact("<sip:agentb@[::1]>").unwrap();
        assert_eq!(addr, "[::1]:5060".parse().unwrap());
        let (_, addr) = parse_contact("<sip:agentb@localhost:5072>").unwrap();
        assert_eq!(addr.port(), 5072);
    }

    #[test]
    fn a_redirect_contact_must_name_its_port() {
        assert!(parse_redirect_contact("<sip:x@10.0.0.5>").is_none());
        assert!(parse_redirect_contact("<sip:x@localhost>").is_none());
        let (_, addr) = parse_redirect_contact("<sip:x@10.0.0.5:5072;transport=udp>").unwrap();
        assert_eq!(addr.port(), 5072);
    }

    #[test]
    fn a_contact_that_is_not_a_sip_uri_is_rejected() {
        assert!(parse_contact("<tel:+919000000000>").is_none());
        assert!(parse_contact("<sip:agentb@>").is_none());
        assert!(parse_contact("").is_none());
    }

    #[test]
    fn a_sip_uri_without_a_user_has_no_at_sign() {
        let addr: SocketAddr = "10.0.0.5:5072".parse().unwrap();
        assert_eq!(sip_uri("", addr), "sip:10.0.0.5:5072");
        assert_eq!(sip_uri("agentb", addr), "sip:agentb@10.0.0.5:5072");
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
