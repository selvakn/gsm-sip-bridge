// specs/096 User Story 3: every steady-state recovery the line supervisor
// performs is counted per line and per reason, so a route that keeps
// vanishing -- repaired every tick, never counted as flapping -- is visible
// without reading logs. Drives a real supervisor `Reporter` over a real
// control socket into the real registry, the same path
// `supervise::orchestrate`'s strongSwan steady loop uses.

use gsm_sip_bridge::control::protocol::{
    AgentKind, AgentState, DeadPeerVerdict, HoldEndedBy, ObservedEvent, TunnelRecoveryReason,
    UplinkHoldCause, UplinkState,
};
use gsm_sip_bridge::control::server::start_control_server;
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

fn recoveries(module_id: &str, reason: TunnelRecoveryReason) -> f64 {
    metrics::VOWIFI_TUNNEL_RECOVERIES_TOTAL
        .with_label_values(&[module_id, reason.as_str()])
        .get()
}

fn dead_peer_losses(module_id: &str, verdict: DeadPeerVerdict) -> f64 {
    metrics::VOWIFI_DEAD_PEER_LOSSES_TOTAL
        .with_label_values(&[module_id, verdict.as_str()])
        .get()
}

fn lost(verdict: DeadPeerVerdict) -> ObservedEvent {
    ObservedEvent::TunnelRecovered {
        reason: TunnelRecoveryReason::IkeSaLost,
        pcscfs: Vec::new(),
        rebuilt_at_unix: None,
        dead_peer: Some(verdict),
    }
}

fn tunnel_state() -> AgentState {
    AgentState {
        child_sa_up: Some(true),
        tunnel_flapping: Some(false),
        ..Default::default()
    }
}

