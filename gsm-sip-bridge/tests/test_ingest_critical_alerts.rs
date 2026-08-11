//! specs/022-discord-critical-alerts — end-to-end proof for the Greptile P1/
//! P2 fixes on the PR review (2026-07-27):
//!
//! - P1 "Failed delivery suppresses incident": a failed Discord delivery
//!   must not leave the incident permanently un-alertable — the next
//!   unhealthy `AgentReport` must retry it.
//! - P2 "Scrape timing loses incidents": alert evaluation must happen when
//!   a real `AgentReport` arrives, not only when something happens to hit
//!   `/metrics` — this test never touches the metrics HTTP server at all.
//!
//! `metrics::ingest::init_alerts` is a process-wide `OnceLock` (first call
//! wins), so this scenario is deliberately one long-lived `#[tokio::test]`
//! in its own binary rather than several small ones that would race over
//! the same global config.

use gsm_sip_bridge::alerts::discord::DiscordClient;
use gsm_sip_bridge::config::secret::Secret;
use gsm_sip_bridge::config::{
    AlertsConfig, CategoryAlertConfig, GmConnectionLostThresholds, ModuleLifecycleThresholds,
    RegistrationLossThresholds, TunnelFailureThresholds,
};
use gsm_sip_bridge::control::protocol::{AgentKind, AgentReport, AgentState};
use gsm_sip_bridge::metrics::ingest;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn enabled(webhook_override: Option<&str>) -> CategoryAlertConfig {
    CategoryAlertConfig {
        enabled: true,
        webhook_url_override: webhook_override.map(|s| Secret::new(s.to_string())),
    }
}

fn report(module_id: &str, seq: u64, registered: bool) -> AgentReport {
    AgentReport {
        agent: AgentKind::Ims,
        module_id: module_id.to_string(),
        epoch: 1,
        seq,
        state: AgentState {
            registered: Some(registered),
            ..AgentState::default()
        },
        events: vec![],
        dropped: 0,
    }
}

/// A report that only carries `gm_connection_up` (specs/028), for the
/// Gm-connection alert phases. Registration is left unreported so it never
/// interacts with the registration-loss episode above.
fn gm_report(module_id: &str, seq: u64, gm_connection_up: bool) -> AgentReport {
    AgentReport {
        agent: AgentKind::Ims,
        module_id: module_id.to_string(),
        epoch: 1,
        seq,
        state: AgentState {
            gm_connection_up: Some(gm_connection_up),
            ..AgentState::default()
        },
        events: vec![],
        dropped: 0,
    }
}

async fn settle() {
    // The dispatch is a fire-and-forget tokio::spawn from inside
    // apply_report; give it a moment to reach the local wiremock server.
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn failed_delivery_is_retried_then_succeeds_then_recovers() {
    let server = MockServer::start().await;
    let webhook = format!("{}/webhook", server.uri());

    // unhealthy_sec = 0: the very first unhealthy report already satisfies
    // "elapsed >= threshold", so the scenario runs in milliseconds instead
    // of needing a real multi-minute wait.
    ingest::init_alerts(
        AlertsConfig {
            default_webhook_url: Secret::new(String::new()),
            instance_name: None,
            sms: enabled(None),
            module_lifecycle: CategoryAlertConfig {
                enabled: false,
                webhook_url_override: None,
            },
            registration_loss: enabled(Some(&webhook)),
            tunnel_failure: CategoryAlertConfig {
                enabled: false,
                webhook_url_override: None,
            },
            missed_call: CategoryAlertConfig {
                enabled: false,
                webhook_url_override: None,
            },
            line_discovery_failed: CategoryAlertConfig {
                enabled: false,
                webhook_url_override: None,
            },
            // specs/028: enabled and threshold 0 so the gm-connection phase
            // below fires on the first unhealthy report, same as registration.
            gm_connection_lost: enabled(Some(&webhook)),
            module_lifecycle_thresholds: ModuleLifecycleThresholds {
                at_worker_unresponsive_sec: 60,
            },
            tunnel_failure_thresholds: TunnelFailureThresholds { unhealthy_sec: 300 },
            registration_loss_thresholds: RegistrationLossThresholds { unhealthy_sec: 0 },
            gm_connection_lost_thresholds: GmConnectionLostThresholds { unhealthy_sec: 0 },
        },
        DiscordClient::new(Secret::new(String::new()), "test-instance".to_string()).unwrap(),
        std::collections::HashMap::new(),
    );

    let module_id = "test-e2e-retry";

    // Phase 1: Discord is down (400 — post_with_retry gives up immediately,
    // no internal retry, keeping this test fast). The Failure alert must be
    // attempted...
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(400))
        .expect(1)
        .mount(&server)
        .await;
    ingest::apply_report(&report(module_id, 1, false));
    settle().await;
    server.verify().await;

    // ...and after the failed attempt, still-unhealthy reports must not be
    // permanently suppressed — this is the exact P1 bug: without the fix,
    // record_alert_outcome never resets Pending back to Idle on failure, so
    // this second attempt would never fire.
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    ingest::apply_report(&report(module_id, 2, false));
    settle().await;
    server.verify().await;

    // Phase 2: now delivered successfully. A further still-unhealthy report
    // must NOT fire a third attempt (FR-013 — suppressed while Alerted).
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&server)
        .await;
    ingest::apply_report(&report(module_id, 3, false));
    settle().await;
    server.verify().await;

    // Phase 3: recovers — exactly one Recovered notice.
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    ingest::apply_report(&report(module_id, 4, true));
    settle().await;
    server.verify().await;

    // --- specs/028-gm-tcp-reconnect: the Gm-connection category pairs a
    // failure and a recovery through the very same AlertPhase machine, on an
    // independent module so the two episodes never overlap. ---
    let gm_module = "test-e2e-gm";

    // A down connection fires exactly one Failure alert.
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    ingest::apply_report(&gm_report(gm_module, 1, false));
    settle().await;
    server.verify().await;

    // A further still-down report must NOT fire a second alert (FR-015 —
    // suppressed while Alerted).
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&server)
        .await;
    ingest::apply_report(&gm_report(gm_module, 2, false));
    settle().await;
    server.verify().await;

    // Recovery fires exactly one paired Recovered notice (FR-016).
    server.reset().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    ingest::apply_report(&gm_report(gm_module, 3, true));
    settle().await;
    server.verify().await;
}
