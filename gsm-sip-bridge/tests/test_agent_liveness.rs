mod common;

// Agent liveness (specs/014-vowifi-metrics-restore, FR-021/FR-021a/FR-021b,
// SC-009/SC-010): a report delivered over a real control socket must make
// `metrics::ingest::evaluate_liveness` — the same function
// `metrics::server`'s scrape handler calls on every request — report the
// agent as up; once reports stop, the same evaluation (given the
// threshold it would compute from a short report interval) must report it
// down, within one interval, with no call or SMS needed to trigger it.

use gsm_sip_bridge::control::protocol::AgentKind;
use gsm_sip_bridge::control::server::start_control_server;
use gsm_sip_bridge::metrics::ingest::evaluate_liveness;
use gsm_sip_bridge::observability::reporter::Reporter;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// Polls until `evaluate_liveness(threshold)` reports the given agent's
/// `up` state as `expected_up`, instead of a single fixed-delay check —
/// avoids flakiness under parallel-test-thread CPU contention.
async fn wait_for_liveness(
    agent: AgentKind,
    threshold: Duration,
    expected_up: bool,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(state) = evaluate_liveness(threshold)
            .into_iter()
            .find(|s| s.agent == agent)
        {
            if state.up == expected_up {
                return;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("liveness for {agent:?} did not reach up={expected_up} within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_reporting_agent_is_up_then_goes_stale_after_missed_heartbeats() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir
        .path()
        .join("liveness-test.sock")
        .to_str()
        .unwrap()
        .to_string();

    let (cmd_tx, _cmd_rx) = mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    // No readiness probe needed — see the comment in
    // test_observability_ingest.rs's start_test_server: bind() is
    // synchronous, so the socket is already listening once this returns.
    let handle = start_control_server(&socket_path, cmd_tx, shutdown_rx).await;

    let module_id = "test-liveness-agent".to_string();
    let report_interval = Duration::from_millis(80);
    let reporter = Reporter::spawn(
        socket_path.clone(),
        AgentKind::Ims,
        module_id.clone(),
        report_interval,
    );
    // 3x the interval, per research.md §R5 — the same multiplier
    // metrics::server::serve derives its staleness threshold with.
    let staleness_threshold = report_interval * 3;

    // No explicit report() call: the reporter's own heartbeat ticker
    // (FR-021) is what must produce the first delivered report.
    wait_for_liveness(
        AgentKind::Ims,
        staleness_threshold,
        true,
        Duration::from_secs(5),
    )
    .await;

    let states = evaluate_liveness(staleness_threshold);
    let ims = states
        .iter()
        .find(|s| s.agent == AgentKind::Ims)
        .expect("ims agent must have an entry");
    assert_eq!(ims.module_id, module_id);

    // Stop the agent (drop the reporter, closing its channel and ending its
    // thread) and let more than the staleness window pass with no further
    // reports arriving — the scenario a crashed Agent A produces.
    drop(reporter);
    wait_for_liveness(
        AgentKind::Ims,
        staleness_threshold,
        false,
        staleness_threshold + Duration::from_secs(5),
    )
    .await;

    let _ = shutdown_tx.send(true);
    handle.abort();
}
