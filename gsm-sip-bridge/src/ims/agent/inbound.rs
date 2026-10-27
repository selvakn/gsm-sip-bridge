//! Answering — or declining — one carrier `INVITE`.
//!
//! Split out of `agent::mod` because the inbound path has its own distinct
//! shape: it must hold the carrier in the *ringing* state (real ringback to
//! the caller) while a human decides whether to pick up the PBX extension, and
//! service the carrier's own signaling throughout in case the caller gives up
//! first. That wait ([`await_pbx_answer`]) is the whole reason this is not a
//! straight-line handler.

use super::call::{spawn_control_reader, ActiveCall, CachedInviteAnswer, DialogInfo, RtcpHandle};
use super::observability;
use super::veth::{spawn_relay, spawn_veth_uas_listener, VETH_INVITE_TIMEOUT};
use super::{unsupported_required_extensions, CONTROL_TIMEOUT};
use crate::amr_forward::relay::{ForwardParams, ForwardSession, Leg, Remote};
use crate::amr_forward::{compatible, AmrLegParams};
use crate::control::protocol::{BridgeFailureReason, CallStatus};
use crate::error::{BridgeError, BridgeResult};
use crate::ims::body;
use crate::ims::lifecycle::{BridgedCall, CallStage};
use crate::ims::sdp::{self, NegotiatedCodec};
use crate::ims::session::{
    caller_identity_is_private, extract_caller, extract_caller_name, respond, Inbound,
};
use crate::ims::sip_client::{
    build_100_trying, build_180_ringing, build_415_unsupported_media, build_420_bad_extension,
    build_488_incompatible_transport, build_488_not_acceptable, build_580_precondition_failure,
    build_uas_response, build_uas_response_with_headers, format_sip_addr, SipMessage, SipRequest,
    SipSink,
};
use crate::vowifi::control::{reason, write_msg, ControlMessage};
use chrono::Utc;
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// How long the PBX extension may ring — with the caller hearing real ringback
/// throughout — before we give up and return `480`. Must stay under the
/// carrier's own no-answer timer so *we* decide the outcome, not the network.
/// `crate::vowifi`'s `PBX_RING_TIMEOUT` is deliberately a little shorter, so
/// Agent B normally reports `BridgeFailed` before this fires.
const RING_TIMEOUT: Duration = Duration::from_secs(50);

/// How often, while ringing, to check the control channel and the carrier's
/// signaling. Bounds how fast a caller's `CANCEL` gets answered.
const RING_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// specs/047-offerless-invite-sms-reassembly (SDP-04): how long, after
/// answering an offerless INVITE with our own offer, to wait for the
/// caller's `ACK` to carry their answer before giving up on the call.
///
/// Deliberately much shorter than [`RING_TIMEOUT`] — that bound is for a
/// *human* decision (whether to pick up the PBX extension); this one is for
/// a protocol-transaction-scale event. RFC 3261 §13.3.1.4 requires the ACK
/// as the very next thing on the wire once the caller's stack processes our
/// `2xx`, and §17.1.1.3's Timer H (32s) is the outer bound anything in this
/// codebase should ever wait for one. This bridge is not itself running
/// that timer (no server-transaction layer exists here — specs/042's own
/// scoping decision), so this is a plain, independent bound serving the
/// same purpose: don't wait forever for something the protocol says arrives
/// almost immediately.
const OFFERLESS_ACK_TIMEOUT: Duration = Duration::from_secs(4);

/// The `Contact` for a response that answers a network-initiated INVITE.
///
/// Must carry the same feature tags we registered with, not just the address.
/// See the call site for the measurement that motivated this.
fn uas_contact(public_user: &str, addr: &str, transport: &str, imei: &str) -> String {
    format!(
        "<sip:{public_user}@{addr};transport={transport}>\
         ;+g.3gpp.icsi-ref=\"urn%3Aurn-7%3A3gpp-service.ims.icsi.mmtel\"\
         ;audio;+sip.instance=\"<urn:gsma:imei:{imei}>\""
    )
}

/// Everything `handle_invite` needs that is fixed for the life of the agent.
pub(super) struct InviteContext<'a> {
    pub(super) control_addr: SocketAddr,
    pub(super) veth_local_ip: IpAddr,
    pub(super) wideband: bool,
    pub(super) answer_preference: sdp::AnswerPreference,
    /// The port on `veth_local_ip` where the telephone-side half's leg is
    /// expected. It MUST match what that half dials — a mismatch produces a
    /// call that rings the PBX, is answered, and then times out with the
    /// caller still hearing ringback (observed live, specs/017 R17).
    pub(super) veth_sip_port: u16,
    pub(super) obs: &'a observability::AgentObservability,
    /// This line's real access-network type, stated in the `200 OK` to an
    /// answered inbound INVITE (specs/045 MT-11) — previously omitted
    /// entirely.
    pub(super) access_network_info: &'a str,
    /// See `config::VowifiConfig::respect_caller_privacy`.
    pub(super) respect_caller_privacy: bool,
    /// See `config::VowifiConfig::amr_passthrough`.
    pub(super) amr_passthrough: bool,
    pub(super) amr_mode_set: crate::amr_forward::ModeSetPolicy,
}

/// The capability headers on a `200 OK` that answers an inbound INVITE.
///
/// `pub(super)`, not private: three sites answer an inbound INVITE (the
/// ordinary path and the offerless-INVITE path here, plus `agent::mod`'s
/// resend of a cached answer to a retransmitted INVITE), and they have
/// already drifted apart once, expensively — `5277765` dropped `Supported`
/// from two of them and every inbound Jio call failed for three weeks
/// (`a38f725`). Routing all three through one function is what keeps that
/// from happening a third time; building the header list by hand at any one
/// site is the mistake this exists to rule out.
///
/// Every header here is carrier-interop boilerplate rather than a promise:
/// `Supported: timer` without session-refresh (RFC 4028 §9 permits it), and
/// [`crate::ims::UAS_INVITE_ALLOW`]'s `UPDATE` without an implementation. See
/// that constant for the measurement that made it necessary.
pub(super) fn uas_answer_headers(access_network_info: &str) -> [(&str, &str); 3] {
    [
        ("Allow", crate::ims::UAS_INVITE_ALLOW),
        ("Supported", "timer, 100rel, replaces, path, gruu"),
        ("P-Access-Network-Info", access_network_info),
    ]
}

/// What an inbound INVITE's `415` says this UAS accepts (RFC 3261 §21.4.13),
/// and what the `OPTIONS` answer advertises (§11.2). RFC 5621 §4.2 makes the
/// two multipart types mandatory for every SIP UA.
pub(crate) const INVITE_BODY_ACCEPT: &str =
    "application/sdp, multipart/mixed, multipart/alternative";

/// What [`handle_invite`] does with an INVITE's body.
#[derive(Debug, PartialEq, Eq)]
enum InviteBody {
    /// No body: RFC 3264 §3's offerless INVITE (specs/047 SDP-04).
    Offerless,
    /// The SDP offer, extracted from whatever MIME wrapping it came in.
    Offer(String),
    /// A body with no usable SDP: the complete `415`/`400` to send.
    Refuse { response: String, reason: String },
}

/// Decides, from the body alone, whether an INVITE is offerless, carries an
/// offer, or must be refused — the whole of [`handle_invite`]'s body
/// handling, kept pure so it is testable without the live control channel
/// and veth listener `handle_invite` needs.
///
/// specs/088 (issue #103): the body may be `multipart/*` — T2 sends its SDP
/// alongside a PIDF-LO location part — so it goes through
/// [`body::extract_sdp`] instead of the old "only `application/sdp`" gate,
/// which declined every such call `415`. A body that has no session
/// description is refused `415` with [`INVITE_BODY_ACCEPT`]; a broken or
/// ambiguous multipart `400`. Neither is ever scanned as if it were SDP
/// (specs/045 SDP-05).
fn invite_body_decision(req: &SipRequest, to_tag: &str) -> InviteBody {
    match body::extract_sdp(req.header("Content-Type"), &req.body_bytes) {
        Ok(body::Decoded::Empty) => InviteBody::Offerless,
        Ok(body::Decoded::Sdp { text, .. }) => InviteBody::Offer(text),
        Err(rejection) => {
            let response = if rejection.status() == 415 {
                build_415_unsupported_media(req, to_tag, INVITE_BODY_ACCEPT)
            } else {
                build_uas_response(400, "Bad Request", req, Some(to_tag), None, None)
            };
            InviteBody::Refuse {
                response,
                reason: rejection.to_string(),
            }
        }
    }
}

/// The SDP answer an offerless call's `ACK` carries (RFC 3264 §3), decoded
/// the same way as an INVITE's body (specs/088) — `None` when the ACK has no
/// body or none this bridge can read as SDP. The ACK itself still arrived;
/// the caller tells that apart from a timeout.
fn ack_answer(ack: &SipRequest) -> Option<String> {
    match body::extract_sdp(ack.header("Content-Type"), &ack.body_bytes) {
        Ok(body::Decoded::Sdp { text, .. }) => Some(text),
        Ok(body::Decoded::Empty) | Err(_) => None,
    }
}

/// The caller's display name, suitable for re-presenting to Agent B/the PBX
/// — unlike `extract_caller_name` alone, this also honours RFC 3325 §9.1
/// `Privacy: id`/`user` on the inbound INVITE (when `respect_caller_privacy`,
/// `config::VowifiConfig::respect_caller_privacy`, says to) by withholding
/// the name (`None`) rather than passing it onward.
fn caller_name_for_onward_signaling(
    req: &SipRequest,
    respect_caller_privacy: bool,
) -> Option<String> {
    if respect_caller_privacy && caller_identity_is_private(req) {
        None
    } else {
        extract_caller_name(req)
    }
}

