use crate::metrics::ingest;
use axum::{http::StatusCode, response::IntoResponse, routing::get, Router};
use prometheus::TextEncoder;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

static START_TIME: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
/// 3x `[metrics].agent_report_interval_seconds` (research.md §R5): one
/// missed heartbeat of tolerance before an agent is declared down.
static AGENT_STALENESS_THRESHOLD: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();

pub fn record_start_time() {
    START_TIME.get_or_init(Instant::now);
}

fn staleness_threshold() -> Duration {
    *AGENT_STALENESS_THRESHOLD.get_or_init(|| Duration::from_secs(30))
}

/// Evaluated on every scrape (FR-021a) rather than on a timer, mirroring how
/// `UPTIME_SECONDS` is already refreshed here: a silent VoWiFi agent's
/// `AGENT_UP` and the gauges it owns must read correctly even if the daemon
/// itself only just restarted and has never seen a report at all.
fn refresh_agent_liveness() {
    for state in ingest::evaluate_liveness(staleness_threshold()) {
        super::AGENT_UP
            .with_label_values(&[state.agent.as_str(), &state.module_id])
            .set(if state.up { 1.0 } else { 0.0 });
        super::AGENT_LAST_REPORT_SECONDS
            .with_label_values(&[state.agent.as_str(), &state.module_id])
            .set(state.age_seconds);

        if !state.up {
            super::ACTIVE_CALLS
                .with_label_values(&[&state.module_id, "vowifi"])
                .set(0.0);
            if state.agent == crate::control::protocol::AgentKind::Ims {
                super::VOWIFI_REGISTERED
                    .with_label_values(&[&state.module_id])
                    .set(0.0);
                super::VOWIFI_TUNNEL_UP
                    .with_label_values(&[&state.module_id])
                    .set(0.0);
            }
        }
    }
}

async fn metrics_handler() -> impl IntoResponse {
    if let Some(start) = START_TIME.get() {
        super::UPTIME_SECONDS.set(start.elapsed().as_secs_f64());
    }
    refresh_agent_liveness();

    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();

    match encoder.encode_to_string(&metric_families) {
        Ok(output) => (
            StatusCode::OK,
            [("Content-Type", "text/plain; version=0.0.4")],
            output,
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "failed to encode metrics");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn serve(
    port: u16,
    agent_report_interval_seconds: u64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    AGENT_STALENESS_THRESHOLD
        .get_or_init(|| Duration::from_secs(3 * agent_report_interval_seconds));

    let app = Router::new().route("/metrics", get(metrics_handler));
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    tracing::info!(port = port, "metrics server starting");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
