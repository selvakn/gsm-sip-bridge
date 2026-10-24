// `observability::reporter::Reporter` against a real Unix socket: reports
// made while the daemon is unreachable must survive (buffered, bounded)
// and be delivered once the daemon comes back, with no counter reset on
// the daemon side (FR-019/FR-019a/FR-019b, FR-020).

use gsm_sip_bridge::control::protocol::{AgentKind, AgentState, CallStatus, ObservedEvent};
use gsm_sip_bridge::control::server::start_control_server;
use gsm_sip_bridge::metrics;
use gsm_sip_bridge::observability::reporter::Reporter;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// Polls a Prometheus counter until it reaches `expected` or `timeout`
/// elapses, instead of a single fixed-delay check — avoids flakiness under
/// parallel-test-thread CPU contention, where a fixed sleep can't guarantee
/// the reporter's background thread got scheduled in time.
async fn wait_for_counter(counter: &prometheus::Counter, expected: f64, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if counter.get() == expected {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "counter did not reach {expected} within {timeout:?} (last seen: {})",
                counter.get()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_reports_made_before_the_daemon_exists_are_delivered_once_it_starts() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir
        .path()
        .join("reporter-test.sock")
        .to_str()
        .unwrap()
        .to_string();

    let module_id = "test-reporter-delivery".to_string();
    // No control server listening yet — every send in this window must be
    // buffered, not lost, and must not block the caller (FR-018).
    let reporter = Reporter::spawn(
        socket_path.clone(),
        AgentKind::Ims,
        module_id.clone(),
        std::time::Duration::from_millis(50),
    );
    reporter.report(
        AgentState {
            active_calls: Some(0),
            ..Default::default()
        },
        vec![ObservedEvent::CallCompleted {
            status: CallStatus::Answered,
            duration_seconds: 1.0,
        }],
    );

    // Give the reporter a couple of failed-connect cycles to prove it isn't
    // just getting lucky with timing.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let counter = metrics::CALLS_TOTAL.with_label_values(&[&module_id, "answered", "vowifi"]);
    assert_eq!(
        counter.get(),
        0.0,
        "nothing should have reached the registry before a listener existed"
    );

    // Now start the daemon side. The reporter's next tick should flush the
    // buffered report.
    let (cmd_tx, _cmd_rx) = mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = start_control_server(&socket_path, cmd_tx, shutdown_rx).await;

    wait_for_counter(&counter, 1.0, Duration::from_secs(5)).await;

    let _ = shutdown_tx.send(true);
    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_repeated_reports_do_not_reset_the_daemon_side_counter() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir
        .path()
        .join("reporter-restart-test.sock")
        .to_str()
        .unwrap()
        .to_string();
    let module_id = "test-reporter-restart".to_string();

    let (cmd_tx, _cmd_rx) = mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = start_control_server(&socket_path, cmd_tx, shutdown_rx).await;

    let counter = metrics::CALLS_TOTAL.with_label_values(&[&module_id, "answered", "vowifi"]);

    // Simulate an agent restart: a fresh Reporter, same module id, sending
    // its own count of completed calls. The daemon's counter is the only
    // place totals live (research.md §R2) — a second "generation" of the
    // agent must add to it, not replace it.
    for generation in 1..=2 {
        let reporter = Reporter::spawn(
            socket_path.clone(),
            AgentKind::Ims,
            module_id.clone(),
            Duration::from_millis(50),
        );
        reporter.report(
            AgentState::default(),
            vec![ObservedEvent::CallCompleted {
                status: CallStatus::Answered,
                duration_seconds: 2.0,
            }],
        );
        wait_for_counter(&counter, generation as f64, Duration::from_secs(5)).await;
        // reporter dropped here — its channel disconnects, its thread exits
    }

    assert_eq!(
        counter.get(),
        2.0,
        "two agent \"generations\" reporting one call each must sum to 2, not reset to 1"
    );

    let _ = shutdown_tx.send(true);
    handle.abort();
}

// specs/122-management-webui: the reporter carries `AgentDetails` from its
// provider on every report, through the real socket into the daemon's
// ingest, so the web UI sees registration wording, carrier and signal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_details_from_the_provider_reach_the_daemons_snapshot() {
    use gsm_sip_bridge::control::protocol::{
        AgentDetails, RegistrationStateWire, SignalReadingWire,
    };
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir
        .path()
        .join("reporter-details.sock")
        .to_str()
        .unwrap()
        .to_string();
    let (cmd_tx, _cmd_rx) = mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = start_control_server(&socket_path, cmd_tx, shutdown_rx).await;

    let module_id = "test-reporter-details".to_string();
    let reporter = Reporter::spawn(
        socket_path.clone(),
        AgentKind::Ims,
        module_id.clone(),
        std::time::Duration::from_millis(50),
    );
    reporter.set_details_provider(Arc::new(|| AgentDetails {
        registration_state: Some(RegistrationStateWire::Registered),
        registered_at: Some(1_700_000_000),
        plmn: Some("405-857".into()),
        signal: Some(SignalReadingWire {
            at_unix: 1_700_000_100,
            rssi_dbm: Some(-71),
            ..Default::default()
        }),
        active_call_details: None,
    }));
    reporter.report(
        AgentState {
            registered: Some(true),
            ..Default::default()
        },
        vec![],
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let found = metrics::ingest::snapshot_lines()
            .into_iter()
            .find(|s| s.module_id == module_id && s.agent == AgentKind::Ims);
        if let Some(snap) = found {
            if snap.details.plmn.is_some() {
                assert_eq!(snap.details.plmn.as_deref(), Some("405-857"));
                assert_eq!(snap.details.registered_at, Some(1_700_000_000));
                assert_eq!(snap.details.signal.unwrap().rssi_dbm, Some(-71));
                assert_eq!(
                    snap.details.registration_state.unwrap().to_string(),
                    "registered"
                );
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the details never reached the daemon"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let _ = shutdown_tx.send(true);
    handle.abort();
}
