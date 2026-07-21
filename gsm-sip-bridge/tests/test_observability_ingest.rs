mod common;

// End-to-end check of the observability wire protocol
// (specs/014-vowifi-metrics-restore, contracts/observability-protocol.md):
// a real `Observe` command over a real Unix control socket, applied to the
// real Prometheus registry, then read back through the real scrape
// handler's encoding path. No mocks — this is exactly the path a VoWiFi
// agent and the daemon exercise in production, just with the client side
// swapped for a direct `ControlCmd::Observe` send instead of going through
// `observability::reporter::Reporter` (that path is covered separately in
// test_observability_reporter.rs).

use gsm_sip_bridge::control::client::send_cmd;
use gsm_sip_bridge::control::protocol::{
    AgentKind, AgentReport, AgentState, CallStatus, ControlCmd, ObservedEvent,
};
use gsm_sip_bridge::control::server::start_control_server;
use gsm_sip_bridge::metrics;
use tokio::sync::{mpsc, watch};

async fn start_test_server() -> (String, tokio::task::JoinHandle<()>, watch::Sender<bool>) {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir
        .path()
        .join("ingest-test.sock")
        .to_str()
        .unwrap()
        .to_string();
    std::mem::forget(dir); // keep the tempdir alive for the socket's lifetime

    let (cmd_tx, _cmd_rx) = mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = start_control_server(&socket_path, cmd_tx, shutdown_rx).await;
    (socket_path, handle, shutdown_tx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_observe_report_lands_in_scraped_metrics() {
    let (socket_path, handle, shutdown_tx) = start_test_server().await;

    let module_id = "test-ingest-e2e".to_string();
    let report = AgentReport {
        agent: AgentKind::Ims,
        module_id: module_id.clone(),
        epoch: 1,
        seq: 1,
        state: AgentState {
            active_calls: Some(0),
            registered: Some(true),
            tunnel_up: Some(true),
            pbx_registered: None,
        },
        events: vec![ObservedEvent::CallCompleted {
            status: CallStatus::Answered,
            duration_seconds: 12.5,
        }],
        dropped: 0,
    };

    // `send_cmd` is a blocking call (`std::os::unix::net::UnixStream`).
    // `spawn_blocking` keeps it off this runtime's async worker threads, so
    // it can't starve the spawned control server's accept loop.
    let sock = socket_path.clone();
    let resp =
        tokio::task::spawn_blocking(move || send_cmd(&sock, &ControlCmd::Observe { report }))
            .await
            .unwrap()
            .unwrap();
    assert!(matches!(
        resp,
        gsm_sip_bridge::control::protocol::ControlResp::Ok
    ));

    let encoder = prometheus::TextEncoder::new();
    let families = prometheus::gather();
    let output = encoder.encode_to_string(&families).unwrap();

    assert!(
        output.contains(&format!(
            "gsm_sip_bridge_calls_total{{module=\"{module_id}\",status=\"answered\",transport=\"vowifi\"}} 1"
        )),
        "expected VoWiFi call count missing from scrape output:\n{output}"
    );
    assert_eq!(
        metrics::VOWIFI_REGISTERED
            .with_label_values(&[&module_id])
            .get(),
        1.0,
        "registered=true in the report must set the registration gauge"
    );
    assert_eq!(
        metrics::VOWIFI_TUNNEL_UP
            .with_label_values(&[&module_id])
            .get(),
        1.0,
        "tunnel_up=true in the report must set the tunnel gauge"
    );

    let _ = shutdown_tx.send(true);
    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_observe_heartbeat_with_no_events_still_updates_liveness() {
    let (socket_path, handle, shutdown_tx) = start_test_server().await;

    let report = AgentReport {
        agent: AgentKind::Sip,
        module_id: "test-ingest-heartbeat".to_string(),
        epoch: 1,
        seq: 1,
        state: AgentState::default(),
        events: vec![],
        dropped: 3,
    };
    let sock = socket_path.clone();
    tokio::task::spawn_blocking(move || send_cmd(&sock, &ControlCmd::Observe { report }))
        .await
        .unwrap()
        .unwrap();

    let before = metrics::OBSERVABILITY_EVENTS_DROPPED_TOTAL
        .with_label_values(&["sip", "test-ingest-heartbeat"])
        .get();
    assert!(
        before >= 3.0,
        "dropped count from the report must be folded into the daemon-side counter"
    );

    let _ = shutdown_tx.send(true);
    handle.abort();
}
