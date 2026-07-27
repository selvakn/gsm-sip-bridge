use crate::alerts::discord::DiscordClient;
use crate::config::secret::Secret;
use crate::config::AlertsConfig;
use crate::metrics::ingest;
use axum::{http::StatusCode, response::IntoResponse, routing::get, Router};
use prometheus::TextEncoder;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

static START_TIME: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
/// 3x `[metrics].agent_report_interval_seconds` (research.md §R5): one
/// missed heartbeat of tolerance before an agent is declared down.
static AGENT_STALENESS_THRESHOLD: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
/// specs/022-discord-critical-alerts (US2/US3): set once in `serve`, read on
/// every scrape by `refresh_critical_alerts`. `metrics_handler` already runs
/// inside the daemon's one tokio runtime, so — unlike
/// `supervise::orchestrate` — no dedicated `Runtime` is needed here; alerts
/// are fired with a plain `tokio::spawn`.
static ALERTS_CONFIG: std::sync::OnceLock<AlertsConfig> = std::sync::OnceLock::new();
static ALERTS_CLIENT: std::sync::OnceLock<DiscordClient> = std::sync::OnceLock::new();

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

/// specs/022-discord-critical-alerts FR-005/FR-006 (US2/US3). Same
/// "evaluated on every scrape, not a timer" shape as
/// `refresh_agent_liveness` above.
fn refresh_critical_alerts() {
    let (Some(config), Some(client)) = (ALERTS_CONFIG.get(), ALERTS_CLIENT.get()) else {
        return;
    };
    let registration_threshold =
        Duration::from_secs(config.registration_loss_thresholds.unhealthy_sec);
    let tunnel_threshold = Duration::from_secs(config.tunnel_failure_thresholds.unhealthy_sec);

    for event in ingest::evaluate_critical_alerts(registration_threshold, tunnel_threshold) {
        let client = client.clone();
        let config = config.clone();
        tokio::spawn(async move { crate::alerts::dispatch(&client, &config, event).await });
    }
}

async fn metrics_handler() -> impl IntoResponse {
    if let Some(start) = START_TIME.get() {
        super::UPTIME_SECONDS.set(start.elapsed().as_secs_f64());
    }
    refresh_agent_liveness();
    refresh_critical_alerts();

    let encoder = TextEncoder::new();
    // Both registries, not just the default one.
    //
    // `prometheus::gather()` collects the *default* registry. The host-side
    // LTE gauges (`gsm_bridge_volte_*`) register into `metrics::REGISTRY`
    // instead, so for as long as they have existed they have been invisible
    // to every scrape — set faithfully by the code, collected by nobody.
    // Found live (specs/017 research R16) while checking that this path's
    // health was distinguishable from the Wi-Fi path's: it was not, because
    // it was not published at all.
    //
    // This is the failure `sms::record_and_forward` already warns about in
    // its own doc comment — "would land in a Prometheus registry nothing ever
    // reads" — reached by a different route.
    let mut metric_families = prometheus::gather();
    metric_families.extend(super::REGISTRY.gather());

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
    alerts_config: AlertsConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    AGENT_STALENESS_THRESHOLD
        .get_or_init(|| Duration::from_secs(3 * agent_report_interval_seconds));
    ALERTS_CONFIG.get_or_init(|| alerts_config);
    match DiscordClient::new(Secret::new(String::new())) {
        Ok(client) => {
            ALERTS_CLIENT.get_or_init(|| client);
        }
        Err(e) => tracing::error!(error = %e, "failed to create critical-alerts Discord client"),
    }

    let app = Router::new().route("/metrics", get(metrics_handler));
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    tracing::info!(port = port, "metrics server starting");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
