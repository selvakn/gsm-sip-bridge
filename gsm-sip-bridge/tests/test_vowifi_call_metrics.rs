mod common;

// User Story 1 (specs/014-vowifi-metrics-restore): inbound VoWiFi calls must
// move the same call-metric panels circuit-switched calls already use. This
// drives `ims::observability::AgentObservability` — the real component
// `ims::agent`'s dispatch loop calls into for every call outcome — through
// an answered call and a declined call, over a real control socket, into
// the real Prometheus registry. It intentionally does not fake the SIP/RTP
// carrier stack around it (that plumbing is covered by ims::sip_client's
// and ims::call's own tests) — what's under test here is specifically
// whether a call outcome becomes a correct, scraped metric and history row.

use gsm_sip_bridge::control::protocol::{AgentKind, BridgeFailureReason, CallStatus};
use gsm_sip_bridge::control::server::start_control_server;
use gsm_sip_bridge::ims::observability::AgentObservability;
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
async fn test_answered_and_declined_calls_produce_correct_call_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir
        .path()
        .join("call-metrics-test.sock")
        .to_str()
        .unwrap()
        .to_string();

    let (cmd_tx, _cmd_rx) = mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = start_control_server(&socket_path, cmd_tx, shutdown_rx).await;

    let module_id = "test-call-metrics".to_string();
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
        gsm_sip_bridge::store::Transport::Vowifi,
    );

    // Acceptance Scenario 1: an inbound call is answered, then ends.
    obs.set_active_calls(1);
    let answered_counter =
        metrics::CALLS_TOTAL.with_label_values(&[&module_id, "answered", "vowifi"]);
    wait_for(
        || {
            metrics::ACTIVE_CALLS
                .with_label_values(&[&module_id, "vowifi"])
                .get()
                == 1.0
        },
        Duration::from_secs(5),
    )
    .await;

    obs.report_call_answered_and_ended(
        "+15551234567",
        chrono::Utc::now(),
        42.0,
        gsm_sip_bridge::ims::media_stats::DirectionVerdict::BothWays,
    );
    obs.set_active_calls(0);
    wait_for(|| answered_counter.get() == 1.0, Duration::from_secs(5)).await;
    wait_for(
        || {
            metrics::ACTIVE_CALLS
                .with_label_values(&[&module_id, "vowifi"])
                .get()
                == 0.0
        },
        Duration::from_secs(5),
    )
    .await;

    let duration_sum = metrics::CALL_DURATION_SECONDS
        .with_label_values(&[&module_id, "vowifi"])
        .get_sample_sum();
    assert!(
        duration_sum >= 42.0,
        "answered call's duration must land in the histogram: {duration_sum}"
    );

    // Acceptance Scenario 2: an inbound call arrives but cannot be bridged.
    let failed_counter = metrics::CALLS_TOTAL.with_label_values(&[&module_id, "failed", "vowifi"]);
    let bridge_failures =
        metrics::VOWIFI_BRIDGE_FAILURES_TOTAL.with_label_values(&[&module_id, "pbx_declined"]);
    obs.report_call_not_answered(
        CallStatus::Failed,
        BridgeFailureReason::PbxDeclined,
        "+15559876543",
        chrono::Utc::now(),
    );
    wait_for(|| failed_counter.get() == 1.0, Duration::from_secs(5)).await;
    assert!(
        bridge_failures.get() >= 1.0,
        "the declined call must also be attributed a bridge-failure reason"
    );
    // active_calls must stay at 0 — this call never bridged, so it never
    // incremented it in the first place.
    assert_eq!(
        metrics::ACTIVE_CALLS
            .with_label_values(&[&module_id, "vowifi"])
            .get(),
        0.0
    );

    let _ = shutdown_tx.send(true);
    handle.abort();
}
