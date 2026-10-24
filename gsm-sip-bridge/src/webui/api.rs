//! `/api/*` handlers (specs/122-management-webui, contracts/http-api.md).

use super::lines::{self, Inputs};
use super::queries::{CallFilter, EventFilter, Page, SmsFilter};
use super::WebuiState;
use crate::control::protocol::AgentKind;
use crate::metrics;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

/// Sum of a counter family across all its label sets.
fn counter_total(name: &str) -> f64 {
    prometheus::gather()
        .iter()
        .filter(|family| family.get_name() == name)
        .flat_map(|family| family.get_metric())
        .map(|m| m.get_counter().get_value())
        .sum()
}

/// `GET /api/health`
pub async fn health() -> Json<Value> {
    Json(json!({
        "modules_active": metrics::MODULES_ACTIVE.get() as i64,
        "modules_failed": metrics::MODULES_FAILED.get() as i64,
        "store_queue_depth": metrics::STORE_QUEUE_DEPTH.get() as i64,
        "observability_events_dropped":
            counter_total("gsm_sip_bridge_observability_events_dropped_total") as i64,
        "cs_enabled": metrics::CS_ENABLED.get() >= 1.0,
    }))
}

/// Categories and modules whose `critical_event_active` gauge reads 1.
fn firing_alerts() -> Vec<(String, String)> {
    let mut firing = Vec::new();
    for family in prometheus::gather() {
        if family.get_name() != "gsm_sip_bridge_critical_event_active" {
            continue;
        }
        for metric in family.get_metric() {
            if metric.get_gauge().get_value() < 1.0 {
                continue;
            }
            let label = |name: &str| {
                metric
                    .get_label()
                    .iter()
                    .find(|l| l.get_name() == name)
                    .map(|l| l.get_value().to_string())
                    .unwrap_or_default()
            };
            firing.push((label("category"), label("module")));
        }
    }
    firing.sort();
    firing
}

/// Alerts open now: those this process has raised (with when) together with any
/// whose `critical_event_active` gauge reads 1 (specs/122-management-webui).
/// The gauge covers an alert raised in another process, so it cannot say since
/// when.
fn active_alerts() -> Vec<Value> {
    use std::collections::BTreeMap;
    let mut open: BTreeMap<(String, String), Option<String>> = BTreeMap::new();
    for (category, module) in firing_alerts() {
        open.insert((category, module), None);
    }
    for raise in crate::store::events::open_raises() {
        let since = raise
            .since
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        open.insert(
            (raise.category, raise.module_id.unwrap_or_default()),
            Some(since),
        );
    }
    open.into_iter()
        .map(|((category, module_id), since)| {
            json!({ "category": category, "module_id": module_id, "since": since })
        })
        .collect()
}

/// Whether the PBX registration is up: `null` when there is no PBX to
/// register with (SIP-server mode, or no agent has said).
fn pbx_registered(
    state: &WebuiState,
    agents: &[crate::metrics::ingest::AgentSnapshot],
) -> Option<bool> {
    let config = &state.loaded.config;
    if config.sip_server.enabled {
        return None;
    }
    if config.cs.enabled {
        return Some(metrics::SIP_REGISTERED.get() >= 1.0);
    }
    let reports: Vec<bool> = agents
        .iter()
        .filter(|a| matches!(a.agent, AgentKind::Sip | AgentKind::VolteSip))
        .filter_map(|a| a.pbx_registered)
        .collect();
    if reports.is_empty() {
        None
    } else {
        Some(reports.contains(&true))
    }
}

/// `GET /api/overview`
pub async fn overview(State(state): State<WebuiState>) -> Json<Value> {
    let config = &state.loaded.config;
    let agents = crate::metrics::ingest::snapshot_lines();
    let pbx = pbx_registered(&state, &agents);

    let cs_slots = match (&state.cs, config.cs.enabled) {
        (Some(cs), true) => lines::list_cs_slots(cs).await.unwrap_or_default(),
        _ => Vec::new(),
    };
    let phones = lines::phone_map(config);
    let lines = lines::assemble(Inputs {
        agents,
        phones: &phones,
        cs_slots,
        cs_signal: super::signal::all(),
        signal_poll_enabled: config.metrics.signal_poll_interval_seconds > 0,
    });

    let active_alerts = active_alerts();

    let grafana = &config.webui.grafana_url;
    Json(json!({
        "bridge": {
            "version": env!("CARGO_PKG_VERSION"),
            "git_sha": option_env!("GIT_SHA"),
            "uptime_seconds": crate::metrics::server::uptime().map(|d| d.as_secs()),
            "pbx_registered": pbx,
            "grafana_url": (!grafana.is_empty()).then_some(grafana.as_str()),
        },
        "active_alerts": active_alerts,
        "discovery_note": lines.is_empty().then_some("no_lines"),
        "lines": lines,
    }))
}