fn recovered(reason: TunnelRecoveryReason) -> ObservedEvent {
    ObservedEvent::TunnelRecovered {
        reason,
        pcscfs: Vec::new(),
        rebuilt_at_unix: None,
        dead_peer: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_recoveries_are_counted_per_reason_and_never_rewind() {
    use TunnelRecoveryReason::{DefaultRouteMissing, PcscfUnreachable};

    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir
        .path()
        .join("tunnel-recoveries-test.sock")
        .to_str()
        .unwrap()
        .to_string();

    let (cmd_tx, _cmd_rx) = mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = start_control_server(&socket_path, cmd_tx, shutdown_rx).await;

    let module = "test-tunnel-recoveries";
    let interval = Duration::from_millis(50);
    let reporter = Reporter::spawn(
        socket_path.clone(),
        AgentKind::Supervisor,
        module.to_string(),
        interval,
    );

    reporter.report(
        tunnel_state(),
        vec![
            recovered(DefaultRouteMissing),
            recovered(DefaultRouteMissing),
            recovered(PcscfUnreachable),
        ],
    );
    wait_for(
        || {
            recoveries(module, DefaultRouteMissing) == 2.0
                && recoveries(module, PcscfUnreachable) == 1.0
        },
        Duration::from_secs(5),
    )
    .await;

    // Heartbeats and event-free reports carry state only: nothing to count.
    reporter.report(tunnel_state(), Vec::new());
    tokio::time::sleep(interval * 4).await;
    assert_eq!(recoveries(module, DefaultRouteMissing), 2.0);
    assert_eq!(recoveries(module, PcscfUnreachable), 1.0);

    // A restarted supervisor reports through a fresh reporter: the count
    // carries on rather than starting again from zero.
    drop(reporter);
    let restarted = Reporter::spawn(
        socket_path.clone(),
        AgentKind::Supervisor,
        module.to_string(),
        interval,
    );
    restarted.report(tunnel_state(), vec![recovered(DefaultRouteMissing)]);
    wait_for(
        || recoveries(module, DefaultRouteMissing) == 3.0,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(recoveries(module, PcscfUnreachable), 1.0);

    let _ = shutdown_tx.send(true);
    handle.abort();
}

/// specs/098 US3: a dead-peer loss is counted by verdict, through the same
/// reporter -> socket -> registry path, and still counts as a recovery.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_peer_losses_are_counted_by_verdict() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir
        .path()
        .join("dead-peer-test.sock")
        .to_str()
        .unwrap()
        .to_string();

    let (cmd_tx, _cmd_rx) = mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = start_control_server(&socket_path, cmd_tx, shutdown_rx).await;

    let module = "test-dead-peer";
    let reporter = Reporter::spawn(
        socket_path.clone(),
        AgentKind::Supervisor,
        module.to_string(),
        Duration::from_millis(50),
    );
    reporter.report(
        tunnel_state(),
        vec![
            lost(DeadPeerVerdict::False),
            lost(DeadPeerVerdict::Real),
            recovered(TunnelRecoveryReason::IkeSaLost),
        ],
    );
    wait_for(
        || recoveries(module, TunnelRecoveryReason::IkeSaLost) == 3.0,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(dead_peer_losses(module, DeadPeerVerdict::False), 1.0);
    assert_eq!(dead_peer_losses(module, DeadPeerVerdict::Real), 1.0);
    // A loss with no verdict (carrier DELETE, unknown cause) counts nowhere.
    assert_eq!(dead_peer_losses(module, DeadPeerVerdict::Unknown), 0.0);

    let _ = shutdown_tx.send(true);
    handle.abort();
}

/// specs/100 US4: an uplink hold is visible in the registry, through the same
/// reporter -> socket -> registry path the supervisor uses: the gauge follows
/// the reported state, the start counts a hold, the end adds its seconds, and
/// the two new label values on the existing counters are counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_uplink_hold_is_counted_gauged_and_attributed() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir
        .path()
        .join("uplink-hold-test.sock")
        .to_str()
        .unwrap()
        .to_string();

    let (cmd_tx, _cmd_rx) = mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = start_control_server(&socket_path, cmd_tx, shutdown_rx).await;

    let module = "test-uplink-hold";
    let reporter = Reporter::spawn(
        socket_path.clone(),
        AgentKind::Supervisor,
        module.to_string(),
        Duration::from_millis(50),
    );
    let holds = |cause: UplinkHoldCause| {
        metrics::VOWIFI_UPLINK_HOLDS_TOTAL
            .with_label_values(&[module, cause.as_str()])
            .get()
    };
    let held_seconds = |cause: UplinkHoldCause| {
        metrics::VOWIFI_UPLINK_HELD_SECONDS_TOTAL
            .with_label_values(&[module, cause.as_str()])
            .get()
    };
    let gauge = || {
        metrics::VOWIFI_UPLINK_HELD
            .with_label_values(&[module])
            .get()
    };

    // The hold starts.
    reporter.report(
        AgentState {
            uplink: Some(UplinkState::NoRoute),
            ..tunnel_state()
        },
        vec![ObservedEvent::UplinkHoldStarted {
            cause: UplinkHoldCause::NoRoute,
        }],
    );
    wait_for(
        || holds(UplinkHoldCause::NoRoute) == 1.0 && gauge() == 1.0,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(holds(UplinkHoldCause::NoResponse), 0.0);

    // It ends, 312s later.
    reporter.report(
        AgentState {
            uplink: Some(UplinkState::Up),
            ..tunnel_state()
        },
        vec![ObservedEvent::UplinkHoldEnded {
            cause: UplinkHoldCause::NoRoute,
            held_secs: 312,
            ended_by: HoldEndedBy::Restored,
        }],
    );
    wait_for(
        || held_seconds(UplinkHoldCause::NoRoute) == 312.0 && gauge() == 0.0,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(held_seconds(UplinkHoldCause::NoResponse), 0.0);

    // A stuck half-open IKE_SA has its own recovery label, and a dead-peer loss
    // seen on a held line is attributed to the uplink, never to the peer.
    reporter.report(
        tunnel_state(),
        vec![
            recovered(TunnelRecoveryReason::IkeSaNegotiationStuck),
            lost(DeadPeerVerdict::Uplink),
        ],
    );
    wait_for(
        || {
            recoveries(module, TunnelRecoveryReason::IkeSaNegotiationStuck) == 1.0
                && recoveries(module, TunnelRecoveryReason::IkeSaLost) == 1.0
        },
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(dead_peer_losses(module, DeadPeerVerdict::Uplink), 1.0);
    assert_eq!(dead_peer_losses(module, DeadPeerVerdict::Real), 0.0);

    let _ = shutdown_tx.send(true);
    handle.abort();
}
