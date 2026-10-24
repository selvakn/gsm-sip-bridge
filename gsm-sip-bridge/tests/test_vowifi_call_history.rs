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
        gsm_sip_bridge::store::calls::CallDirection::Inbound,
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

// specs/122-management-webui: an outbound call leaves a row of its own, in the
// same table, filed by direction, with the dialled number as the remote party.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_outbound_calls_are_persisted_with_outbound_direction() {
    use gsm_sip_bridge::control::protocol::OutboundAttemptOutcome;
    use gsm_sip_bridge::store::calls::CallRecord;
    use gsm_sip_bridge::store::{StoreCommand, Transport};

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("outbound-history.db");
    let store = StoreHandle::open(&db_path).unwrap();
    let tx = store.sender();
    let started = chrono::Utc::now();

    tx.send(StoreCommand::InsertCall(CallRecord::outbound(
        "card-out",
        "+919000000001",
        started,
        37.5,
        "answered",
        Transport::Vowifi,
    )))
    .unwrap();
    let refusal = OutboundAttemptOutcome::RefusedNoIdleLine
        .history_status()
        .expect("a refusal is recorded");
    tx.send(StoreCommand::InsertCall(CallRecord::outbound(
        "",
        "+919000000002",
        started,
        0.0,
        refusal.as_str(),
        Transport::Volte,
    )))
    .unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    wait_for(
        || {
            conn.query_row("SELECT COUNT(*) FROM calls", [], |r| r.get::<_, i64>(0))
                .unwrap_or(0)
                == 2
        },
        Duration::from_secs(5),
    )
    .await;

    let rows: Vec<(String, String, String, String, f64, String)> = conn
        .prepare("SELECT module_id, direction, caller_id, status, duration_seconds, transport FROM calls ORDER BY id")
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows[0],
        (
            "card-out".into(),
            "outbound".into(),
            "+919000000001".into(),
            "answered".into(),
            37.5,
            "vowifi".into()
        )
    );
    assert_eq!(
        rows[1],
        (
            "".into(),
            "outbound".into(),
            "+919000000002".into(),
            "failed".into(),
            0.0,
            "volte".into()
        ),
        "refused before a line was chosen: no module"
    );
    drop(store);
}

// specs/122-management-webui: the quality figures reported for a call end up
// on that call's row, and only that call's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_media_quality_lands_on_the_call_row_and_not_the_next_one() {
    use gsm_sip_bridge::control::protocol::QualitySource;
    use gsm_sip_bridge::ims::media_stats::DirectionVerdict;

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("quality-history.db");
    let reporter = Reporter::spawn(
        dir.path().join("none.sock").to_str().unwrap().to_string(),
        AgentKind::Ims,
        "test-quality".to_string(),
        Duration::from_secs(3600),
    );
    let store = StoreHandle::open(&db_path).unwrap();
    let obs = AgentObservability::new(
        reporter,
        "test-quality".to_string(),
        Some(store),
        "sip:100@pbx:5060".to_string(),
        gsm_sip_bridge::store::Transport::Vowifi,
    );

    // Call one: both sources reported, then the row.
    obs.report_media_quality(QualitySource::Remote, 0.0, 0.004, Some(0.080));
    obs.report_media_quality(QualitySource::Local, 1.5, 0.012, None);
    obs.report_call_answered_and_ended(
        "+919000000001",
        chrono::Utc::now(),
        30.0,
        DirectionVerdict::BothWays,
        gsm_sip_bridge::store::calls::CallDirection::Inbound,
    );
    // Call two: nothing reported, and one this side placed.
    obs.report_call_answered_and_ended(
        "+919000000002",
        chrono::Utc::now(),
        10.0,
        DirectionVerdict::BothWays,
        gsm_sip_bridge::store::calls::CallDirection::Outbound,
    );

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    wait_for(
        || {
            conn.query_row("SELECT COUNT(*) FROM calls", [], |r| r.get::<_, i64>(0))
                .unwrap_or(0)
                == 2
        },
        Duration::from_secs(5),
    )
    .await;

    let quality = |caller: &str| -> (Option<f64>, Option<f64>, Option<f64>) {
        conn.query_row(
            "SELECT loss_percent, jitter_ms, rtt_ms FROM calls WHERE caller_id = ?1",
            [caller],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    };
    let (loss, jitter, rtt) = quality("+919000000001");
    assert_eq!(loss, Some(1.5));
    assert!(
        (jitter.unwrap() - 12.0).abs() < 1e-9,
        "seconds become milliseconds"
    );
    assert!((rtt.unwrap() - 80.0).abs() < 1e-9);
    assert_eq!(quality("+919000000002"), (None, None, None));

    let direction = |caller: &str| -> String {
        conn.query_row(
            "SELECT direction FROM calls WHERE caller_id = ?1",
            [caller],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(direction("+919000000001"), "inbound");
    assert_eq!(direction("+919000000002"), "outbound");
}