fn store_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "store_unavailable" })),
    )
        .into_response()
}

/// Runs a blocking store query off the async workers.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, super::queries::StoreUnavailable> + Send + 'static,
) -> Result<T, Response> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        _ => Err(store_unavailable()),
    }
}

#[derive(Deserialize)]
pub struct SmsParams {
    line: Option<String>,
    q: Option<String>,
    page: Option<u32>,
    per_page: Option<u32>,
}

/// `GET /api/sms`
pub async fn sms(State(state): State<WebuiState>, Query(params): Query<SmsParams>) -> Response {
    let filter = SmsFilter {
        line: params.line,
        q: params.q,
    };
    let page = Page::new(params.page, params.per_page);
    let store = state.store.clone();
    match blocking(move || store.list_sms(&filter, page)).await {
        Ok(rows) => Json(rows).into_response(),
        Err(response) => response,
    }
}

/// `GET /api/calls/summary`: "today" runs from the host's local midnight.
pub async fn calls_summary(State(state): State<WebuiState>) -> Response {
    use chrono::{Duration, Local, TimeZone, Utc};
    let now = Utc::now();
    let local_midnight = Local::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|naive| Local.from_local_datetime(&naive).earliest())
        .map_or(now - Duration::hours(24), |t| t.with_timezone(&Utc));
    let store = state.store.clone();
    match blocking(move || store.call_summary(local_midnight, now - Duration::days(7))).await {
        Ok(summary) => Json(summary).into_response(),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
pub struct CallParams {
    line: Option<String>,
    direction: Option<String>,
    transport: Option<String>,
    status: Option<String>,
    from: Option<String>,
    to: Option<String>,
    page: Option<u32>,
    per_page: Option<u32>,
}

fn bad_request(what: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "bad_request", "detail": what })),
    )
        .into_response()
}

fn parse_time(value: Option<&str>) -> Result<Option<chrono::DateTime<chrono::Utc>>, ()> {
    match value.filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(v) => chrono::DateTime::parse_from_rfc3339(v)
            .map(|t| Some(t.with_timezone(&chrono::Utc)))
            .map_err(|_| ()),
    }
}

/// `GET /api/calls`
pub async fn calls(State(state): State<WebuiState>, Query(p): Query<CallParams>) -> Response {
    let Ok(from) = parse_time(p.from.as_deref()) else {
        return bad_request("from must be an RFC 3339 time");
    };
    let Ok(to) = parse_time(p.to.as_deref()) else {
        return bad_request("to must be an RFC 3339 time");
    };
    let filter = CallFilter {
        line: p.line,
        direction: p.direction,
        transport: p.transport,
        status: p.status,
        from,
        to,
    };
    let page = Page::new(p.page, p.per_page);
    let store = state.store.clone();
    match blocking(move || store.list_calls(&filter, page)).await {
        Ok(rows) => Json(rows).into_response(),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
pub struct EventParams {
    line: Option<String>,
    kind: Option<String>,
    page: Option<u32>,
    per_page: Option<u32>,
}

/// `GET /api/events`
pub async fn events(State(state): State<WebuiState>, Query(p): Query<EventParams>) -> Response {
    let filter = EventFilter {
        line: p.line,
        kind: p.kind,
    };
    let page = Page::new(p.page, p.per_page);
    let store = state.store.clone();
    match blocking(move || store.list_events(&filter, page)).await {
        Ok(rows) => Json(rows).into_response(),
        Err(response) => response,
    }
}

/// `GET /api/config`: the document as loaded at startup, secrets redacted.
pub async fn config(State(state): State<WebuiState>) -> Json<Value> {
    let loaded = &state.loaded;
    Json(json!({
        "loaded_at": loaded.loaded_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "source": loaded.source_path.display().to_string(),
        "note": "As loaded at startup; later edits to the file are not shown.",
        "document": super::config_view::redacted(&loaded.snapshot),
    }))
}
