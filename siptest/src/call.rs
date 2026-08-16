//! Shared call identity and lifecycle types, used by both the outbound/inbound
//! dialog FSMs and the API layer. `CallReport` (the verdict bundle) lives in
//! [`crate::media::report`] since it is produced by the media layer.

use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gsm_sip_bridge::ims::media_stats::{self, ReceiveStats};
use gsm_sip_bridge::ims::sip_client::{
    build_100_trying, build_180_ringing, build_200_ok_invite, ByeRequest, SipRequest,
};
use serde::Serialize;

use crate::api::state::{InboundMode, ManualDecision, SharedState};
use crate::error::{SipTestError, SipTestResult};
use crate::media::codec::{resolve_codec, select_inbound_codec};
use crate::media::report::{
    CallReport, MediaCounters, Recordings, RequireLevel, SignallingTimings,
};
use crate::safety::SafetyRefusal;
use crate::sip::outbound;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct CallId(pub String);

impl std::fmt::Display for CallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Outbound,
    Inbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    Idle,
    Inviting,
    Redirected,
    Ringing,
    Answered,
    Terminating,
    Ended,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum EndReason {
    DurationElapsed,
    LocalHangup,
    RemoteBye,
    CallerCancelled,
    RingTimeout,
    Rejected { status: u16 },
    Failed { detail: String },
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CallerId {
    pub from: Option<String>,
    pub p_asserted_identity: Option<String>,
    pub x_gsm_caller_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Call {
    pub id: CallId,
    pub direction: Direction,
    pub state: CallState,
    pub peer: String,
    pub peer_uri: String,
    pub caller_id: CallerId,
    pub started_at: Instant,
    pub end_reason: Option<EndReason>,
    pub report: Option<CallReport>,
}

impl Call {
    pub fn is_terminal(&self) -> bool {
        matches!(self.state, CallState::Ended)
    }
}

fn pick_rtp_port(min: u16, max: u16) -> u16 {
    if max <= min {
        return min;
    }
    min + (rand::random::<u16>() % (max - min))
}

/// Places one outbound call end to end: the safety gate, the 302 dance,
/// media, and BYE — everything US1's acceptance scenarios require. Runs
/// synchronously on whichever thread calls it (the API layer wraps it in
/// `spawn_blocking`), for the whole duration of the call.
pub fn execute_outbound_call(
    state: &SharedState,
    destination: String,
    duration: Duration,
    ring_timeout: Duration,
    codec_name: &str,
) -> SipTestResult<Call> {
    let codec = resolve_codec(codec_name).map_err(SipTestError::InvalidCodec)?;
    let now = Instant::now();
    {
        let history = state
            .attempt_history
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state
            .safety
            .check(&destination, &history, now)
            .map_err(|refusal| match refusal {
                SafetyRefusal::NotAllowed => {
                    SipTestError::DestinationNotAllowed(destination.clone())
                }
                SafetyRefusal::RateLimited { retry_after_s } => {
                    SipTestError::RateLimited { retry_after_s }
                }
            })?;
    }
    if state
        .registration
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .state
        != crate::sip::registration::RegState::Registered
    {
        return Err(SipTestError::NotRegistered);
    }
    if state
        .calls
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .active()
        .is_some()
    {
        return Err(SipTestError::CallInProgress);
    }
    state
        .attempt_history
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .record(now);
    state
        .counters
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .calls_placed += 1;

    let call_id = state.next_call_id();
    let rtp_port = pick_rtp_port(
        state.config.media.rtp_port_min,
        state.config.media.rtp_port_max,
    );

    let mut call = Call {
        id: call_id.clone(),
        direction: Direction::Outbound,
        state: CallState::Inviting,
        peer: destination.clone(),
        peer_uri: String::new(),
        caller_id: CallerId::default(),
        started_at: now,
        end_reason: None,
        report: None,
    };
    state
        .calls
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .upsert(call.clone());
    state.events.publish(
        "call_state",
        serde_json::json!({"call_id": call_id.0, "state": "inviting", "destination": destination}),
    );

    let outcome = outbound::place_call(
        &state.sip_socket,
        state.bridge_registrar,
        &state.config.sip.realm,
        &state.config.sip.username,
        &destination,
        codec,
        rtp_port,
        ring_timeout,
    )?;

    if let Some(observed) = outcome.remote_target {
        *state
            .last_outbound_observed
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(observed);
    }

    let require: RequireLevel = state
        .config
        .call
        .require
        .parse()
        .unwrap_or(RequireLevel::Packets);

    if !outcome.answered {
        call.state = CallState::Ended;
        call.end_reason = Some(match outcome.refusal_reason {
            Some("ring_timeout") => EndReason::RingTimeout,
            Some(reason) => EndReason::Failed {
                detail: reason.to_string(),
            },
            None => EndReason::Failed {
                detail: format!("status {}", outcome.final_status),
            },
        });
        let signalling = SignallingTimings {
            invite_to_180_ms: outcome.invite_to_180_ms,
            invite_to_200_ms: outcome.invite_to_200_ms,
            answer_to_first_rtp_ms: None,
            final_status: Some(outcome.final_status),
        };
        let media_counters = MediaCounters::new(codec, 0, &ReceiveStats::default());
        call.report = Some(CallReport::build(
            signalling,
            media_counters,
            false,
            media_stats::DirectionVerdict::Neither,
            require,
            Recordings {
                received: None,
                sent: None,
            },
        ));
        finish(state, call_id, call.clone());
        return Ok(call);
    }

    call.state = CallState::Answered;
    let dialog = outcome
        .dialog
        .expect("answered outcome always carries a confirmed dialog");
    call.peer_uri = format!("sip:{}@{}", dialog.to_user, dialog.remote_target);

    let sdp_answer = outcome
        .sdp_answer
        .expect("answered outcome always carries an SDP answer");
    let remote_rtp = sdp_answer.remote_rtp;
    let local_rtp = SocketAddr::new(state.sip_socket.local_ip, rtp_port);

    let recording_dir = &state.config.media.recording_dir;
    let _ = std::fs::create_dir_all(recording_dir);
    let sent_wav_path = state
        .config
        .media
        .record
        .then(|| recording_dir.join(format!("{}-sent.wav", call_id.0)));
    let received_wav_path = state
        .config
        .media
        .record
        .then(|| recording_dir.join(format!("{}-received.wav", call_id.0)));

    let tone_enabled = state.config.media.tone_plan != "silence";
    let stop = Arc::new(AtomicBool::new(false));
    let media_result = crate::media::session::run(
        crate::media::session::MediaSessionConfig {
            local_rtp,
            remote_rtp,
            codec,
            duration,
            sent_wav_path: sent_wav_path.clone(),
            received_wav_path: received_wav_path.clone(),
            tone_enabled,
        },
        stop,
    )?;

    let _ = outbound::send_bye(&state.sip_socket, &dialog);

    let packets = media_stats::verdict(
        media_result.sent_packets,
        media_result.receive_stats.received_packets,
        media_stats::DEFAULT_ONE_WAY_THRESHOLD_PERCENT,
    );
    let signalling = SignallingTimings {
        invite_to_180_ms: outcome.invite_to_180_ms,
        invite_to_200_ms: outcome.invite_to_200_ms,
        answer_to_first_rtp_ms: None,
        final_status: Some(200),
    };
    let media_counters = MediaCounters::new(
        codec,
        media_result.sent_packets,
        &media_result.receive_stats,
    )
    .with_tone_and_level(&media_result.level, &media_result.tone);
    let recordings = Recordings {
        received: received_wav_path.map(|p| p.display().to_string()),
        sent: sent_wav_path.map(|p| p.display().to_string()),
    };
    call.report = Some(CallReport::build(
        signalling,
        media_counters,
        true,
        packets,
        require,
        recordings,
    ));
    call.state = CallState::Ended;
    call.end_reason = Some(EndReason::DurationElapsed);

    finish(state, call_id, call.clone());
    Ok(call)
}

/// Handles one inbound INVITE end to end: `100`, caller-ID capture, `180`,
/// policy (answer/reject/manual), the `200`-OK T1 retransmit ladder, media,
/// and our own `BYE` at the end (contracts/sip-flows.md C-3). Called by the
/// daemon's inbound listener thread, which has already confirmed no other
/// call is active — a second concurrent INVITE reaching the socket while
/// this function runs is busied out by `sip::inbound`'s stray-request
/// handling, not by this function itself.
///
/// Runs to completion on the calling thread; the caller decides whether that
/// blocks anything else (in this MVP, the inbound listener has nothing else
/// to do while a call is active, since `max_concurrent` is 1).
pub fn execute_inbound_call(state: &SharedState, req: SipRequest, peer: SocketAddr) {
    let start = Instant::now();
    let call_id_hdr = req.header("Call-ID").unwrap_or("").to_string();
    let caller_id = crate::sip::inbound::extract_caller_id(&req);
    let call_id = state.next_call_id();

    let _ = state.sip_socket.send(peer, &build_100_trying(&req));

    let peer_display = caller_id
        .x_gsm_caller_id
        .clone()
        .or_else(|| caller_id.from.clone())
        .unwrap_or_else(|| "unknown".to_string());

    let mut call = Call {
        id: call_id.clone(),
        direction: Direction::Inbound,
        state: CallState::Ringing,
        peer: peer_display,
        peer_uri: req.request_uri.clone(),
        caller_id: caller_id.clone(),
        started_at: start,
        end_reason: None,
        report: None,
    };
    state
        .calls
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .upsert(call.clone());
    state
        .counters
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .calls_received += 1;
    state.events.publish(
        "incoming_call",
        serde_json::json!({
            "call_id": call_id.0,
            "caller_id": {
                "from": caller_id.from,
                "p_asserted_identity": caller_id.p_asserted_identity,
                "x_gsm_caller_id": caller_id.x_gsm_caller_id,
            }
        }),
    );

    let require: RequireLevel = state
        .config
        .call
        .require
        .parse()
        .unwrap_or(RequireLevel::Packets);

    // The offer must name a codec we can answer with, constrained by both
    // `[media].codec` and what the caller actually offered — an unusable
    // offer is refused before ever ringing.
    let offer = match crate::sdp::parse_offer(&req.body) {
        Ok(o) => o,
        Err(_) => {
            let to_tag = crate::sip::message::new_tag();
            let _ = state
                .sip_socket
                .send(peer, &crate::sip::inbound::build_488(&req, &to_tag));
            call.state = CallState::Ended;
            call.end_reason = Some(EndReason::Failed {
                detail: "unparseable SDP offer".to_string(),
            });
            finish(state, call_id, call);
            return;
        }
    };
    // The signalling source-IP check in `daemon::inbound_listener_loop` only
    // establishes who sent the INVITE — it says nothing about where the SDP
    // body then tells us to send RTP. Without this, a signalling peer we do
    // trust could still name a third party's address in `c=`/`m=` and turn
    // every answered call into sustained RTP traffic aimed at them. The
    // offer's media address must be the same host the signalling itself came
    // from.
    if offer.remote_rtp.ip() != peer.ip() {
        let to_tag = crate::sip::message::new_tag();
        let _ = state
            .sip_socket
            .send(peer, &crate::sip::inbound::build_488(&req, &to_tag));
        call.state = CallState::Ended;
        call.end_reason = Some(EndReason::Failed {
            detail: "SDP media address does not match the signalling peer".to_string(),
        });
        finish(state, call_id, call);
        return;
    }
    let codec = match select_inbound_codec(&state.config.media.codec, &offer) {
        Some(c) => c,
        None => {
            let to_tag = crate::sip::message::new_tag();
            let _ = state
                .sip_socket
                .send(peer, &crate::sip::inbound::build_488(&req, &to_tag));
            call.state = CallState::Ended;
            call.end_reason = Some(EndReason::Failed {
                detail: "no acceptable codec offered".to_string(),
            });
            finish(state, call_id, call);
            return;
        }
    };

    let to_tag = crate::sip::message::new_tag();
    let our_contact = format!(
        "sip:{}@{}",
        state.config.sip.username,
        state.sip_socket.local_addr()
    );
    let _ = state
        .sip_socket
        .send(peer, &build_180_ringing(&req, &to_tag, &our_contact));

    let policy = *state
        .inbound_policy
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let ring_timeout = Duration::from_secs(state.config.call.ring_timeout_secs as u64);
    let ring_deadline = Instant::now() + ring_timeout;

    enum Decision {
        Answer,
        Reject(u16),
        RingTimeout,
        Cancelled(SipRequest),
    }

    let decision = match policy.mode {
        InboundMode::Reject => Decision::Reject(policy.reject_status),
        InboundMode::Answer => match crate::sip::inbound::wait_or_cancel(
            &state.sip_socket,
            &call_id_hdr,
            Duration::from_millis(policy.answer_delay_ms as u64),
        ) {
            crate::sip::inbound::WaitOutcome::TimedOut => Decision::Answer,
            crate::sip::inbound::WaitOutcome::Cancelled(cancel_req) => {
                Decision::Cancelled(cancel_req)
            }
        },
        InboundMode::Manual => loop {
            if let Some(d) = state
                .manual_decisions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&call_id)
            {
                break match d {
                    ManualDecision::Answer => Decision::Answer,
                    ManualDecision::Reject(status) => Decision::Reject(status),
                };
            }
            if Instant::now() >= ring_deadline {
                break Decision::RingTimeout;
            }
            match crate::sip::inbound::wait_or_cancel(
                &state.sip_socket,
                &call_id_hdr,
                Duration::from_millis(200),
            ) {
                crate::sip::inbound::WaitOutcome::TimedOut => continue,
                crate::sip::inbound::WaitOutcome::Cancelled(cancel_req) => {
                    break Decision::Cancelled(cancel_req)
                }
            }
        },
    };

    match decision {
        Decision::Cancelled(cancel_req) => {
            let _ = state.sip_socket.send(
                peer,
                &gsm_sip_bridge::ims::sip_client::build_uas_response(
                    200,
                    "OK",
                    &cancel_req,
                    None,
                    None,
                    None,
                ),
            );
            let _ = state.sip_socket.send(
                peer,
                &gsm_sip_bridge::ims::sip_client::build_uas_response(
                    487,
                    "Request Terminated",
                    &req,
                    Some(&to_tag),
                    None,
                    None,
                ),
            );
            call.state = CallState::Ended;
            call.end_reason = Some(EndReason::CallerCancelled);
            finish(state, call_id, call);
        }
        Decision::RingTimeout => {
            let _ = state.sip_socket.send(
                peer,
                &crate::sip::inbound::build_reject(&req, &to_tag, policy.reject_status),
            );
            call.state = CallState::Ended;
            call.end_reason = Some(EndReason::RingTimeout);
            finish(state, call_id, call);
        }
        Decision::Reject(status) => {
            let _ = state.sip_socket.send(
                peer,
                &crate::sip::inbound::build_reject(&req, &to_tag, status),
            );
            call.state = CallState::Ended;
            call.end_reason = Some(EndReason::Rejected { status });
            finish(state, call_id, call);
        }
        Decision::Answer => {
            let rtp_port = pick_rtp_port(
                state.config.media.rtp_port_min,
                state.config.media.rtp_port_max,
            );
            let local_rtp = SocketAddr::new(state.sip_socket.local_ip, rtp_port);
            let session_id: u64 = rand::random();
            let sdp_body =
                crate::sdp::build_offer(state.sip_socket.local_ip, rtp_port, session_id, codec);

            let send_200 = || {
                let _ = state.sip_socket.send(
                    peer,
                    &build_200_ok_invite(&req, &to_tag, &our_contact, &sdp_body),
                );
            };
            send_200();

            let acked =
                crate::sip::inbound::wait_for_ack(&state.sip_socket, &call_id_hdr, send_200);
            if !acked {
                call.state = CallState::Ended;
                call.end_reason = Some(EndReason::Failed {
                    detail: "no ACK received for our 200 OK".to_string(),
                });
                finish(state, call_id, call);
                return;
            }

            call.state = CallState::Answered;
            let duration = Duration::from_secs(state.config.inbound.duration_secs as u64);

            let recording_dir = &state.config.media.recording_dir;
            let _ = std::fs::create_dir_all(recording_dir);
            let sent_wav_path = state
                .config
                .media
                .record
                .then(|| recording_dir.join(format!("{}-sent.wav", call_id.0)));
            let received_wav_path = state
                .config
                .media
                .record
                .then(|| recording_dir.join(format!("{}-received.wav", call_id.0)));

            let tone_enabled = state.config.media.tone_plan != "silence";
            let stop = Arc::new(AtomicBool::new(false));
            let media_result = crate::media::session::run(
                crate::media::session::MediaSessionConfig {
                    local_rtp,
                    remote_rtp: offer.remote_rtp,
                    codec,
                    duration,
                    sent_wav_path: sent_wav_path.clone(),
                    received_wav_path: received_wav_path.clone(),
                    tone_enabled,
                },
                stop,
            );

            // We initiate the BYE at the end of the configured duration —
            // mirroring outbound's own scope (an early BYE from the far end
            // mid-call is not specially detected here either; the call runs
            // for its full configured duration regardless).
            send_our_bye(state, &to_tag, &call_id_hdr, peer, &caller_id);

            let media_result = match media_result {
                Ok(r) => r,
                Err(e) => {
                    call.state = CallState::Ended;
                    call.end_reason = Some(EndReason::Failed {
                        detail: e.to_string(),
                    });
                    finish(state, call_id, call);
                    return;
                }
            };

            let packets = media_stats::verdict(
                media_result.sent_packets,
                media_result.receive_stats.received_packets,
                media_stats::DEFAULT_ONE_WAY_THRESHOLD_PERCENT,
            );
            let signalling = SignallingTimings {
                invite_to_180_ms: Some(start.elapsed().as_millis() as u64),
                invite_to_200_ms: Some(start.elapsed().as_millis() as u64),
                answer_to_first_rtp_ms: None,
                final_status: Some(200),
            };
            let media_counters = MediaCounters::new(
                codec,
                media_result.sent_packets,
                &media_result.receive_stats,
            )
            .with_tone_and_level(&media_result.level, &media_result.tone);
            let recordings = Recordings {
                received: received_wav_path.map(|p| p.display().to_string()),
                sent: sent_wav_path.map(|p| p.display().to_string()),
            };
            call.report = Some(CallReport::build(
                signalling,
                media_counters,
                true,
                packets,
                require,
                recordings,
            ));
            call.state = CallState::Ended;
            call.end_reason = Some(EndReason::DurationElapsed);
            finish(state, call_id, call);
        }
    }
}

/// Sends the BYE that ends a call we answered. `to`/`from` are RFC 3261
/// in-dialog role-swapped: the caller's original `From` (with their tag)
/// becomes our `To`; our own identity plus the `to_tag` we minted when
/// answering becomes our `From`.
fn send_our_bye(
    state: &SharedState,
    to_tag: &str,
    call_id: &str,
    peer: SocketAddr,
    caller_id: &CallerId,
) {
    let from = format!(
        "<sip:{}@{}>;tag={}",
        state.config.sip.username,
        state.sip_socket.local_addr(),
        to_tag
    );
    let to = caller_id.from.clone().unwrap_or_default();
    let request_uri = format!("sip:agent@{peer}");
    let branch = crate::sip::message::new_branch();
    let bye = gsm_sip_bridge::ims::sip_client::build_bye(&ByeRequest {
        request_uri: &request_uri,
        route_headers: &[],
        via_transport: "UDP",
        local_addr: state.sip_socket.local_addr(),
        from: &from,
        to: &to,
        call_id,
        cseq: 1,
        branch: &branch,
    });
    let _ = state.sip_socket.send(peer, &bye);
}

fn finish(state: &SharedState, call_id: CallId, call: Call) {
    let success = call.report.as_ref().map(|r| r.success).unwrap_or(false);
    if let Some((evicted_id, paths)) = state
        .calls
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .upsert(call)
    {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
        tracing::debug!(evicted = %evicted_id, "call evicted past retention cap");
    }
    state.events.publish(
        "call_ended",
        serde_json::json!({"call_id": call_id.0, "success": success}),
    );
}
