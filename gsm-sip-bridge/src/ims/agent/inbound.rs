//! Answering — or declining — one carrier `INVITE`.
//!
//! Split out of `agent::mod` because the inbound path has its own distinct
//! shape: it must hold the carrier in the *ringing* state (real ringback to
//! the caller) while a human decides whether to pick up the PBX extension, and
//! service the carrier's own signaling throughout in case the caller gives up
//! first. That wait ([`await_pbx_answer`]) is the whole reason this is not a
//! straight-line handler.

use super::call::{spawn_control_reader, ActiveCall, DialogInfo};
use super::observability;
use super::veth::{spawn_relay, spawn_veth_uas_listener, VETH_INVITE_TIMEOUT};
use super::CONTROL_TIMEOUT;
use crate::control::protocol::{BridgeFailureReason, CallStatus};
use crate::error::{BridgeError, BridgeResult};
use crate::ims::lifecycle::{BridgedCall, CallStage};
use crate::ims::sdp::{self, NegotiatedCodec};
use crate::ims::session::{extract_caller, respond, Inbound};
use crate::ims::sip_client::{
    build_100_trying, build_180_ringing, build_200_ok_invite, build_486_busy_here,
    build_uas_response, format_sip_addr, random_hex, SipMessage, SipRequest, SipSink,
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
}

/// Answers (or declines) one inbound carrier `INVITE`. Returns `Some` with
/// the bookkeeping `handle_bye` will need once the call is actually
/// bridged; `None` if it was declined (busy line, no compatible codec, or
/// Agent B couldn't bridge it) — every decline path sends a fast, explicit
/// `486 Busy Here` per the spec's Clarifications answer, never silence or
/// unanswered ringing (FR-009/FR-010).
pub(super) fn handle_invite(
    session: &crate::ims::RegisteredSession,
    req: &SipRequest,
    sink: &SipSink,
    inbound: &Inbound,
    ctx: &InviteContext,
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

    sink.send(&build_100_trying(req))?;

    let offer = sdp::parse_offer(&req.body)?;
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
        sink.send(&build_486_busy_here(req, &random_hex(4)))?;
        obs.report_call_not_answered(
            CallStatus::Failed,
            BridgeFailureReason::BridgeSetupFailed,
            &caller,
            started_at,
        );
        return Ok(None);
    };

    // Only a wideband *carrier* leg has anything for a wideband veth leg to
    // preserve. A narrowband call (PCMU or AMR-NB — the two shapes Airtel
    // sends when the originating leg is narrowband) keeps the veth link on
    // PCMU, exactly the path it took before L16 existed.
    let veth_wideband = precheck.codec == NegotiatedCodec::AmrWb;
    let veth_rx = spawn_veth_uas_listener(ctx.veth_local_ip, ctx.veth_sip_port, veth_wideband)?;

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

    let mut control = TcpStream::connect_timeout(&ctx.control_addr, CONTROL_TIMEOUT)
        .map_err(|e| BridgeError::Ims(format!("failed to reach Agent B control channel: {e}")))?;
    write_msg(
        &mut control,
        &ControlMessage::IncomingCall {
            call_id: call_id.clone(),
            caller: caller.clone(),
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
                ctx.wideband,
                ctx.answer_preference,
            )?;

            // Do NOT answer yet. The PBX extension is only ringing; our
            // `180 Ringing` above is what makes the network play ringback to
            // the caller, and a `200 OK` here would cut that off and leave them
            // in silence until someone picks up. Wait for Agent B to report a
            // real answer — while still watching the carrier's own signaling,
            // since the caller may give up (`CANCEL`) while it rings.
            match await_pbx_answer(&call_id, &ctrl_rx, inbound, req, &to_tag, sink)? {
                RingOutcome::Answered => {}
                RingOutcome::PbxDeclined => {
                    obs.report_call_not_answered(
                        CallStatus::Missed,
                        BridgeFailureReason::PbxDeclined,
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

            sink.send(&build_200_ok_invite(req, &to_tag, &contact, &answer_sdp))?;

            let stop = Arc::new(AtomicBool::new(false));
            // Counts audio each way so the completed call can be judged
            // both-ways or one-way (FR-017) — the same guard the outbound path
            // applies, here on the shared inbound bridge both transports use.
            let meter = crate::ims::media_stats::MediaMeter::new();
            let transcoding = chosen.codec != veth.codec.codec;
            if transcoding {
                // The two legs speak different codecs (or the same codec at
                // different rates), so it has to be terminated on each side
                // and re-encoded.
                crate::ims::transcode::spawn_transcoding_relay(
                    ims_rtp_socket,
                    veth.rtp_socket,
                    chosen,
                    veth.codec,
                    stop.clone(),
                    &meter,
                )?;
            } else {
                // Both legs speak PCMU: forward the payloads untouched.
                spawn_relay(ims_rtp_socket, veth.rtp_socket, stop.clone(), &meter);
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
                dialog: DialogInfo::from_invite(req, &to_tag, session),
                call_id,
                to_tag,
                caller,
                answered_at: Utc::now(),
                answered_instant: Instant::now(),
                meter,
                lifecycle,
            }))
        }
        ControlMessage::BridgeFailed {
            reason: fail_reason,
            ..
        } => {
            tracing::info!(call_id = %call_id, reason = %fail_reason, "Agent B could not bridge the call, declining");
            sink.send(&build_486_busy_here(req, &random_hex(4)))?;
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

    let deadline = Instant::now() + RING_TIMEOUT;
    while Instant::now() < deadline {
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
