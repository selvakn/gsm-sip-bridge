// End-to-end check that the three distinguishable outbound-attempt outcomes
// (specs/025-outbound-calling, US5/SC-005: "no idle line", "network
// refused", "unanswered") really are distinguishable from metrics alone —
// a real `Observe` command over a real Unix control socket, applied to the
// real Prometheus registry, then read back through the real scrape
// handler's encoding path, exactly mirroring test_observability_ingest.rs's
// own no-mocks approach for the rest of the observability wire protocol.
//
// This exercises the ingest → metric path with the real `OutboundAttemptOutcome`
// enum (not a reimplementation of it); the logic that decides *which*
// outcome a given failure maps to is unit-tested separately, close to the
// code that makes the decision (`vowifi::mod`'s
// `committed_failure_outcome_distinguishes_unanswered_from_refused` and
// friends) — a real end-to-end call needs real hardware (pjsua + a modem
// or a live VoWiFi/VoLTE line), which this test suite does not have
// available, matching the same scope adjustment already made for T017e/T025.

use gsm_sip_bridge::control::client::send_cmd;
use gsm_sip_bridge::control::protocol::{
    AgentKind, AgentReport, AgentState, ControlCmd, ObservedEvent, OutboundAttemptOutcome,
};
use gsm_sip_bridge::control::server::start_control_server;
use gsm_sip_bridge::metrics;
use tokio::sync::{mpsc, watch};

async fn start_test_server() -> (String, tokio::task::JoinHandle<()>, watch::Sender<bool>) {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir
        .path()
        .join("outbound-diagnostics-test.sock")
        .to_str()
        .unwrap()
        .to_string();
    std::mem::forget(dir); // keep the tempdir alive for the socket's lifetime

    let (cmd_tx, _cmd_rx) = mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = start_control_server(&socket_path, cmd_tx, shutdown_rx).await;
    (socket_path, handle, shutdown_tx)
}

async fn report_one_outbound_attempt(
    socket_path: &str,
    module_id: &str,
    outcome: OutboundAttemptOutcome,
) {
    let report = AgentReport {
        agent: AgentKind::Sip,
        module_id: module_id.to_string(),
        epoch: 1,
        seq: 1,
        state: AgentState::default(),
        events: vec![ObservedEvent::OutboundAttempt { outcome }],
        dropped: 0,
    };
    let sock = socket_path.to_string();
    let resp =
        tokio::task::spawn_blocking(move || send_cmd(&sock, &ControlCmd::Observe { report }))
            .await
            .unwrap()
            .unwrap();
    assert!(matches!(
        resp,
        gsm_sip_bridge::control::protocol::ControlResp::Ok
    ));
}

/// The three outcomes SC-005 cares about being distinguishable
/// ("no idle line", "network refused", "unanswered") must each increment
/// their own, separately-labeled `gsm_sip_bridge_outbound_attempts_total`
/// series — not collapse into one generic "failed" bucket the way the
/// pre-fix VoWiFi/VoLTE path used to (second code review, 2026-08-03: FR-012
/// unimplemented, `Unanswered` declared but never emitted).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_three_diagnostic_outcomes_are_separately_countable() {
    let (socket_path, handle, shutdown_tx) = start_test_server().await;

    // Baseline first: `OUTBOUND_ATTEMPTS_TOTAL` has no per-call label (only
    // `outcome`), so it accumulates across whatever else already ran in
    // this process before this test — diff against a captured baseline
    // rather than asserting an absolute count.
    let before_no_idle_line = metrics::OUTBOUND_ATTEMPTS_TOTAL
        .with_label_values(&["refused_no_idle_line"])
        .get();
    let before_network_failure = metrics::OUTBOUND_ATTEMPTS_TOTAL
        .with_label_values(&["refused_network_failure"])
        .get();
    let before_unanswered = metrics::OUTBOUND_ATTEMPTS_TOTAL
        .with_label_values(&["unanswered"])
        .get();

    report_one_outbound_attempt(
        &socket_path,
        "test-diag-no-idle-line",
        OutboundAttemptOutcome::RefusedNoIdleLine,
    )
    .await;
    report_one_outbound_attempt(
        &socket_path,
        "test-diag-network-failure",
        OutboundAttemptOutcome::RefusedNetworkFailure,
    )
    .await;
    report_one_outbound_attempt(
        &socket_path,
        "test-diag-unanswered",
        OutboundAttemptOutcome::Unanswered,
    )
    .await;

    assert_eq!(
        metrics::OUTBOUND_ATTEMPTS_TOTAL
            .with_label_values(&["refused_no_idle_line"])
            .get(),
        before_no_idle_line + 1.0,
        "no-idle-line attempt must be counted under its own outcome label"
    );
    assert_eq!(
        metrics::OUTBOUND_ATTEMPTS_TOTAL
            .with_label_values(&["refused_network_failure"])
            .get(),
        before_network_failure + 1.0,
        "network-refused attempt must be counted separately from no-idle-line and unanswered"
    );
    assert_eq!(
        metrics::OUTBOUND_ATTEMPTS_TOTAL
            .with_label_values(&["unanswered"])
            .get(),
        before_unanswered + 1.0,
        "unanswered attempt must be counted separately, not folded into refused_network_failure"
    );

    // The scrape path (what an operator's dashboard actually reads) must
    // expose all three as distinct series too, not just the in-process
    // gauge accessor used above.
    let encoder = prometheus::TextEncoder::new();
    let families = prometheus::gather();
    let output = encoder.encode_to_string(&families).unwrap();
    for outcome in [
        "refused_no_idle_line",
        "refused_network_failure",
        "unanswered",
    ] {
        assert!(
            output.contains(&format!(
                "gsm_sip_bridge_outbound_attempts_total{{outcome=\"{outcome}\"}}"
            )),
            "expected a {outcome} series in scrape output:\n{output}"
        );
    }

    let _ = shutdown_tx.send(true);
    handle.abort();
}
