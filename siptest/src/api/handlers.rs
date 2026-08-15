use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::state::{AppState, InboundMode, Lookup, ManualDecision};
use crate::call::{execute_outbound_call, CallId};
use crate::error::SipTestError;

pub async fn health() -> Json<serde_json::Value> {
    Json(json!({"ok": true, "version": env!("CARGO_PKG_VERSION")}))
}

#[derive(Serialize)]
pub struct StatusResponse {
    registration: serde_json::Value,
    local: serde_json::Value,
    bridge: serde_json::Value,
    active_call: Option<serde_json::Value>,
    counters: serde_json::Value,
    event_seq: u64,
}

pub async fn status(State(state): State<AppState>) -> Json<StatusResponse> {
    let reg = state.registration.lock().unwrap_or_else(|e| e.into_inner());
    let counters = state.counters.lock().unwrap_or_else(|e| e.into_inner());
    let active = state
        .calls
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .active();
    let observed = *state
        .last_outbound_observed
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    Json(StatusResponse {
        registration: json!({
            "state": format!("{:?}", reg.state).to_lowercase(),
            "granted_expires": reg.granted_expires,
            "last_status": reg.last_status,
            "consecutive_failures": reg.consecutive_failures,
        }),
        local: json!({ "sip_addr": state.local_sip_addr.to_string() }),
        bridge: json!({
            "registrar": state.bridge_registrar.to_string(),
            "outbound_observed": observed.map(|a| a.to_string()),
        }),
        active_call: active.map(|c| json!({"id": c.id.0, "direction": format!("{:?}", c.direction).to_lowercase(), "state": format!("{:?}", c.state).to_lowercase(), "peer": c.peer})),
        counters: json!({
            "calls_placed": counters.calls_placed,
            "calls_received": counters.calls_received,
            "registrations": counters.registrations,
            "errors": counters.errors,
        }),
        event_seq: state.events.current_seq(),
    })
}

#[derive(Deserialize)]
pub struct PlaceCallRequest {
    pub destination: String,
    #[serde(default)]
    pub duration_secs: Option<u64>,
    #[serde(default)]
    pub ring_timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
pub struct WaitQuery {
    #[serde(default)]
    pub wait: bool,
}

pub async fn place_call(
    State(state): State<AppState>,
    Query(q): Query<WaitQuery>,
    Json(req): Json<PlaceCallRequest>,
) -> Response {
    let duration = Duration::from_secs(
        req.duration_secs
            .unwrap_or(state.config.call.default_duration_secs as u64),
    );
    let ring_timeout = Duration::from_secs(
        req.ring_timeout_secs
            .unwrap_or(state.config.call.ring_timeout_secs as u64),
    );

    let state2 = state.clone();
    let destination = req.destination.clone();
    let result = tokio::task::spawn_blocking(move || {
        execute_outbound_call(&state2, destination, duration, ring_timeout)
    })
    .await;

    match result {
        Ok(Ok(call)) => {
            if q.wait {
                let report = call.report.clone();
                let report_text = call.report.as_ref().map(|r| r.render_text(&call.id.0));
                (
                    StatusCode::OK,
                    Json(json!({"id": call.id.0, "report": report, "report_text": report_text})),
                )
                    .into_response()
            } else {
                (
                    StatusCode::ACCEPTED,
                    Json(json!({"id": call.id.0, "state": "ended"})),
                )
                    .into_response()
            }
        }
        Ok(Err(e)) => error_response(e),
        Err(_join_err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "internal_error"})),
        )
            .into_response(),
    }
}

