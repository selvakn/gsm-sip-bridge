mod common;

// User Story 4 (specs/014-vowifi-metrics-restore): registration state,
// tunnel state, and bridge-failure reasons must be visible without reading
// logs. Drives the real `ims::observability::AgentObservability` — the same
// component `ims::agent::run_inner`/`dispatch_loop` call at every
// registration attempt and bridge-failure outcome — over a real control
// socket into the real registry.

use gsm_sip_bridge::control::protocol::{AgentKind, CallStatus};
use gsm_sip_bridge::control::protocol::{BridgeFailureReason, RegistrationStatus};
use gsm_sip_bridge::control::server::start_control_server;
use gsm_sip_bridge::ims::observability::{map_bridge_failure_reason, AgentObservability};
use gsm_sip_bridge::metrics;
use gsm_sip_bridge::observability::reporter::Reporter;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

async fn wait_for<F: Fn() -> bool>(check: F, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if check() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("condition not met within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_registration_and_tunnel_gauges_and_bridge_failure_reasons() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir
        .path()
        .join("health-metrics-test.sock")
        .to_str()
        .unwrap()
        .to_string();

    let (cmd_tx, _cmd_rx) = mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = start_control_server(&socket_path, cmd_tx, shutdown_rx).await;

    let module_id = "test-health-metrics".to_string();
    let reporter = Reporter::spawn(
        socket_path.clone(),
        AgentKind::Ims,
        module_id.clone(),
        Duration::from_millis(50),
    );
    let obs = AgentObservability::new(
        reporter,
        module_id.clone(),
        None,
        "sip:100@pbx:5060".to_string(),
    );

    // Registration success brings both gauges up.
    obs.report_registration_attempt(RegistrationStatus::Success);
    obs.set_registered(true);
    obs.set_tunnel_up(true);
    wait_for(
        || {
            metrics::VOWIFI_REGISTERED.with_label_values(&[&module_id]).get() == 1.0
                && metrics::VOWIFI_TUNNEL_UP.with_label_values(&[&module_id]).get() == 1.0
        },
        Duration::from_secs(5),
    )
    .await;
    let success_count = metrics::VOWIFI_REGISTRATIONS_TOTAL
        .with_label_values(&[&module_id, "success"])
        .get();
    assert!(success_count >= 1.0);

    // A registration failure brings both back down and counts the attempt.
    obs.report_registration_attempt(RegistrationStatus::AuthFailed);
    obs.set_registered(false);
    obs.set_tunnel_up(false);
    wait_for(
        || {
            metrics::VOWIFI_REGISTERED.with_label_values(&[&module_id]).get() == 0.0
                && metrics::VOWIFI_TUNNEL_UP.with_label_values(&[&module_id]).get() == 0.0
        },
        Duration::from_secs(5),
    )
    .await;
    let auth_failed_count = metrics::VOWIFI_REGISTRATIONS_TOTAL
        .with_label_values(&[&module_id, "auth_failed"])
        .get();
    assert!(auth_failed_count >= 1.0);

    // A bridge failure is attributed to a specific, bounded reason — never
    // an unattributed bucket (SC-005).
    let ring_timeout_before = metrics::VOWIFI_BRIDGE_FAILURES_TOTAL
        .with_label_values(&[&module_id, "ring_timeout"])
        .get();
    obs.report_call_not_answered(
        CallStatus::Missed,
        BridgeFailureReason::RingTimeout,
        "+15550001111",
        chrono::Utc::now(),
    );
    wait_for(
        || {
            metrics::VOWIFI_BRIDGE_FAILURES_TOTAL
                .with_label_values(&[&module_id, "ring_timeout"])
                .get()
                == ring_timeout_before + 1.0
        },
        Duration::from_secs(5),
    )
    .await;

    let _ = shutdown_tx.send(true);
    handle.abort();
}

#[test]
fn test_map_bridge_failure_reason_is_exposed_and_bounded() {
    // Every mapped value must be one of the five closed reasons (FR-014) —
    // exercised more exhaustively in ims::observability's own unit tests;
    // this just confirms the function is reachable from outside the crate
    // the way ims::agent uses it.
    assert_eq!(
        map_bridge_failure_reason("pbx_no_answer"),
        BridgeFailureReason::RingTimeout
    );
    assert_eq!(
        map_bridge_failure_reason("never_seen_before"),
        BridgeFailureReason::BridgeSetupFailed
    );
}
