mod common;

// User Story 3 (specs/014-vowifi-metrics-restore): every inbound VoWiFi
// call must land in the shared `calls` table, the same one circuit-switched
// calls use, with `transport='vowifi'`. Drives the real
// `ims::observability::AgentObservability` (the component `ims::agent`
// calls into at every call outcome) with a real `StoreHandle` against a
// real temp-file database — no mocks.

use gsm_sip_bridge::control::protocol::{AgentKind, BridgeFailureReason, CallStatus};
use gsm_sip_bridge::control::server::start_control_server;
use gsm_sip_bridge::ims::observability::AgentObservability;
use gsm_sip_bridge::observability::reporter::Reporter;
use gsm_sip_bridge::store::StoreHandle;
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
async fn test_answered_and_missed_calls_are_persisted_with_vowifi_transport() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir
        .path()
        .join("call-history-test.sock")
        .to_str()
        .unwrap()
        .to_string();
    let db_path = dir.path().join("call-history-test.db");

    let (cmd_tx, _cmd_rx) = mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = start_control_server(&socket_path, cmd_tx, shutdown_rx).await;

    let module_id = "test-call-history".to_string();
    let reporter = Reporter::spawn(
        socket_path.clone(),
        AgentKind::Ims,
        module_id.clone(),
        Duration::from_millis(50),
    );
    let store = StoreHandle::open(&db_path).unwrap();
    let obs = AgentObservability::new(
        reporter,
        module_id.clone(),
        Some(store),
        "sip:100@pbx:5060".to_string(),
        gsm_sip_bridge::store::Transport::Vowifi,
    );

    obs.report_call_answered_and_ended(
        "+15551110000",
        chrono::Utc::now(),
        30.0,
        gsm_sip_bridge::ims::media_stats::DirectionVerdict::BothWays,
    );
    obs.report_call_not_answered(
        CallStatus::Missed,
        BridgeFailureReason::RingTimeout,
        "+15552220000",
        chrono::Utc::now(),
    );

    wait_for(
        || {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM calls WHERE transport = 'vowifi'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
                == 2
        },
        Duration::from_secs(5),
    )
    .await;

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let answered: (String, f64, String) = conn
        .query_row(
            "SELECT caller_id, duration_seconds, status FROM calls WHERE caller_id = '+15551110000'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(answered.0, "+15551110000");
    assert!(answered.1 >= 30.0);
    assert_eq!(answered.2, "answered");

    let missed: (f64, String) = conn
        .query_row(
            "SELECT duration_seconds, status FROM calls WHERE caller_id = '+15552220000'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(missed.0, 0.0);
    assert_eq!(missed.1, "missed");

    let _ = shutdown_tx.send(true);
    handle.abort();
}