fn error_response(e: SipTestError) -> Response {
    let (status, code) = match &e {
        SipTestError::InvalidDestination(_) => (StatusCode::BAD_REQUEST, "invalid_destination"),
        SipTestError::DestinationNotAllowed(_) => {
            (StatusCode::FORBIDDEN, "destination_not_allowed")
        }
        SipTestError::RateLimited { .. } => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
        SipTestError::CallInProgress => (StatusCode::CONFLICT, "call_in_progress"),
        SipTestError::NotRegistered => (StatusCode::SERVICE_UNAVAILABLE, "not_registered"),
        SipTestError::CallEvicted(_) => (StatusCode::GONE, "call_evicted"),
        SipTestError::CallNotFound(_) => (StatusCode::NOT_FOUND, "call_not_found"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
    };
    let mut body = json!({"error": code, "detail": e.to_string()});
    if let SipTestError::RateLimited { retry_after_s } = &e {
        body["retry_after_s"] = json!(retry_after_s);
    }
    (status, Json(body)).into_response()
}

pub async fn list_calls(State(state): State<AppState>) -> Json<serde_json::Value> {
    let calls = state.calls.lock().unwrap_or_else(|e| e.into_inner());
    let recent = calls.recent(50);
    Json(json!(recent
        .into_iter()
        .map(|c| json!({
            "id": c.id.0,
            "direction": format!("{:?}", c.direction).to_lowercase(),
            "state": format!("{:?}", c.state).to_lowercase(),
            "peer": c.peer,
            "caller_id": {
                "from": c.caller_id.from,
                "p_asserted_identity": c.caller_id.p_asserted_identity,
                "x_gsm_caller_id": c.caller_id.x_gsm_caller_id,
            },
            "success": c.report.as_ref().map(|r| r.success),
        }))
        .collect::<Vec<_>>()))
}

pub async fn get_call(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let calls = state.calls.lock().unwrap_or_else(|e| e.into_inner());
    match calls.lookup(&CallId(id.clone())) {
        Lookup::Found(call) => {
            let report_text = call.report.as_ref().map(|r| r.render_text(&call.id.0));
            (
                StatusCode::OK,
                Json(json!({
                    "id": call.id.0,
                    "direction": format!("{:?}", call.direction).to_lowercase(),
                    "state": format!("{:?}", call.state).to_lowercase(),
                    "peer": call.peer,
                    "peer_uri": call.peer_uri,
                    "caller_id": {
                        "from": call.caller_id.from,
                        "p_asserted_identity": call.caller_id.p_asserted_identity,
                        "x_gsm_caller_id": call.caller_id.x_gsm_caller_id,
                    },
                    "end_reason": call.end_reason,
                    "report": call.report,
                    "report_text": report_text,
                })),
            )
                .into_response()
        }
        Lookup::Evicted => error_response(SipTestError::CallEvicted(id)),
        Lookup::NotFound => error_response(SipTestError::CallNotFound(id)),
    }
}

pub async fn answer_call(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let call_id = CallId(id);
    state
        .manual_decisions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(call_id, ManualDecision::Answer);
    (StatusCode::OK, Json(json!({}))).into_response()
}

#[derive(Deserialize, Default)]
pub struct RejectRequest {
    #[serde(default = "default_reject_status")]
    pub status: u16,
}

fn default_reject_status() -> u16 {
    486
}

pub async fn reject_call(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<RejectRequest>>,
) -> Response {
    let status = body
        .map(|Json(b)| b.status)
        .unwrap_or_else(default_reject_status);
    let call_id = CallId(id);
    state
        .manual_decisions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(call_id, ManualDecision::Reject(status));
    (StatusCode::OK, Json(json!({}))).into_response()
}

pub async fn get_inbound_policy(State(state): State<AppState>) -> Json<serde_json::Value> {
    let p = *state
        .inbound_policy
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    Json(json!({
        "mode": inbound_mode_str(p.mode),
        "answer_delay_ms": p.answer_delay_ms,
        "reject_status": p.reject_status,
        "duration_secs": p.duration_secs,
    }))
}

#[derive(Deserialize)]
pub struct PolicyUpdate {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub answer_delay_ms: Option<u32>,
    #[serde(default)]
    pub reject_status: Option<u16>,
    #[serde(default)]
    pub duration_secs: Option<u32>,
}

pub async fn put_inbound_policy(
    State(state): State<AppState>,
    Json(update): Json<PolicyUpdate>,
) -> Response {
    let mode = match update.mode.as_deref() {
        Some(m) => match m.parse::<InboundMode>() {
            Ok(m) => Some(m),
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "invalid_mode", "detail": e})),
                )
                    .into_response()
            }
        },
        None => None,
    };
    let mut p = state
        .inbound_policy
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(mode) = mode {
        p.mode = mode;
    }
    if let Some(d) = update.answer_delay_ms {
        p.answer_delay_ms = d;
    }
    if let Some(s) = update.reject_status {
        p.reject_status = s;
    }
    if let Some(d) = update.duration_secs {
        p.duration_secs = d;
    }
    let response = json!({
        "mode": inbound_mode_str(p.mode),
        "answer_delay_ms": p.answer_delay_ms,
        "reject_status": p.reject_status,
        "duration_secs": p.duration_secs,
    });
    drop(p);
    (StatusCode::OK, Json(response)).into_response()
}

