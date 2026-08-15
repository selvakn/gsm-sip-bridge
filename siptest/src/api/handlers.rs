use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::state::{AppState, Lookup};
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