/// Answers (or declines) one inbound carrier `INVITE`. Returns `Some` with
/// the bookkeeping `handle_bye` will need once the call is actually
/// bridged; `None` if it was declined (no compatible codec, Agent B couldn't
/// bridge it, the PBX declined or never answered) — every decline path sends
/// a fast, explicit final response, never silence or unanswered ringing
/// (FR-009/FR-010); which status is [`decline_response`]'s call.
///
/// `to_tag` is the one tag every response to this INVITE carries — the
/// `180`, the `200` and every decline alike (RFC 3261 §12.1.1). The caller
/// owns it so its own fallback response on `Err` can use it too: a final
/// response on a fresh tag after a `180` names a dialog the carrier has
/// never seen (specs/084 FR-005).
pub(super) fn handle_invite(
    session: &crate::ims::RegisteredSession,
    req: &SipRequest,
    sink: &SipSink,
    inbound: &Inbound,
    ctx: &InviteContext,
    to_tag: &str,
) -> BridgeResult<Option<ActiveCall>> {
    let obs = ctx.obs;
    let call_id = req.header("Call-ID").unwrap_or_default().to_string();
    let caller = extract_caller(req);
    let started_at = Utc::now();
    tracing::info!(
        call_id = %call_id,
        caller = %caller,
        request_uri = %req.request_uri,
        "inbound VoWiFi call"
    );

    // RFC 3261 §8.2.2.3: a `Require` naming an extension we do not implement
    // must refuse the whole request `420` before anything else is attempted
    // — proceeding as if the requirement did not exist is not a conforming
    // option (specs/041 conformance review, MT-03).
    let unsupported = unsupported_required_extensions(req);
    if !unsupported.is_empty() {
        tracing::info!(
            call_id = %call_id,
            unsupported = %unsupported.join(", "),
            "declining: caller requires an extension we do not implement"
        );
        sink.send(&build_420_bad_extension(
            req,
            to_tag,
            &unsupported.join(", "),
        ))?;
        obs.report_call_not_answered(
            CallStatus::Failed,
            BridgeFailureReason::BridgeSetupFailed,
            &caller,
            started_at,
        );
        return Ok(None);
    }

    sink.send(&build_100_trying(req))?;

    // specs/047-offerless-invite-sms-reassembly (SDP-04), research.md
    // Decision 1: an INVITE with no body at all is not a malformed offer —
    // it is a different, legal RFC 3264 §3 case (the answerer states the
    // offer, in the 2xx, and the caller answers it in the ACK). specs/048
    // MT-06, research.md Decision 5: that branch returns before
    // `parse_offer`/`precondition_verdict` ever run, so an offerless INVITE
    // naturally has zero `a=des:qos` lines to act on.
    let offer_sdp = match invite_body_decision(req, to_tag) {
        InviteBody::Offerless => {
            return handle_offerless_invite(
                session, req, sink, inbound, ctx, &call_id, &caller, started_at, to_tag,
            );
        }
        InviteBody::Offer(sdp) => sdp,
        InviteBody::Refuse { response, reason } => {
            tracing::info!(
                call_id = %call_id,
                reason = %reason,
                "declining: INVITE body"
            );
            sink.send(&response)?;
            obs.report_call_not_answered(
                CallStatus::Failed,
                BridgeFailureReason::BridgeSetupFailed,
                &caller,
                started_at,
            );
            return Ok(None);
        }
    };

    let offer = sdp::parse_offer(&offer_sdp)?;

    // The transport this bridge implements is plain `RTP/AVP` — no SRTP, no
    // RTCP feedback profile. An offer naming anything else (`RTP/SAVP`, a
    // garbage token) describes a stream we structurally cannot service;
    // accepting its codec list while ignoring what it said about the
    // transport would answer a protocol the offer never actually proposed.
    // Checked before the codec precheck below: an unusable transport makes
    // codec selection moot (specs/043 SDP-03).
    if offer.proto != "RTP/AVP" {
        tracing::info!(
            call_id = %call_id,
            proto = %offer.proto,
            "offer's m=audio transport profile is not one we implement; declining"
        );
        sink.send(&build_488_incompatible_transport(
            req,
            to_tag,
            &format_sip_addr(session.contact_addr),
        ))?;
        obs.report_call_not_answered(
            CallStatus::Failed,
            BridgeFailureReason::BridgeSetupFailed,
            &caller,
            started_at,
        );
        return Ok(None);
    }

    // specs/048 MT-06: `Require: precondition` no longer blanket-declines
    // on its own (`precondition` is in `SUPPORTED_EXTENSIONS`) — whether
    // this offer's precondition content is honourable depends on what it
    // actually asked for. Checked before the codec precheck below, same
    // reasoning as the transport check above: an unconfirmable `e2e`
    // precondition makes codec selection moot.
    if sdp::precondition_verdict(&offer) == sdp::PreconditionVerdict::Decline {
        tracing::info!(
            call_id = %call_id,
            "offer requires an e2e QoS precondition we cannot honestly confirm; declining"
        );
        sink.send(&build_580_precondition_failure(req, to_tag))?;
        obs.report_call_not_answered(
            CallStatus::Failed,
            BridgeFailureReason::BridgeSetupFailed,
            &caller,
            started_at,
        );
        return Ok(None);
    }

    // specs/088 FR-018a: the caller's own mandatory segment not yet reserved
    // is the carrier's to wait for, not ours — the call proceeds (answering
    // would need an `UPDATE` we don't implement), but say so once.
    let unmet = sdp::offerer_unmet_mandatory(&offer);
    if !unmet.is_empty() {
        tracing::info!(
            call_id = %call_id,
            unmet = ?unmet,
            "offerer reports its own mandatory QoS precondition unmet; proceeding"
        );
    }

    // A carrier's mobile-terminating VoWiFi INVITE often offers no PCMU at
    // all (Airtel: AMR-WB+AMR-NB on some calls, AMR-NB alone on others), so
    // anything AMR gets answered and transcoded rather than declined. Uses
    // `sdp::select_codec` — the same decision `build_answer` makes below, with
    // the same arguments — so we can never accept a call we then can't build an
    // answer for.
    let Some(precheck) = sdp::select_codec_with(
        &offer,
        amr_safe::is_available(),
        ctx.wideband,
        ctx.answer_preference,
    ) else {
        tracing::info!(
            call_id = %call_id,
            amr_linked = amr_safe::is_available(),
            offered = ?offer.offered.iter().map(|c| (c.payload_type, c.codec)).collect::<Vec<_>>(),
            "offer has no codec we can answer with; declining"
        );
        // `488`, not `486`: the line is not busy, no codec in the offer is
        // one we can answer with — a caller redialling immediately on `486`
        // (busy-line semantics) will hit the exact same wall (specs/041
        // conformance review, MT-07).
        sink.send(&build_488_not_acceptable(
            req,
            to_tag,
            &format_sip_addr(session.contact_addr),
        ))?;
        obs.report_call_not_answered(
            CallStatus::Failed,
            BridgeFailureReason::BridgeSetupFailed,
            &caller,
            started_at,
        );
        return Ok(None);
    };

    // specs/120: what the carrier leg negotiated, for Agent B to judge against
    // the client's answer. Only computed when the setting is on, so a
    // deployment that leaves it off sends exactly the message it always did.
    // `build_answer` is pure: this is the same selection the real answer makes
    // below, with a placeholder port.
    let carrier_amr = carrier_amr_for(
        ctx.amr_passthrough,
        ctx.amr_mode_set,
        session.local_addr.ip(),
        &offer,
        ctx.wideband,
        ctx.answer_preference,
    );

    // Only a wideband *carrier* leg has anything for a wideband veth leg to
    // preserve. A narrowband call (PCMU or AMR-NB — the two shapes Airtel
    // sends when the originating leg is narrowband) keeps the veth link on
    // PCMU, exactly the path it took before L16 existed.
    let veth_wideband = precheck.codec == NegotiatedCodec::AmrWb;
    let veth_rx = spawn_veth_uas_listener(ctx.veth_local_ip, ctx.veth_sip_port, veth_wideband)?;

    // Generated once and reused for every response in this dialog (180 and
    // 200 OK alike) — RFC 3261 requires the same To-tag across all
    // responses that establish/confirm one dialog.
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
    // The feature tags matter as much as the address. Jio's inbound INVITE
    // carries RFC 3841 caller preferences that *require* them:
    //
    //   Accept-Contact: *;require;explicit;+sip.instance="<urn:gsma:imei:...>"
    //   Accept-Contact: *;+g.3gpp.icsi-ref="urn%3Aurn-7%3A3gpp-service.ims.icsi.mmtel"
    //
    // `require` means the caller is demanding the answering contact match
    // those tags, and TS 24.229 §5.1.3 has the UE state its IMS communication
    // service in the Contact of a session response regardless. We registered
    // with both (see `sip_client::RegisterRequest`'s Contact) and then answered
    // with a bare `<sip:user@addr;transport=...>`, so the contact that picked
    // the call up advertised neither the MMTEL ICSI nor the instance the caller
    // asked for.
    //
    // Measured on Jio 2026-08-15: five different SDP answer bodies (with and
    // without `telephone-event`, `maxptime`, the `b=` trio, and both AMR-WB
    // framings) were each torn down ~460 ms after the `200 OK` with
    // `BYE Reason:SIP;cause=503;text="IO: SIP SDP Protocol Error."`, while the
    // answer itself was verified byte-perfect on the wire and media negotiated
    // and flowed both ways. The SDP body was never the variable; this omission
    // is invariant across all five.
    let contact = uas_contact(
        &public_user,
        &format_sip_addr(session.contact_addr),
        via_transport,
        &session.imei,
    );
    // Ring the caller. The network turns this into audible ringback and keeps
    // playing it until we answer — which we now deliberately don't do until a
    // human picks up the PBX extension (see `await_pbx_answer`).
    sink.send(&build_180_ringing(req, to_tag, &contact))?;

    let mut control = TcpStream::connect_timeout(&ctx.control_addr, CONTROL_TIMEOUT)
        .map_err(|e| BridgeError::Ims(format!("failed to reach Agent B control channel: {e}")))?;
    write_msg(
        &mut control,
        &ControlMessage::IncomingCall {
            call_id: call_id.clone(),
            caller: caller.clone(),
            caller_name: caller_name_for_onward_signaling(req, ctx.respect_caller_privacy),
            carrier_amr: carrier_amr.clone(),
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

            // specs/046-rtcp-reporting: binds the carrier RTP socket and,
            // where possible, its RTCP companion in one call — RTP+1 by
            // convention (tier 1, the common case, leaving the answer
            // unchanged), an ephemeral declared port (tier 2), or no RTCP
            // at all (tier 3: media still proceeds, per SC-006). See
            // `ims::rtcp::bind_rtp_and_rtcp`'s own doc for the full
            // rationale.
            let crate::ims::rtcp::RtpRtcpBind {
                rtp_socket: ims_rtp_socket,
                rtp_port: ims_rtp_port,
                rtcp: rtcp_endpoint,
            } = crate::ims::rtcp::bind_rtp_and_rtcp(session.local_addr.ip())
                .map_err(|e| BridgeError::Ims(format!("IMS RTP/RTCP bind failed: {e}")))?;
            ims_rtp_socket
                .connect(offer.remote_rtp)
                .map_err(|e| BridgeError::Ims(format!("IMS RTP connect failed: {e}")))?;

            // FR-014/015/016: only the port form of the offer's own
            // `a=rtcp` is honoured, and only when it parsed to something
            // usable — otherwise the RTP+1-derived convention already
            // reflected in `rtcp_endpoint`'s own port stands.
            let declared_rtcp_port =
                rtcp_endpoint.as_ref().and_then(
                    |(_, port, declared)| {
                        if *declared {
                            Some(*port)
                        } else {
                            None
                        }
                    },
                );

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
                ctx.wideband,
                ctx.answer_preference,
                declared_rtcp_port,
            )?;

            // Do NOT answer yet. The PBX extension is only ringing; our
            // `180 Ringing` above is what makes the network play ringback to
            // the caller, and a `200 OK` here would cut that off and leave them
            // in silence until someone picks up. Wait for Agent B to report a
            // real answer — while still watching the carrier's own signaling,
            // since the caller may give up (`CANCEL`) while it rings.
            let mut forward_offer = ForwardOffer {
                carrier: carrier_amr.as_ref(),
                veth_local_ip: ctx.veth_local_ip,
                control: &mut control,
                pending: None,
            };
            let mut held = HeldMessages::new(&inbound.tx);
            let ringing = await_pbx_answer(
                &call_id,
                &ctrl_rx,
                &inbound.rx,
                req,
                to_tag,
                &contact,
                sink,
                Some(&mut forward_offer),
                &mut held,
            );
            // Back on the loop's queue before anything else is done with the
            // outcome, including an error.
            drop(held);
            let ringing = ringing?;
            let forward_hop = forward_offer.pending.take();
            match ringing {
                RingOutcome::Answered => {}
                RingOutcome::PbxDeclined { reason } => {
                    obs.report_call_not_answered(
                        CallStatus::Missed,
                        observability::map_bridge_failure_reason(&reason),
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

            // What a real UE states in a session response: the methods it
            // accepts (RFC 3261 §20.5) and the access network it registered
            // over. `Allow` (`super::ALLOW`) is what made inbound Jio calls
            // work — answering without it got every call torn down ~460ms
            // later with `BYE Reason:SIP;cause=503;text="IO: SIP SDP
            // Protocol Error."`; RFC 3261 §13.3.1.4 requires it in a `2xx`
            // to INVITE regardless. `Supported` was dropped in `5277765`
            // (specs/041, MT-10) on "capability truthfulness" grounds —
            // hardware-verified only against Vi, which doesn't care either
            // way — and that reintroduced the *exact* `cause=503 "IO: SIP
            // SDP Protocol Error"` teardown on the Jio line (live-verified
            // 2026-09-15, four separate calls, always ~0.5-2.5s after this
            // `200 OK`): Jio's IMS core requires it regardless of whether
            // this UAS implements any behaviour behind it. Restored
            // unconditionally — Vi's own prior test showed no downside.
            // `P-Access-Network-Info` is a real per-line value (VoWiFi vs.
            // VoLTE), not a fixed capability claim, so it's built per-call
            // rather than a shared const (specs/045 MT-11).
            let response = build_uas_response_with_headers(
                200,
                "OK",
                req,
                Some(to_tag),
                Some(&contact),
                Some(&answer_sdp),
                &uas_answer_headers(ctx.access_network_info),
            );
            sink.send(&response)?;

            let stop = Arc::new(AtomicBool::new(false));
            // Counts audio each way so the completed call can be judged
            // both-ways or one-way (FR-017) — the same guard the outbound path
            // applies, here on the shared inbound bridge both transports use.
            let meter = crate::ims::media_stats::MediaMeter::new();

            // specs/046-rtcp-reporting: shared send/receive/far-end state
            // for this call's RTCP thread — `None` only on tier 3 (no
            // RTCP endpoint obtained), in which case media still proceeds
            // exactly as before this feature (SC-006).
            let rtcp_bundle = rtcp_endpoint
                .is_some()
                .then(crate::ims::rtcp::CarrierRtcpBundle::new);

            // specs/120: Agent B asked to forward this call's AMR frames and
            // we accepted, so the carrier socket is wired straight to Agent B's
            // forwarder instead of through the transcoder. Everything else
            // below (RTCP toward the carrier, the meters, teardown) is the same
            // either way.
            let forwarding;
            let transcoding;
            if let (Some(hop), Some(carrier)) = (forward_hop, carrier_amr.as_ref()) {
                let leg = |socket: UdpSocket| Leg {
                    socket,
                    remote: Remote::Connected,
                    // Agent B relabels to the client's payload types; this hop
                    // carries the carrier's own.
                    payload_type: chosen.payload_type,
                    dtmf_payload_type: chosen.dtmf_payload_type,
                    octet_aligned: carrier.octet_aligned,
                };
                let session = ForwardSession::start(ForwardParams {
                    kind: carrier.flavour,
                    a: leg(ims_rtp_socket),
                    b: leg(hop),
                    a_rtcp: rtcp_bundle.as_ref().map(|b| b.receiver_role()),
                    b_rtcp: rtcp_bundle.as_ref().map(|b| b.sender_role()),
                    a_rx: meter.carrier_rx_counter(),
                    b_rx: meter.pbx_rx_counter(),
                    stop: stop.clone(),
                    mode_guard: carrier
                        .mode_guard()
                        .map(|(modes, drop)| crate::amr_forward::relay::ModeGuard { modes, drop }),
                })
                .map_err(|e| BridgeError::Ims(format!("AMR forwarder start failed: {e}")))?;
                // Ends with the call's stop flag, like every other relay. The
                // veth call is a quiet dialog now: its RTP socket carries
                // nothing and closes with this scope.
                session.detach();
                forwarding = true;
                transcoding = false;
            } else {
                forwarding = false;
                transcoding = chosen.codec != veth.codec.codec;
                if transcoding {
                    // The two legs speak different codecs (or the same codec
                    // at different rates), so it has to be terminated on each
                    // side and re-encoded.
                    crate::ims::transcode::spawn_transcoding_relay(
                        ims_rtp_socket,
                        veth.rtp_socket,
                        chosen,
                        veth.codec,
                        stop.clone(),
                        &meter,
                        rtcp_bundle.clone(),
                    )?;
                } else {
                    // Both legs speak PCMU: forward the payloads untouched.
                    spawn_relay(
                        ims_rtp_socket,
                        veth.rtp_socket,
                        stop.clone(),
                        &meter,
                        chosen.dtmf_payload_type,
                        veth.codec.dtmf_payload_type,
                        rtcp_bundle.clone(),
                    );
                }
            }

            // The per-call RTCP thread itself (US1/US2/US5): sends
            // periodic reports on the cadence `b=RS:` declares, reads the
            // far end's, and sends a BYE on hangup — all from a thread
            // that owns its own socket, so no teardown call site changes
            // (research.md Decision 2). Honours the offer's own `a=rtcp`
            // when it names one (FR-015), falling back to the RTP+1
            // convention otherwise (FR-016).
            match (rtcp_endpoint, rtcp_bundle.clone()) {
                (Some((rtcp_socket, _port, _declared)), Some(bundle)) => {
                    let remote_rtcp = SocketAddr::new(
                        offer.remote_rtp.ip(),
                        offer
                            .rtcp
                            .unwrap_or_else(|| offer.remote_rtp.port().saturating_add(1)),
                    );
                    crate::ims::rtcp::spawn_report_loop(
                        rtcp_socket,
                        remote_rtcp,
                        offer.remote_rtp.ip(),
                        bundle,
                        chosen.codec.sample_rate(),
                        format!("gsm-sip-bridge-{call_id}"),
                        stop.clone(),
                    );
                }
                _ => {
                    tracing::warn!(
                        call_id = %call_id,
                        "no RTCP endpoint obtained for this call; proceeding without RTCP"
                    );
                    // Reported here, at the moment binding actually failed
                    // (tier 3) — not inferred later from `ActiveCall.rtcp`
                    // being `None` at teardown, which is equally true of an
                    // originated call that was never in RTCP's scope to
                    // begin with (FR-023) and would otherwise inflate this
                    // metric on every successful outbound call.
                    obs.report_rtcp_unavailable();
                }
            }

            // Read at teardown for this call's far-end and own receive
            // quality (specs/046-rtcp-reporting). `None` alongside
            // `rtcp_bundle` above.
            let rtcp_handle = rtcp_bundle.as_ref().map(|b| RtcpHandle {
                far_end: b.far_end.clone(),
                receive: b.receive.clone(),
                clock_rate: chosen.codec.sample_rate(),
            });
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
                forwarding,
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
                dialog: DialogInfo::from_invite(req, to_tag, session),
                call_id,
                answered_invite: Some(CachedInviteAnswer {
                    invite_cseq: req.header("CSeq").unwrap_or_default().to_string(),
                    contact,
                    answer_sdp,
                }),
                to_tag: to_tag.to_string(),
                caller,
                answered_at: Utc::now(),
                answered_instant: Instant::now(),
                meter,
                lifecycle,
                rtcp: rtcp_handle,
                // Inbound-answered calls are out of scope for RFC 4028
                // session-timer refresh (specs/049, FR-011) — this bridge
                // never advertises `Supported: timer` on its own `200 OK`
                // (`agent::mod::SUPPORTED_EXTENSIONS`), so no such call
                // ever has a refresh obligation.
                session_refresh: None,
            }))
        }
        ControlMessage::BridgeFailed {
            reason: fail_reason,
            ..
        } => {
            tracing::info!(call_id = %call_id, reason = %fail_reason, "Agent B could not bridge the call, declining");
            sink.send(&decline_response(req, to_tag, &fail_reason))?;
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

/// Answers an inbound INVITE that carried no media description at all
/// (specs/047-offerless-invite-sms-reassembly, SDP-04) — RFC 3261 §14.2/RFC
/// 3264 §3: the answerer (this bridge) states its own offer in the `2xx`,
/// and the caller's device answers it in the `ACK`.
///
/// Deliberately a separate function rather than woven into [`handle_invite`]
/// as more branches: the two paths diverge at exactly one point (what goes
/// in the `200 OK`, and what has to happen before the media path can start)
/// but share every other mechanism — ringing, the Agent B control exchange,
/// `await_pbx_answer`, and the relay/`ActiveCall` construction at the end —
/// by calling the same functions `handle_invite` already calls, not by
/// duplicating what they do.
///
/// **Scope note (FR-002a)**: the offer this sends carries audio codecs
/// only — no `telephone-event` (DTMF) and no RTCP, because
/// [`sdp::build_offer`] (reused as-is from the origination path,
/// research.md Decision 2) has never offered either. A deliberate,
/// recorded residue, not an oversight.
///
/// Eight parameters: `#[allow]` rather than a bundling struct, since this
/// is a single-call-site internal helper — `handle_invite`'s own single
/// branch — not a shared/growing surface the way the relay functions
/// `#[allow(clippy::too_many_arguments)]` elsewhere in this codebase are
/// (plan.md's own Constitution Check called out exactly this trade-off).
#[allow(clippy::too_many_arguments)]
fn handle_offerless_invite(
    session: &crate::ims::RegisteredSession,
    req: &SipRequest,
    sink: &SipSink,
    inbound: &Inbound,
    ctx: &InviteContext,
    call_id: &str,
    caller: &str,
    started_at: chrono::DateTime<Utc>,
    to_tag: &str,
) -> BridgeResult<Option<ActiveCall>> {
    let obs = ctx.obs;
    tracing::info!(call_id = %call_id, caller = %caller, "inbound INVITE carries no offer; will answer with our own");

    // `veth_wideband` normally comes from the offer's own codec precheck —
    // there is no offer yet here, so it falls back to this line's own
    // general preference instead (research.md Decision 5), the same signal
    // `sdp::CodecOffer::preferring_wideband` below uses for what we're about
    // to offer the carrier.
    let veth_wideband = ctx.wideband;
    let veth_rx = spawn_veth_uas_listener(ctx.veth_local_ip, ctx.veth_sip_port, veth_wideband)?;

    let public_user = session
        .public_uri
        .split('@')
        .next()
        .unwrap_or(&session.public_uri)
        .to_string();
    let via_transport = if session.use_tcp { "TCP" } else { "UDP" };
    let contact = uas_contact(
        &public_user,
        &format_sip_addr(session.contact_addr),
        via_transport,
        &session.imei,
    );
    sink.send(&build_180_ringing(req, to_tag, &contact))?;

    let mut control = TcpStream::connect_timeout(&ctx.control_addr, CONTROL_TIMEOUT)
        .map_err(|e| BridgeError::Ims(format!("failed to reach Agent B control channel: {e}")))?;
    write_msg(
        &mut control,
        &ControlMessage::IncomingCall {
            call_id: call_id.to_string(),
            caller: caller.to_string(),
            caller_name: caller_name_for_onward_signaling(req, ctx.respect_caller_privacy),
            carrier_amr: None,
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

    let ControlMessage::BridgeReady { .. } = reply else {
        // `BridgeFailed` or anything unexpected: identical handling to
        // `handle_invite`'s own match on this same reply.
        return match reply {
            ControlMessage::BridgeFailed {
                reason: fail_reason,
                ..
            } => {
                tracing::info!(call_id = %call_id, reason = %fail_reason, "Agent B could not bridge the call, declining");
                sink.send(&decline_response(req, to_tag, &fail_reason))?;
                obs.report_call_not_answered(
                    CallStatus::Failed,
                    observability::map_bridge_failure_reason(&fail_reason),
                    caller,
                    started_at,
                );
                Ok(None)
            }
            other => Err(BridgeError::Ims(format!(
                "unexpected control-channel reply to IncomingCall: {other:?}"
            ))),
        };
    };

    let veth = veth_rx
        .recv_timeout(VETH_INVITE_TIMEOUT)
        .map_err(|_| BridgeError::Ims("timed out waiting for Agent B's veth call".into()))??;

    // No far-end offer to bind against yet — a plain RTP port is enough.
    // No RTCP endpoint is bound on this path (FR-002a): there is nothing to
    // declare it against yet, and RTCP itself is out of scope here.
    let ims_rtp_socket = std::net::UdpSocket::bind((session.local_addr.ip(), 0))
        .map_err(|e| BridgeError::Ims(format!("IMS RTP bind failed: {e}")))?;
    let ims_rtp_port = ims_rtp_socket
        .local_addr()
        .map_err(|e| BridgeError::Ims(format!("IMS RTP local_addr failed: {e}")))?
        .port();

    let session_id: u64 = rand::random::<u32>() as u64;
    let our_offer = sdp::build_offer(
        session.local_addr.ip(),
        ims_rtp_port,
        session_id,
        sdp::CodecOffer::preferring_wideband(ctx.wideband && amr_safe::is_available()),
    );

    let mut held = HeldMessages::new(&inbound.tx);
    let ring_outcome = await_pbx_answer(
        call_id,
        &ctrl_rx,
        &inbound.rx,
        req,
        to_tag,
        &contact,
        sink,
        None,
        &mut held,
    );
    drop(held);
    match ring_outcome? {
        RingOutcome::Answered => {}
        RingOutcome::PbxDeclined { reason } => {
            obs.report_call_not_answered(
                CallStatus::Missed,
                observability::map_bridge_failure_reason(&reason),
                caller,
                started_at,
            );
            return Ok(None);
        }
        RingOutcome::Abandoned { reason } => {
            let _ = write_msg(
                &mut control,
                &ControlMessage::CallEnded {
                    call_id: call_id.to_string(),
                    reason: reason.to_string(),
                },
            );
            obs.report_call_not_answered(
                CallStatus::Missed,
                observability::map_bridge_failure_reason(reason),
                caller,
                started_at,
            );
            return Ok(None);
        }
    }

    let response = build_uas_response_with_headers(
        200,
        "OK",
        req,
        Some(to_tag),
        Some(&contact),
        Some(&our_offer),
        &uas_answer_headers(ctx.access_network_info),
    );
    sink.send(&response)?;

    // specs/047-offerless-invite-sms-reassembly, research.md Decision 3:
    // the same drain-loop shape `await_pbx_answer` already uses, watching
    // for the one thing this wait cares about — the caller's `ACK` naming
    // this exact dialog (RFC 3261 §12.2.2: Call-ID plus both tags —
    // `names_active_dialog`, reused from `agent::mod` unchanged) — and
    // ignoring everything else, same posture `await_pbx_answer` already
    // has for an irrelevant method.
    //
    // A `BYE` naming this dialog is *not* one of the things safely ignored,
    // unlike an unrelated method: RFC 3261 §12.1.1 confirms this dialog the
    // moment we sent the `2xx` above, before the caller's own `ACK` — so a
    // caller that answers and immediately hangs up (or never actually
    // intended to complete the offer/answer) can legitimately `BYE` it
    // before its `ACK` ever arrives. Left unanswered, that `BYE` is simply
    // retransmitted by the network until this wait times out and this
    // bridge sends its *own*, redundant `BYE` for a dialog already ended
    // from the other side (code review finding, 2026-08-28).
    let mut held = HeldMessages::new(&inbound.tx);
    let deadline = Instant::now() + OFFERLESS_ACK_TIMEOUT;
    // Outer `Some`: the ACK arrived. Inner: the SDP answer it carried, if
    // its body had a usable one (`ack_answer`).
    let mut ack: Option<Option<String>> = None;
    let mut caller_bye = false;
    'wait_for_ack: while Instant::now() < deadline {
        while let Ok((msg, req_sink)) = inbound.rx.try_recv() {
            let Some((candidate, req_sink)) = held.take_response(msg, req_sink) else {
                continue;
            };
            let this_dialog = super::names_active_dialog(
                &candidate,
                call_id,
                to_tag,
                req.header("From").unwrap_or_default(),
            );
            if candidate.method == "ACK" && this_dialog {
                ack = Some(ack_answer(&candidate));
                break 'wait_for_ack;
            }
            if candidate.method == "BYE" && this_dialog {
                tracing::info!(call_id = %call_id, "caller ended the offerless call before its ACK arrived");
                respond(
                    &req_sink,
                    "200 OK (BYE, offerless pre-ACK)",
                    &build_uas_response(200, "OK", &candidate, Some(to_tag), None, None),
                );
                caller_bye = true;
                break 'wait_for_ack;
            }
            tracing::debug!(method = %candidate.method, "ignoring inbound request while awaiting the offerless call's ACK");
        }
        std::thread::sleep(RING_POLL_INTERVAL);
    }

    drop(held);
    if caller_bye {
        // The far end already ended this dialog — echoing our own `BYE`
        // back at it would be answering a call that no longer exists from
        // its perspective. Just tell Agent B and report the outcome.
        let _ = write_msg(
            &mut control,
            &ControlMessage::CallEnded {
                call_id: call_id.to_string(),
                reason: reason::CALLER_CANCELLED.to_string(),
            },
        );
        obs.report_call_not_answered(
            CallStatus::Missed,
            observability::map_bridge_failure_reason(reason::CALLER_CANCELLED),
            caller,
            started_at,
        );
        return Ok(None);
    }

    let teardown = |reason: &str, control: &mut TcpStream| {
        let bye = DialogInfo::from_invite(req, to_tag, session).build_bye_for(call_id);
        let _ = sink.send(&bye);
        let _ = write_msg(
            control,
            &ControlMessage::CallEnded {
                call_id: call_id.to_string(),
                reason: reason.to_string(),
            },
        );
        tracing::info!(call_id = %call_id, reason, "offerless call answered but not completed; tore it down");
    };

    let Some(ack_sdp) = ack else {
        // research.md Decision 4: no automatic 2xx retransmission exists in
        // this codebase's transaction-lite design, so a `BYE` — not a
        // second final response — is the honest way to end a dialog whose
        // ACK never arrived (FR-006/SC-002).
        teardown("offerless_ack_timeout", &mut control);
        obs.report_call_not_answered(
            CallStatus::Failed,
            BridgeFailureReason::BridgeSetupFailed,
            caller,
            started_at,
        );
        return Ok(None);
    };

    let answer = ack_sdp
        .ok_or_else(|| BridgeError::Ims("offerless ACK carried no usable SDP answer".into()))
        .and_then(|text| sdp::parse_answer(&text));
    let chosen = answer
        .as_ref()
        .ok()
        .and_then(|a| sdp::offered_chosen_codec(a.codec));
    let (Ok(answer), Some(chosen)) = (answer, chosen) else {
        // FR-005/SC-002: the caller's device answered with something this
        // bridge cannot service — end the call explicitly, never silently.
        teardown("offerless_ack_incompatible", &mut control);
        obs.report_call_not_answered(
            CallStatus::Failed,
            BridgeFailureReason::BridgeSetupFailed,
            caller,
            started_at,
        );
        return Ok(None);
    };

    if let Err(e) = ims_rtp_socket.connect(answer.remote_rtp) {
        tracing::warn!(call_id = %call_id, error = %e, "offerless call: IMS RTP connect failed");
        teardown("offerless_rtp_connect_failed", &mut control);
        obs.report_call_not_answered(
            CallStatus::Failed,
            BridgeFailureReason::BridgeSetupFailed,
            caller,
            started_at,
        );
        return Ok(None);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let meter = crate::ims::media_stats::MediaMeter::new();

    let transcoding = chosen.codec != veth.codec.codec;
    if transcoding {
        crate::ims::transcode::spawn_transcoding_relay(
            ims_rtp_socket,
            veth.rtp_socket,
            chosen,
            veth.codec,
            stop.clone(),
            &meter,
            None,
        )?;
    } else {
        spawn_relay(
            ims_rtp_socket,
            veth.rtp_socket,
            stop.clone(),
            &meter,
            chosen.dtmf_payload_type,
            veth.codec.dtmf_payload_type,
            None,
        );
    }

    tracing::info!(
        call_id = %call_id,
        carrier_codec = chosen.codec.name(),
        carrier_sample_rate = chosen.codec.sample_rate(),
        carrier_payload_type = chosen.payload_type,
        carrier_octet_aligned = chosen.octet_aligned,
        veth_codec = veth.codec.codec.name(),
        veth_sample_rate = veth.codec.codec.sample_rate(),
        transcoding,
        offerless = true,
        "call answered and bridged"
    );

    let mut lifecycle = BridgedCall::new(call_id.to_string(), caller.to_string(), None);
    lifecycle.advance_to(CallStage::Answering);
    lifecycle.advance_to(CallStage::PbxRinging);
    lifecycle.advance_to(CallStage::Bridged);

    Ok(Some(ActiveCall {
        control,
        ctrl_rx,
        stop,
        dialog: DialogInfo::from_invite(req, to_tag, session),
        call_id: call_id.to_string(),
        answered_invite: Some(CachedInviteAnswer {
            invite_cseq: req.header("CSeq").unwrap_or_default().to_string(),
            contact,
            answer_sdp: our_offer,
        }),
        to_tag: to_tag.to_string(),
        caller: caller.to_string(),
        answered_at: Utc::now(),
        answered_instant: Instant::now(),
        meter,
        lifecycle,
        // FR-002a: no RTCP on the offerless path.
        rtcp: None,
        // See the matching comment at this file's other `ActiveCall`
        // construction site — inbound-answered calls are out of scope for
        // specs/049 (FR-011).
        session_refresh: None,
    }))
}

/// The final response that declines `invite` because Agent B reported
/// `BridgeFailed { reason }` — at setup, or while the PBX rang.
///
/// Only a telephone-side decline (`reason::PBX_REJECTED`) is `486 Busy Here`
/// (RFC 3261 §13.3.1.3); every other reason, including one this build has
/// never heard of, is `480 Temporarily Unavailable`. Issue #108: T2 treats
/// `480` as "unavailable, try elsewhere" (§21.4.18) and re-offered a call the
/// user had just rejected, ringing the phone again ~11s later; and the
/// opposite mistake would report a dead PBX to the caller as a busy line.
///
/// `to_tag` must be the tag our `180 Ringing` already established, so the
/// decline lands on the early dialog the carrier is holding.
fn decline_response(invite: &SipRequest, to_tag: &str, reason: &str) -> String {
    let (status, phrase) = if reason == reason::PBX_REJECTED {
        (486, "Busy Here")
    } else {
        (480, "Temporarily Unavailable")
    };
    build_uas_response(status, phrase, invite, Some(to_tag), None, None)
}

/// What the carrier leg negotiated, for `IncomingCall.carrier_amr` — `None`
/// when passthrough is off, the chosen codec is not AMR, or its parameters are
/// not ones forwarding understands. `build_answer` is pure: this is the same
/// selection the real answer makes later, with a placeholder port.
fn carrier_amr_for(
    enabled: bool,
    policy: crate::amr_forward::ModeSetPolicy,
    local_ip: IpAddr,
    offer: &sdp::SdpOffer,
    wideband: bool,
    preference: sdp::AnswerPreference,
) -> Option<AmrLegParams> {
    if !enabled {
        return None;
    }
    let (_, chosen) = sdp::build_answer(
        local_ip,
        0,
        0,
        offer,
        amr_safe::is_available(),
        wideband,
        preference,
        None,
    )
    .ok()?;
    offer
        .amr_leg_params(&chosen)
        .map(|p| p.with_mode_set_policy(policy))
}

/// Agent B's offer to forward the call's AMR frames instead of transcoding
/// (specs/120). Lives for the ring wait: `ForwardMedia` and `ForwardCancel`
/// arrive on the control channel between `BridgeReady` and `CallAnswered`.
pub(super) struct ForwardOffer<'a> {
    /// What the carrier leg negotiated; `None` when the setting is off or the
    /// codec is not AMR, in which case every `ForwardMedia` is declined.
    pub(super) carrier: Option<&'a AmrLegParams>,
    pub(super) veth_local_ip: IpAddr,
    pub(super) control: &'a mut TcpStream,
    /// The socket for the hop to Agent B, bound and connected once a
    /// `ForwardMedia` was accepted; used instead of the transcoding relay if
    /// `CallAnswered` follows.
    pub(super) pending: Option<UdpSocket>,
}

impl ForwardOffer<'_> {
    fn on_forward_media(&mut self, call_id: &str, rtp_port: u16, client: &AmrLegParams) {
        let verdict = match self.carrier {
            None => Err("carrier_not_amr".to_string()),
            Some(carrier) => compatible(carrier, client)
                .map_err(|r| r.as_str().to_string())
                .and_then(|()| {
                    self.bind_hop(rtp_port)
                        .map_err(|e| format!("bind_failed: {e}"))
                }),
        };
        let reply = match verdict {
            Ok((socket, port)) => {
                self.pending = Some(socket);
                tracing::info!(call_id = %call_id, hop_port = port, "accepted Agent B's forward offer");
                ControlMessage::ForwardAck {
                    call_id: call_id.to_string(),
                    ok: true,
                    reason: None,
                    rtp_port: Some(port),
                }
            }
            Err(reason) => {
                self.pending = None;
                tracing::info!(call_id = %call_id, reason = %reason, "declined Agent B's forward offer; the call stays transcoded");
                ControlMessage::ForwardAck {
                    call_id: call_id.to_string(),
                    ok: false,
                    reason: Some(reason),
                    rtp_port: None,
                }
            }
        };
        if let Err(e) = write_msg(self.control, &reply) {
            tracing::warn!(call_id = %call_id, error = %e, "could not send ForwardAck");
            self.pending = None;
        }
    }

    /// Binds Agent A's end of the hop on the veth address and connects it to
    /// Agent B's.
    fn bind_hop(&self, rtp_port: u16) -> std::io::Result<(UdpSocket, u16)> {
        let socket = UdpSocket::bind((self.veth_local_ip, 0))?;
        let peer = self.control.peer_addr()?.ip();
        socket.connect((peer, rtp_port))?;
        let port = socket.local_addr()?.port();
        Ok((socket, port))
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
    /// its own legs down. Nothing more to tell it. `reason` is Agent B's —
    /// declined, no answer, or failed — for the metric.
    PbxDeclined { reason: String },
    /// We stopped ringing while Agent B still thinks the call is alive — the
    /// caller hung up, or we hit our own ring timeout. Agent B must be told to
    /// stop ringing the extension.
    Abandoned { reason: &'static str },
}

/// Responses that arrive while the dispatch loop is parked in a ring or ACK
/// wait. The wait has no use for them, but the loop does: the answer to a
/// registration refresh or a SUBSCRIBE would otherwise be read and thrown
/// away, and the transaction would later time out as if it had never been
/// answered. They go back on the loop's queue when the wait is over, in the
/// order they arrived, and the loop handles them before it judges any timer
/// ([`super::LoopState::next_message`]).
pub(super) struct HeldMessages<'a> {
    tx: &'a mpsc::Sender<(SipMessage, SipSink)>,
    items: Vec<(SipMessage, SipSink)>,
}

impl<'a> HeldMessages<'a> {
    pub(super) fn new(tx: &'a mpsc::Sender<(SipMessage, SipSink)>) -> Self {
        Self {
            tx,
            items: Vec::new(),
        }
    }

    /// Takes `msg` if it is not a request, handing a request back for the
    /// wait to deal with.
    fn take_response(&mut self, msg: SipMessage, sink: SipSink) -> Option<(SipRequest, SipSink)> {
        match msg {
            SipMessage::Request(req) => Some((req, sink)),
            response => {
                self.items.push((response, sink));
                None
            }
        }
    }
}

impl Drop for HeldMessages<'_> {
    fn drop(&mut self) {
        for item in self.items.drain(..) {
            let _ = self.tx.send(item);
        }
    }
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
#[allow(clippy::too_many_arguments)]
fn await_pbx_answer(
    call_id: &str,
    ctrl_rx: &mpsc::Receiver<ControlMessage>,
    sip_rx: &mpsc::Receiver<(SipMessage, SipSink)>,
    invite: &SipRequest,
    to_tag: &str,
    contact: &str,
    sink: &SipSink,
    mut forward: Option<&mut ForwardOffer<'_>>,
    held: &mut HeldMessages<'_>,
) -> BridgeResult<RingOutcome> {
    let decline = |status: u16, reason: &str| {
        respond(
            sink,
            reason,
            &build_uas_response(status, reason, invite, Some(to_tag), None, None),
        );
    };

    let deadline = Instant::now() + RING_TIMEOUT;
    while Instant::now() < deadline {
        // 1. Did the caller give up while it rang? A CANCEL must be answered
        //    promptly or the network keeps retransmitting it.
        while let Ok((msg, req_sink)) = sip_rx.try_recv() {
            let Some((req, req_sink)) = held.take_response(msg, req_sink) else {
                continue;
            };
            if req.method == "CANCEL" && req.header("Call-ID") == Some(call_id) {
                tracing::info!(call_id = %call_id, "caller hung up while the PBX was still ringing");
                // RFC 3261 §9.2: 200 OK to the CANCEL, 487 to the INVITE it
                // cancels. The CANCEL is its own transaction, so it is answered
                // on the connection it arrived on.
                respond(
                    &req_sink,
                    "200 OK (CANCEL)",
                    &build_uas_response(200, "OK", &req, Some(to_tag), None, None),
                );
                decline(487, "Request Terminated");
                return Ok(RingOutcome::Abandoned {
                    reason: reason::CALLER_CANCELLED,
                });
            }
            if req.method == "INVITE"
                && req.header("Call-ID") == Some(call_id)
                && req.header("CSeq") == invite.header("CSeq")
            {
                // RFC 3261 §17.2.1: while the server transaction is in the
                // Proceeding state, a duplicate request gets the last
                // provisional response resent, not silence
                // (specs/042-dialog-transaction-identity, MT-01). The CSeq
                // check (not just Call-ID) is what makes this a
                // retransmission of *this* INVITE rather than some other
                // transaction on the same dialog — RFC 3261 §12.2.2
                // guarantees a real retransmission carries an identical
                // CSeq, so anything else falls through unhandled below
                // rather than being answered as if it were one (PR review,
                // 2026-08-26).
                tracing::info!(call_id = %call_id, "retransmitted INVITE while ringing; resending 180 Ringing");
                respond(
                    &req_sink,
                    "180 Ringing (retransmit)",
                    &build_180_ringing(&req, to_tag, contact),
                );
                continue;
            }
            tracing::debug!(method = %req.method, "ignoring inbound request received while ringing");
        }

        // 2. Did Agent B report an answer, or give up on the PBX?
        match ctrl_rx.recv_timeout(RING_POLL_INTERVAL) {
            Ok(ControlMessage::CallAnswered { .. }) => return Ok(RingOutcome::Answered),
            Ok(ControlMessage::BridgeFailed { reason, .. }) => {
                tracing::info!(call_id = %call_id, reason = %reason, "PBX leg did not answer; declining");
                respond(sink, &reason, &decline_response(invite, to_tag, &reason));
                return Ok(RingOutcome::PbxDeclined { reason });
            }
            Ok(ControlMessage::ForwardMedia {
                rtp_port, client, ..
            }) => match forward.as_deref_mut() {
                Some(offer) => offer.on_forward_media(call_id, rtp_port, &client),
                None => {
                    tracing::warn!(call_id = %call_id, "ForwardMedia on a path that cannot forward; ignoring")
                }
            },
            Ok(ControlMessage::ForwardCancel { .. }) => {
                if let Some(offer) = forward.as_deref_mut() {
                    tracing::info!(call_id = %call_id, "Agent B cancelled the forward; the call stays transcoded");
                    offer.pending = None;
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn invite_with_headers(headers: &str) -> SipRequest {
        let raw = format!(
            "INVITE sip:me@10.0.0.9:5060 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.1.1.1:5067;branch=z9hG4bKone\r\n\
             {headers}Call-ID: c1\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
        );
        SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
    }

    /// The decline Agent A answers for each `BridgeFailed` reason (specs/084
    /// FR-002..FR-004): only a telephone-side decline is busy; everything
    /// else — no answer, a broken PBX, a setup failure, or a reason this build
    /// has never heard of — is `480`. Always on the early dialog's tag.
    #[test]
    fn decline_response_maps_reasons() {
        let req = invite_with_headers(
            "From: <sip:+919000000000@ims.example>;tag=abc\r\nTo: <sip:me@ims.example>\r\n",
        );
        let busy = decline_response(&req, "tagR", reason::PBX_REJECTED);
        assert!(busy.starts_with("SIP/2.0 486 Busy Here\r\n"), "{busy}");
        assert!(busy.contains(";tag=tagR"), "{busy}");
        for other in [
            reason::PBX_NO_ANSWER,
            reason::PBX_FAILED,
            reason::PBX_UNREACHABLE,
            "some_future_reason",
        ] {
            let resp = decline_response(&req, "tagR", other);
            assert!(
                resp.starts_with("SIP/2.0 480 Temporarily Unavailable\r\n"),
                "{other}: {resp}"
            );
            assert!(resp.contains(";tag=tagR"), "{other}: {resp}");
        }
    }

    /// Issue #108 end to end through the ringing wait: Agent B's verdict
    /// arrives on the control channel, and what actually goes out on a real
    /// UDP socket to the "carrier" is the matching final response, on the
    /// `180`'s tag — `486` for a human reject, `480` otherwise.
    #[test]
    fn await_pbx_answer_declines_with_the_status_agent_b_reported() {
        use crate::ims::sip_client::SipTransport;
        use std::net::UdpSocket;

        for (reason, status_line) in [
            (reason::PBX_REJECTED, "SIP/2.0 486 Busy Here\r\n"),
            (
                reason::PBX_NO_ANSWER,
                "SIP/2.0 480 Temporarily Unavailable\r\n",
            ),
            (
                reason::PBX_FAILED,
                "SIP/2.0 480 Temporarily Unavailable\r\n",
            ),
        ] {
            let carrier = UdpSocket::bind("127.0.0.1:0").unwrap();
            carrier
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let sink = SipTransport::connect(carrier.local_addr().unwrap(), false)
                .unwrap()
                .sink()
                .unwrap();
            let (sip_tx, sip_rx) = mpsc::channel::<(SipMessage, SipSink)>();
            let (ctrl_tx, ctrl_rx) = mpsc::channel::<ControlMessage>();
            ctrl_tx
                .send(ControlMessage::BridgeFailed {
                    call_id: "c1".into(),
                    reason: reason.into(),
                })
                .unwrap();
            let req = invite_with_headers(
                "From: <sip:+919000000000@ims.example>;tag=abc\r\nTo: <sip:me@ims.example>\r\n",
            );

            let mut held = HeldMessages::new(&sip_tx);
            let outcome = await_pbx_answer(
                "c1",
                &ctrl_rx,
                &sip_rx,
                &req,
                "tag180",
                "<sip:me@10.0.0.9>",
                &sink,
                None,
                &mut held,
            )
            .unwrap();

            match outcome {
                RingOutcome::PbxDeclined { reason: got } => assert_eq!(got, reason),
                _ => panic!("{reason}: expected PbxDeclined"),
            }
            let mut buf = [0u8; 4096];
            let n = carrier.recv(&mut buf).unwrap();
            let sent = String::from_utf8_lossy(&buf[..n]);
            assert!(sent.starts_with(status_line), "{reason}: {sent}");
            assert!(sent.contains(";tag=tag180"), "{reason}: {sent}");
        }
    }

    #[test]
    fn a_response_that_arrives_while_the_call_rings_is_held_not_thrown_away() {
        // The answer to a registration refresh can land during the ring wait.
        // It must still be there for the dispatch loop afterwards.
        let carrier = UdpSocket::bind("127.0.0.1:0").unwrap();
        let sink =
            crate::ims::sip_client::SipTransport::connect(carrier.local_addr().unwrap(), false)
                .unwrap()
                .sink()
                .unwrap();
        let (sip_tx, sip_rx) = mpsc::channel::<(SipMessage, SipSink)>();
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<ControlMessage>();
        let response = SipMessage::Response(crate::ims::sip_client::SipResponse {
            status: 200,
            reason: "OK".into(),
            headers: vec![("CSeq".into(), "7 REGISTER".into())],
            body: String::new(),
        });
        sip_tx.send((response, sink.clone())).unwrap();
        ctrl_tx
            .send(ControlMessage::CallAnswered {
                call_id: "c1".into(),
            })
            .unwrap();
        let req = invite_with_headers("From: <sip:+919000000000@ims.example>;tag=abc\r\n");

        let mut held = HeldMessages::new(&sip_tx);
        let outcome = await_pbx_answer(
            "c1",
            &ctrl_rx,
            &sip_rx,
            &req,
            "tag",
            "<sip:me@10.0.0.9>",
            &sink,
            None,
            &mut held,
        )
        .unwrap();
        assert!(matches!(outcome, RingOutcome::Answered));
        assert!(
            sip_rx.try_recv().is_err(),
            "nothing is re-queued while the wait runs"
        );

        drop(held);
        let (back, _) = sip_rx
            .try_recv()
            .expect("the response is back on the queue");
        match back {
            SipMessage::Response(r) => assert_eq!(r.header("CSeq"), Some("7 REGISTER")),
            _ => panic!("expected the response"),
        }
    }

    #[test]
    fn a_request_is_not_held_it_is_the_waits_to_handle() {
        let carrier = UdpSocket::bind("127.0.0.1:0").unwrap();
        let sink =
            crate::ims::sip_client::SipTransport::connect(carrier.local_addr().unwrap(), false)
                .unwrap()
                .sink()
                .unwrap();
        let (tx, rx) = mpsc::channel::<(SipMessage, SipSink)>();
        let mut held = HeldMessages::new(&tx);
        let req = invite_with_headers("From: <sip:+919000000000@ims.example>;tag=abc\r\n");
        assert!(held.take_response(SipMessage::Request(req), sink).is_some());
        drop(held);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn onward_signaling_withholds_the_name_when_privacy_id_is_present_and_respected() {
        let req = invite_with_headers(
            "From: \"Firstname Lastname\" <sip:+919000000000@ims.example>;tag=abc\r\n\
             Privacy: id\r\n",
        );
        assert_eq!(caller_name_for_onward_signaling(&req, true), None);
    }

    /// The new `[vowifi].respect_caller_privacy` escape hatch — off means a
    /// carrier's `Privacy` request no longer withholds the name from the
    /// onward leg (the number was never withheld either way).
    #[test]
    fn onward_signaling_forwards_the_name_despite_privacy_id_when_not_respected() {
        let req = invite_with_headers(
            "From: \"Firstname Lastname\" <sip:+919000000000@ims.example>;tag=abc\r\n\
             Privacy: id\r\n",
        );
        assert_eq!(
            caller_name_for_onward_signaling(&req, false),
            Some("Firstname Lastname".to_string())
        );
    }

    #[test]
    fn onward_signaling_forwards_the_name_when_no_privacy_header_is_present() {
        let req = invite_with_headers(
            "From: \"Firstname Lastname\" <sip:+919000000000@ims.example>;tag=abc\r\n",
        );
        assert_eq!(
            caller_name_for_onward_signaling(&req, true),
            Some("Firstname Lastname".to_string())
        );
    }

    /// FR-012: a `tel:` P-Asserted-Identity is now the source of the name, and
    /// `Privacy: id` still withholds it from the onward leg (RFC 3325 §9.1).
    #[test]
    fn onward_signaling_withholds_a_tel_asserted_name_under_privacy_id() {
        let req = invite_with_headers(
            "From: \"Other Name\" <sip:+919000000001@ims.example>;tag=abc\r\n\
             P-Asserted-Identity: \"Firstname Lastname\" <tel:+919000000000>\r\n\
             Privacy: id\r\n",
        );
        assert_eq!(caller_name_for_onward_signaling(&req, true), None);
        assert_eq!(
            caller_name_for_onward_signaling(&req, false),
            Some("Firstname Lastname".to_string())
        );
    }

    fn invite_with_content_type(content_type: Option<&str>) -> SipRequest {
        let ct_line = content_type
            .map(|ct| format!("Content-Type: {ct}\r\n"))
            .unwrap_or_default();
        let raw = format!(
            "INVITE sip:me@10.0.0.9:5060 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.1.1.1:5067;branch=z9hG4bKone\r\n\
             From: <sip:caller@example.net>;tag=abc\r\n\
             To: <sip:me@example.net>\r\n\
             Call-ID: c1\r\nCSeq: 1 INVITE\r\n{ct_line}Content-Length: 0\r\n\r\n"
        );
        SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
    }

    /// An INVITE with `content_type` (if any) and `body` verbatim.
    fn invite_with(content_type: Option<&str>, body: &str) -> SipRequest {
        let ct_line = content_type
            .map(|ct| format!("Content-Type: {ct}\r\n"))
            .unwrap_or_default();
        let raw = format!(
            "INVITE sip:me@10.0.0.9:5060 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.1.1.1:5067;branch=z9hG4bKone\r\n\
             From: <sip:caller@example.net>;tag=abc\r\n\
             To: <sip:me@example.net>\r\n\
             Call-ID: c1\r\nCSeq: 1 INVITE\r\n{ct_line}Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
    }

    const BARE_SDP: &str =
        "v=0\r\nc=IN IP4 192.0.2.5\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

    /// The refusal's full response text, or a panic naming what came back.
    fn refusal(decision: InviteBody) -> String {
        match decision {
            InviteBody::Refuse { response, .. } => response,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// specs/088 (#103): T2's `multipart/mixed` INVITE — PIDF-LO first, SDP
    /// second — is an offer of exactly the SDP part, not a `415`.
    #[test]
    fn a_t2_multipart_invite_is_an_offer_of_its_sdp_part() {
        use crate::ims::body::fixtures::{t2_body, T2_CONTENT_TYPE, T2_SDP};
        let req = invite_with(Some(T2_CONTENT_TYPE), &t2_body());
        assert_eq!(
            invite_body_decision(&req, "tt"),
            InviteBody::Offer(T2_SDP.to_string())
        );
    }

    #[test]
    fn a_multipart_invite_with_parts_reversed_or_a_quoted_boundary_is_an_offer() {
        let reversed = format!(
            "--x y\r\nContent-Type: application/sdp\r\n\r\n{BARE_SDP}\r\n\
             --x y\r\nContent-Type: application/pidf+xml\r\n\
             Content-Disposition: render;handling=optional\r\n\r\n<presence/>\r\n--x y--\r\n"
        );
        let req = invite_with(Some("multipart/mixed; boundary=\"x y\""), &reversed);
        assert_eq!(
            invite_body_decision(&req, "tt"),
            InviteBody::Offer(BARE_SDP.to_string())
        );
    }

    /// The pre-existing shapes are unchanged: bare SDP (typed or untyped) is
    /// an offer, no body is offerless.
    #[test]
    fn plain_sdp_and_no_body_keep_their_existing_meaning() {
        for ct in [Some("application/sdp"), Some("APPLICATION/SDP"), None] {
            assert_eq!(
                invite_body_decision(&invite_with(ct, BARE_SDP), "tt"),
                InviteBody::Offer(BARE_SDP.to_string()),
                "{ct:?}"
            );
        }
        assert_eq!(
            invite_body_decision(&invite_with_content_type(None), "tt"),
            InviteBody::Offerless
        );
        // Content-Length: 0 with a Content-Type is still offerless.
        assert_eq!(
            invite_body_decision(&invite_with_content_type(Some("application/sdp")), "tt"),
            InviteBody::Offerless
        );
    }

    /// specs/088 FR-008: a body with no session description is `415` with
    /// the multipart-aware `Accept`, on the call's own To-tag.
    #[test]
    fn a_body_with_no_session_description_is_refused_415() {
        let pidf_only = "--b\r\nContent-Type: application/pidf+xml\r\n\r\n<presence/>\r\n--b--\r\n";
        for req in [
            invite_with(Some("text/plain"), "hello"),
            invite_with(Some("application/isup"), "x"),
            invite_with(Some("multipart/mixed;boundary=b"), pidf_only),
        ] {
            let response = refusal(invite_body_decision(&req, "tt"));
            assert!(response.starts_with("SIP/2.0 415"), "{response}");
            assert!(
                response.contains(
                    "Accept: application/sdp, multipart/mixed, multipart/alternative\r\n"
                ),
                "{response}"
            );
            assert!(response.contains(";tag=tt"), "{response}");
        }
    }

    /// specs/088 FR-009: broken or ambiguous multipart is `400`, never
    /// scanned as SDP.
    #[test]
    fn a_malformed_or_ambiguous_multipart_is_refused_400() {
        let unclosed = format!("--b\r\nContent-Type: application/sdp\r\n\r\n{BARE_SDP}");
        let two = format!(
            "--b\r\nContent-Type: application/sdp\r\n\r\n{BARE_SDP}\r\n\
             --b\r\nContent-Type: application/sdp\r\n\r\n{BARE_SDP}\r\n--b--\r\n"
        );
        for body in [unclosed, two] {
            let req = invite_with(Some("multipart/mixed;boundary=b"), &body);
            let response = refusal(invite_body_decision(&req, "tt"));
            assert!(
                response.starts_with("SIP/2.0 400 Bad Request"),
                "{response}"
            );
            assert!(response.contains(";tag=tt"), "{response}");
        }
    }

    fn invite_with_body(body: &str) -> SipRequest {
        let raw = format!(
            "INVITE sip:me@10.0.0.9:5060 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.1.1.1:5067;branch=z9hG4bKone\r\n\
             From: <sip:caller@example.net>;tag=abc\r\n\
             To: <sip:me@example.net>\r\n\
             Call-ID: c1\r\nCSeq: 1 INVITE\r\n\
             Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
    }

    fn ack_with(content_type: Option<&str>, body: &str) -> SipRequest {
        let mut req = invite_with(content_type, body);
        req.method = "ACK".into();
        req
    }

    /// specs/088 T027: an offerless call's ACK is decoded like an INVITE.
    #[test]
    fn an_acks_sdp_answer_is_decoded_from_any_body_shape() {
        let multipart = format!(
            "--b\r\nContent-Type: application/pidf+xml\r\n\r\n<presence/>\r\n\
             --b\r\nContent-Type: application/sdp\r\n\r\n{BARE_SDP}\r\n--b--\r\n"
        );
        assert_eq!(
            ack_answer(&ack_with(Some("multipart/mixed;boundary=b"), &multipart)).as_deref(),
            Some(BARE_SDP)
        );
        assert_eq!(
            ack_answer(&ack_with(Some("application/sdp"), BARE_SDP)).as_deref(),
            Some(BARE_SDP)
        );
        let unclosed = "--b\r\nContent-Type: application/sdp\r\n\r\nv=0\r\n";
        for (ct, body) in [
            (Some("multipart/mixed;boundary=b"), unclosed),
            (Some("text/plain"), "hello"),
            (Some("application/sdp"), ""),
            (None, ""),
        ] {
            assert_eq!(ack_answer(&ack_with(ct, body)), None, "{ct:?} {body:?}");
        }
    }

    /// specs/047-offerless-invite-sms-reassembly (SDP-04), T018: an INVITE
    /// with no body at all — RFC 3261 §14.2/RFC 3264 §3's offerless case —
    /// is what `handle_invite` routes to `handle_offerless_invite` instead
    /// of `sdp::parse_offer`. `handle_invite` has no unit-test seam (it
    /// needs a live control-channel/veth-listener socket this codebase's
    /// test suite doesn't stand up — see quickstart.md), so the routing
    /// decision, `invite_body_decision`, is pinned directly here.
    #[test]
    fn an_invite_with_no_body_is_recognized_as_offerless() {
        let offerless = invite_with_content_type(None);
        assert_eq!(
            invite_body_decision(&offerless, "tt"),
            InviteBody::Offerless
        );

        // Content-Length: 0 but an explicit Content-Type still counts as
        // offerless — RFC 3264 §3 cares about the body's presence, not
        // whether a header claimed a type for a body that isn't there.
        let empty_typed_body = invite_with_body("");
        assert_eq!(
            invite_body_decision(&empty_typed_body, "tt"),
            InviteBody::Offerless
        );

        // Whitespace-only is not a real offer either — RFC 4566's grammar
        // has no such thing as a body that is only line endings.
        let whitespace_only = invite_with_body("\r\n\r\n");
        assert_eq!(
            invite_body_decision(&whitespace_only, "tt"),
            InviteBody::Offerless
        );
    }

    /// specs/047-offerless-invite-sms-reassembly (SDP-04), T019: the
    /// regression guard for SC-005/FR-007 — an ordinary INVITE that already
    /// states its media must never take the new offerless branch. Pinned at
    /// the same branch-condition level as the test above, since that
    /// condition is the entire routing decision (research.md Decision 1).
    #[test]
    fn an_invite_with_a_real_offer_is_not_recognized_as_offerless() {
        let ordinary = invite_with_body(
            "v=0\r\nc=IN IP4 10.0.0.5\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n",
        );
        assert!(matches!(
            invite_body_decision(&ordinary, "tt"),
            InviteBody::Offer(_)
        ));
    }

    /// The `200 OK` answering a carrier INVITE must state what we accept.
    /// This was the difference between an inbound Jio call that holds and one
    /// torn down ~460 ms later with `cause=503 "SIP SDP Protocol Error"`, and
    /// it is no longer conditional on anything, so nothing may drop it back
    /// out of the response by accident. `Supported` is pinned here too
    /// (specs/050, reversing MT-10): dropping it was hardware-verified only
    /// against Vi, and live-verified 2026-09-15 to reintroduce this exact
    /// `cause=503` teardown on the Jio line — Jio's IMS core requires it
    /// regardless of whether this UAS implements any behaviour behind it.
    #[test]
    fn the_answering_response_states_our_capabilities() {
        let invite = "INVITE sip:me@10.0.0.9:5060 SIP/2.0\r\n\
                      Via: SIP/2.0/UDP 10.1.1.1:5067;branch=z9hG4bKone\r\n\
                      From: <sip:caller@example.net>;tag=abc\r\n\
                      To: <sip:me@example.net>\r\n\
                      Call-ID: c1\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(invite.as_bytes()).unwrap().unwrap();

        let resp = build_uas_response_with_headers(
            200,
            "OK",
            &req,
            Some("totag1"),
            Some("<sip:me@10.0.0.9:5060>"),
            Some("v=0\r\n"),
            &[
                ("Allow", super::super::ALLOW),
                ("Supported", "timer, 100rel, replaces, path, gruu"),
                ("P-Access-Network-Info", "3GPP-WLAN"),
            ],
        );

        assert!(
            resp.contains("\r\nAllow: INVITE, ACK, CANCEL, BYE,"),
            "RFC 3261 §13.3.1.4 wants Allow in a 2xx to INVITE: {resp}"
        );
        assert!(
            resp.contains("\r\nSupported: timer, 100rel, replaces, path, gruu\r\n"),
            "Jio's IMS core requires this even with no behaviour behind it: {resp}"
        );
        // specs/045 MT-11: states the line's real access-network value.
        assert!(
            resp.contains("\r\nP-Access-Network-Info: 3GPP-WLAN\r\n"),
            "must state the real access network: {resp}"
        );
    }

    /// The `200 OK` to an inbound INVITE must claim `UPDATE`. Bisected on a
    /// live Jio line 2026-09-15: without it, every call routed through one of
    /// Jio's Lucent border elements was torn down within milliseconds of the
    /// `ACK` with `cause=503 "SDP Protocol Error"`; with it, 10/10 answered.
    /// Swapping `UPDATE` for `PRACK` — same list length — failed again, so it
    /// is this token specifically. See `ims::UAS_INVITE_ALLOW`.
    #[test]
    fn the_answer_claims_update_even_though_this_uas_does_not_serve_it() {
        let headers = uas_answer_headers("3GPP-WLAN");
        let allow = headers
            .iter()
            .find(|(n, _)| *n == "Allow")
            .expect("an answer always states Allow")
            .1;
        assert!(
            allow.split(", ").any(|m| m == "UPDATE"),
            "Jio tears the call down without it: {allow}"
        );
        assert!(
            !super::super::ALLOW.contains("UPDATE"),
            "the dispatch loop's own list stays honest; only the answer lies"
        );
        // Everything else the answer claims is unchanged.
        assert_eq!(
            headers,
            [
                ("Allow", crate::ims::UAS_INVITE_ALLOW),
                ("Supported", "timer, 100rel, replaces, path, gruu"),
                ("P-Access-Network-Info", "3GPP-WLAN"),
            ]
        );
    }

    /// The claim is scoped to answers. A `405` to a method we really don't
    /// serve — including an `UPDATE` that takes the claim up — must not repeat
    /// it, or the refusal would contradict itself.
    #[test]
    fn the_update_claim_does_not_leak_into_the_honest_method_list() {
        assert_eq!(
            super::super::ALLOW,
            "INVITE, ACK, CANCEL, BYE, OPTIONS, MESSAGE, NOTIFY"
        );
    }

    /// specs/043 MT-05: an inbound INVITE offering RFC 4028 session timers
    /// must not get its `Session-Expires` echoed back — this UAS never
    /// implements the session-refresh role, and RFC 4028 §9 explicitly
    /// permits a UAS to simply omit `Session-Expires` when it doesn't want
    /// the extension. `Supported: timer` is stated anyway (specs/050): that
    /// header is carrier-interop boilerplate, not a promise to refresh.
    #[test]
    fn session_expires_on_the_offer_is_never_echoed_back() {
        let invite = "INVITE sip:me@10.0.0.9:5060 SIP/2.0\r\n\
                      Via: SIP/2.0/UDP 10.1.1.1:5067;branch=z9hG4bKone\r\n\
                      From: <sip:caller@example.net>;tag=abc\r\n\
                      To: <sip:me@example.net>\r\n\
                      Call-ID: c1\r\nCSeq: 1 INVITE\r\n\
                      Session-Expires: 1800;refresher=uac\r\n\
                      Supported: timer\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(invite.as_bytes()).unwrap().unwrap();

        let resp = build_uas_response_with_headers(
            200,
            "OK",
            &req,
            Some("totag1"),
            Some("<sip:me@10.0.0.9:5060>"),
            Some("v=0\r\n"),
            &[
                ("Allow", super::super::ALLOW),
                ("Supported", "timer, 100rel, replaces, path, gruu"),
                ("P-Access-Network-Info", "3GPP-WLAN"),
            ],
        );

        assert!(
            !resp.contains("Session-Expires"),
            "must not echo a session-refresh promise nothing here honours: {resp}"
        );
    }

    /// specs/045 MT-04: `100rel` (RFC 3262 reliable provisionals) is stated
    /// in `Supported` as carrier-interop boilerplate (specs/050) but never
    /// backed by real behaviour — a caller `Require`ing it is still
    /// declined `420` (MT-03's existing gate) rather than silently
    /// accepted.
    #[test]
    fn a_required_100rel_is_still_declined_despite_the_boilerplate_supported() {
        let requiring_100rel = "INVITE sip:me@10.0.0.9:5060 SIP/2.0\r\n\
                      Via: SIP/2.0/UDP 10.1.1.1:5067;branch=z9hG4bKtwo\r\n\
                      From: <sip:caller@example.net>;tag=abc\r\n\
                      To: <sip:me@example.net>\r\n\
                      Call-ID: c2\r\nCSeq: 1 INVITE\r\n\
                      Require: 100rel\r\nContent-Length: 0\r\n\r\n";
        let (req2, _) = SipRequest::try_parse(requiring_100rel.as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(
            super::super::unsupported_required_extensions(&req2),
            vec!["100rel".to_string()],
            "Require: 100rel must still be declined, not silently honoured"
        );
    }

    /// Jio's inbound INVITE carries
    /// `Accept-Contact: *;require;explicit;+sip.instance="<urn:gsma:imei:...>"`
    /// and a second one naming the MMTEL ICSI. `require` makes those caller
    /// preferences mandatory, so the contact that answers has to advertise
    /// them — a bare `<sip:user@addr;transport=...>` does not.
    #[test]
    fn the_answering_contact_advertises_the_tags_the_caller_requires() {
        let c = uas_contact(
            "405800000000000",
            "10.0.0.1:41126",
            "TCP",
            "860000000000000",
        );

        assert!(
            c.starts_with("<sip:405800000000000@10.0.0.1:41126;transport=TCP>"),
            "the address must still lead, or nothing network-initiated reaches us: {c}"
        );
        assert!(
            c.contains(";+g.3gpp.icsi-ref=\"urn%3Aurn-7%3A3gpp-service.ims.icsi.mmtel\""),
            "TS 24.229 §5.1.3 has the UE state its IMS communication service: {c}"
        );
        assert!(
            c.contains(";+sip.instance=\"<urn:gsma:imei:860000000000000>\""),
            "the instance the caller's Accept-Contact requires: {c}"
        );
        // The feature tags are Contact *header* parameters, so they must sit
        // outside the angle brackets — inside, they would be URI parameters and
        // mean something else entirely.
        let closing = c.find('>').expect("the URI must be bracketed");
        assert!(
            c.find("+sip.instance").expect("tag present") > closing,
            "feature tags belong after the closing bracket: {c}"
        );
    }

    /// The Contact states the transport we registered over, whichever that was
    /// — it is the Request-URI of the ACK and of every in-dialog request the
    /// network sends us, so it has to name a leg we are actually listening on.
    #[test]
    fn the_answering_contact_names_the_transport_we_registered_over() {
        let c = uas_contact(
            "405800000000000",
            "10.0.0.1:41126",
            "UDP",
            "860000000000000",
        );

        assert!(
            c.starts_with("<sip:405800000000000@10.0.0.1:41126;transport=UDP>"),
            "the registered transport, inside the brackets: {c}"
        );
        assert!(
            c.contains(";+sip.instance=\"<urn:gsma:imei:860000000000000>\""),
            "the required feature tags must survive: {c}"
        );
    }

    // ----- specs/120: forwarding offers ------------------------------------

    const JIO_OFFER: &str = "v=0\r\n\
         o=JIO_ISBC 1 1 IN IP4 ims.example\r\n\
         s=-\r\nc=IN IP4 192.0.2.50\r\nt=0 0\r\n\
         m=audio 19730 RTP/AVP 104 110 105\r\n\
         a=rtpmap:104 AMR-WB/16000\r\n\
         a=rtpmap:110 AMR-WB/16000\r\n\
         a=rtpmap:105 telephone-event/16000\r\na=fmtp:105 0-15\r\n\
         a=sendrecv\r\na=ptime:20\r\na=maxptime:240\r\n\
         a=fmtp:104 mode-set=0,1,2,3;mode-change-capability=2\r\n\
         a=fmtp:110 mode-set=0,1,2,3;octet-align=1; mode-change-capability=2\r\n";

    fn carrier_for(enabled: bool, body: &str) -> Option<AmrLegParams> {
        carrier_amr_for(
            enabled,
            crate::amr_forward::ModeSetPolicy::Strict,
            "192.0.2.1".parse().unwrap(),
            &sdp::parse_offer(body).unwrap(),
            true,
            sdp::AnswerPreference::legacy(),
        )
    }

    #[test]
    fn the_carrier_leg_is_reported_only_when_passthrough_is_on_and_the_codec_is_amr() {
        // Off: nothing, so IncomingCall is the message it always was.
        assert_eq!(carrier_for(false, JIO_OFFER), None);
        // On with an AMR-WB offer (amr_safe may be unavailable in this build,
        // in which case no AMR codec is chosen and that is also `None`).
        let on = carrier_for(true, JIO_OFFER);
        if amr_safe::is_available() {
            let leg = on.expect("AMR-WB was chosen");
            assert_eq!((leg.payload_type, leg.octet_aligned), (110, true));
            assert_eq!(leg.mode_set, Some(0b1111));
        } else {
            assert_eq!(on, None);
        }
        // A PCMU-only offer is never AMR.
        let pcmu = "v=0\r\nc=IN IP4 192.0.2.50\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
        assert_eq!(carrier_for(true, pcmu), None);
    }

    fn client_wb(fmtp: &str) -> AmrLegParams {
        use crate::amr_forward::SdpLeg;
        AmrLegParams::from_sdp(&SdpLeg {
            flavour: crate::ims::amr_rtp::AmrKind::Wb,
            fmtp,
            payload_type: 96,
            dtmf_payload_type: Some(101),
            ptime_ms: Some(20),
            maxptime_ms: None,
        })
        .unwrap()
    }

    /// A control connection pair: Agent A's writer end and the "Agent B" end.
    fn control_pair() -> (TcpStream, TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (b, _) = listener.accept().unwrap();
        b.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        (a, b)
    }

    /// Runs `await_pbx_answer` through a script of control messages ending in
    /// `CallAnswered`, returning what Agent B read back and the hop socket.
    fn ring_with(
        carrier: Option<&AmrLegParams>,
        script: Vec<ControlMessage>,
    ) -> (Vec<ControlMessage>, Option<UdpSocket>) {
        use crate::ims::sip_client::SipTransport;
        let carrier_side = UdpSocket::bind("127.0.0.1:0").unwrap();
        let sink = SipTransport::connect(carrier_side.local_addr().unwrap(), false)
            .unwrap()
            .sink()
            .unwrap();
        let (sip_tx, sip_rx) = mpsc::channel::<(SipMessage, SipSink)>();
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<ControlMessage>();
        for m in script {
            ctrl_tx.send(m).unwrap();
        }
        ctrl_tx
            .send(ControlMessage::CallAnswered {
                call_id: "c1".into(),
            })
            .unwrap();
        let req = invite_with_headers("From: <sip:+919000000000@ims.example>;tag=abc\r\n");
        let (mut a_end, b_end) = control_pair();
        let mut offer = ForwardOffer {
            carrier,
            veth_local_ip: "127.0.0.1".parse().unwrap(),
            control: &mut a_end,
            pending: None,
        };
        let mut held = HeldMessages::new(&sip_tx);
        let outcome = await_pbx_answer(
            "c1",
            &ctrl_rx,
            &sip_rx,
            &req,
            "tag",
            "<sip:me@10.0.0.9>",
            &sink,
            Some(&mut offer),
            &mut held,
        )
        .unwrap();
        assert!(matches!(outcome, RingOutcome::Answered));
        let pending = offer.pending.take();
        drop(a_end);
        let mut reader = std::io::BufReader::new(b_end);
        let mut got = Vec::new();
        while let Ok(m) = crate::vowifi::control::read_msg(&mut reader) {
            got.push(m);
        }
        (got, pending)
    }

    #[test]
    fn a_compatible_forward_offer_is_acked_with_a_hop_socket_aimed_at_agent_b() {
        let b_hop = UdpSocket::bind("127.0.0.1:0").unwrap();
        b_hop
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let carrier = client_wb("octet-align=1");
        let (replies, pending) = ring_with(
            Some(&carrier),
            vec![ControlMessage::ForwardMedia {
                call_id: "c1".into(),
                rtp_port: b_hop.local_addr().unwrap().port(),
                client: client_wb(""),
            }],
        );
        let hop = pending.expect("an accepted offer leaves the hop socket ready");
        match replies.as_slice() {
            [ControlMessage::ForwardAck {
                ok: true,
                reason: None,
                rtp_port: Some(port),
                ..
            }] => {
                assert_eq!(*port, hop.local_addr().unwrap().port());
            }
            other => panic!("unexpected replies: {other:?}"),
        }
        // The hop socket is connected to Agent B's port.
        hop.send(b"ping").unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(b_hop.recv(&mut buf).unwrap(), 4);
    }

    #[test]
    fn an_incompatible_forward_offer_is_declined_with_the_reason() {
        let carrier = client_wb("octet-align=1");
        let restricted = client_wb("octet-align=1; mode-set=0,1");
        let (replies, pending) = ring_with(
            Some(&carrier),
            vec![ControlMessage::ForwardMedia {
                call_id: "c1".into(),
                rtp_port: 40000,
                client: restricted,
            }],
        );
        assert!(pending.is_none());
        match replies.as_slice() {
            [ControlMessage::ForwardAck {
                ok: false,
                reason: Some(r),
                rtp_port: None,
                ..
            }] => {
                assert_eq!(r, "mode_set")
            }
            other => panic!("unexpected replies: {other:?}"),
        }
    }

    #[test]
    fn a_forward_offer_on_a_call_whose_carrier_is_not_amr_is_declined() {
        let (replies, pending) = ring_with(
            None,
            vec![ControlMessage::ForwardMedia {
                call_id: "c1".into(),
                rtp_port: 40000,
                client: client_wb(""),
            }],
        );
        assert!(pending.is_none());
        assert!(matches!(
            replies.as_slice(),
            [ControlMessage::ForwardAck { ok: false, .. }]
        ));
    }

    #[test]
    fn a_cancelled_forward_leaves_the_call_on_the_transcoding_relay() {
        let carrier = client_wb("octet-align=1");
        let (replies, pending) = ring_with(
            Some(&carrier),
            vec![
                ControlMessage::ForwardMedia {
                    call_id: "c1".into(),
                    rtp_port: 40000,
                    client: client_wb(""),
                },
                ControlMessage::ForwardCancel {
                    call_id: "c1".into(),
                },
            ],
        );
        assert!(pending.is_none(), "cancel discards the pending hop socket");
        assert!(matches!(
            replies.as_slice(),
            [ControlMessage::ForwardAck { ok: true, .. }]
        ));
    }

    #[test]
    fn without_a_forward_offer_nothing_is_pending_and_nothing_is_sent() {
        let carrier = client_wb("octet-align=1");
        let (replies, pending) = ring_with(Some(&carrier), vec![]);
        assert!(pending.is_none());
        assert!(replies.is_empty(), "today's message sequence is unchanged");
    }
}