fn inbound_mode_str(mode: InboundMode) -> &'static str {
    match mode {
        InboundMode::Answer => "answer",
        InboundMode::Reject => "reject",
        InboundMode::Manual => "manual",
    }
}

#[derive(Deserialize)]
pub struct EventsQuery {
    #[serde(default)]
    pub since: u64,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    25000
}

pub async fn recording_info(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let calls = state.calls.lock().unwrap_or_else(|e| e.into_inner());
    match calls.lookup(&CallId(id.clone())) {
        Lookup::Found(call) => {
            let (received, sent, sample_rate) = call
                .report
                .as_ref()
                .map(|r| {
                    (
                        r.recordings.received.clone(),
                        r.recordings.sent.clone(),
                        r.media.audio_hz,
                    )
                })
                .unwrap_or((None, None, 0));
            (
                StatusCode::OK,
                Json(json!({"received": received, "sent": sent, "sample_rate": sample_rate})),
            )
                .into_response()
        }
        Lookup::Evicted => error_response(SipTestError::CallEvicted(id)),
        Lookup::NotFound => error_response(SipTestError::CallNotFound(id)),
    }
}

/// `which` is `received.wav` or `sent.wav`, matching contracts/control-api.md.
pub async fn recording_file(
    State(state): State<AppState>,
    Path((id, which)): Path<(String, String)>,
) -> Response {
    let path = {
        let calls = state.calls.lock().unwrap_or_else(|e| e.into_inner());
        let call = match calls.lookup(&CallId(id.clone())) {
            Lookup::Found(call) => call,
            Lookup::Evicted => return error_response(SipTestError::CallEvicted(id)),
            Lookup::NotFound => return error_response(SipTestError::CallNotFound(id)),
        };
        let Some(report) = &call.report else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "recording_not_found"})),
            )
                .into_response();
        };
        let path = match which.as_str() {
            "received.wav" => report.recordings.received.clone(),
            "sent.wav" => report.recordings.sent.clone(),
            _ => None,
        };
        match path {
            Some(p) => p,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "recording_not_found"})),
                )
                    .into_response()
            }
        }
    };

    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "audio/wav")],
            bytes,
        )
            .into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "recording_file_missing"})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct LogTailQuery {
    #[serde(default = "default_log_lines")]
    pub lines: usize,
}

fn default_log_lines() -> usize {
    200
}

pub async fn log_tail(Query(q): Query<LogTailQuery>) -> Json<serde_json::Value> {
    Json(json!({ "lines": crate::logbuf::tail(q.lines) }))
}

async fn registration_action(state: AppState) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let mut creds = state
            .registration_creds
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::sip::registration::register(
            &state.sip_socket,
            &state.registration_config,
            &mut creds,
        )
        .inspect(|status| {
            *state.registration.lock().unwrap_or_else(|e| e.into_inner()) = status.clone();
        })
    })
    .await;
    match result {
        Ok(Ok(status)) => (
            StatusCode::OK,
            Json(json!({
                "state": format!("{:?}", status.state).to_lowercase(),
                "granted_expires": status.granted_expires,
            })),
        )
            .into_response(),
        Ok(Err(e)) => error_response(e),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "internal_error"})),
        )
            .into_response(),
    }
}

pub async fn force_register(State(state): State<AppState>) -> Response {
    registration_action(state).await
}

pub async fn force_refresh(State(state): State<AppState>) -> Response {
    registration_action(state).await
}

pub async fn force_deregister(State(state): State<AppState>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let mut creds = state
            .registration_creds
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::sip::registration::deregister(
            &state.sip_socket,
            &state.registration_config,
            &mut creds,
        );
        *state.registration.lock().unwrap_or_else(|e| e.into_inner()) =
            crate::sip::registration::RegistrationStatus::default();
    })
    .await;
    match result {
        Ok(()) => (StatusCode::OK, Json(json!({}))).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "internal_error"})),
        )
            .into_response(),
    }
}

pub async fn events(
    State(state): State<AppState>,
    Query(q): Query<EventsQuery>,
) -> Json<serde_json::Value> {
    let timeout = Duration::from_millis(q.timeout_ms.min(60_000));
    let events = tokio::task::spawn_blocking(move || state.events.since(q.since, timeout))
        .await
        .unwrap_or_default();
    Json(json!(events))
}
