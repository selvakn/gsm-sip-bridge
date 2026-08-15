//! Shared call identity and lifecycle types, used by both the outbound/inbound
//! dialog FSMs and the API layer. `CallReport` (the verdict bundle) lives in
//! [`crate::media::report`] since it is produced by the media layer.

use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gsm_sip_bridge::ims::media_stats::{self, ReceiveStats};
use serde::Serialize;

use crate::api::state::SharedState;
use crate::error::{SipTestError, SipTestResult};
use crate::media::codec::PCMU;
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
) -> SipTestResult<Call> {
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
    let codec = PCMU; // MVP: PCMU only; G.722 lands later (research.md R7)
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

    let stop = Arc::new(AtomicBool::new(false));
    let media_result = crate::media::session::run(
        crate::media::session::MediaSessionConfig {
            local_rtp,
            remote_rtp,
            codec,
            duration,
            sent_wav_path: sent_wav_path.clone(),
            received_wav_path: received_wav_path.clone(),
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
    );
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
