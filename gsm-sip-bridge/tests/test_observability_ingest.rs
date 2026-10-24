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
            gm_connection_up: Some(true),
            pbx_registered: None,
            // A carrier-side agent hosts no registrar (spec 024).
            sip_server_bindings: None,
            sip_server_ring_registered: None,
            registration_expires_at: None,
            child_sa_up: None,
            tunnel_flapping: None,
            ike_sa_loss_cause: None,
            uplink: None,
        },
        events: vec![ObservedEvent::CallCompleted {
            status: CallStatus::Answered,
            duration_seconds: 12.5,
        }],
        dropped: 0,
        details: None,
    };

    // `send_cmd` is a blocking call (`std::os::unix::net::UnixStream`).
    // `spawn_blocking` keeps it off this runtime's async worker threads, so
    // it can't starve the spawned control server's accept loop.
    let sock = socket_path.clone();
    let resp = tokio::task::spawn_blocking(move || {
        send_cmd(
            &sock,
            &ControlCmd::Observe {
                report: Box::new(report),
            },
        )
    })
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
        details: None,
    };
    let sock = socket_path.clone();
    tokio::task::spawn_blocking(move || {
        send_cmd(
            &sock,
            &ControlCmd::Observe {
                report: Box::new(report),
            },
        )
    })
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_supervisor_report_sets_the_child_sa_gauge() {
    let (socket_path, handle, shutdown_tx) = start_test_server().await;
    let module_id = "test-ingest-child-sa".to_string();

    for (seq, up, want) in [(1u64, false, 0.0), (2, true, 1.0)] {
        let report = AgentReport {
            agent: AgentKind::Supervisor,
            module_id: module_id.clone(),
            epoch: 1,
            seq,
            state: AgentState {
                child_sa_up: Some(up),
                ..Default::default()
            },
            events: vec![],
            dropped: 0,
            details: None,
        };
        let sock = socket_path.clone();
        tokio::task::spawn_blocking(move || {
            send_cmd(
                &sock,
                &ControlCmd::Observe {
                    report: Box::new(report),
                },
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            metrics::VOWIFI_CHILD_SA_UP
                .with_label_values(&[&module_id])
                .get(),
            want
        );
    }

    let _ = shutdown_tx.send(true);
    handle.abort();
}

// ---- specs/122-management-webui: agent details kept for the web UI ----

mod details {
    use gsm_sip_bridge::control::protocol::{
        ActiveCallInfoWire, AgentDetails, AgentKind, AgentReport, AgentState, CallDirectionWire,
        RegistrationStateWire,
    };
    use gsm_sip_bridge::metrics::ingest::{
        apply_report, clear_active_call_details, snapshot_lines, AgentSnapshot,
    };

    fn report(
        module: &str,
        seq: u64,
        state: AgentState,
        details: Option<AgentDetails>,
    ) -> AgentReport {
        AgentReport {
            agent: AgentKind::Ims,
            module_id: module.to_string(),
            epoch: 7,
            seq,
            state,
            events: vec![],
            dropped: 0,
            details,
        }
    }

    fn snap(module: &str) -> AgentSnapshot {
        snapshot_lines()
            .into_iter()
            .find(|s| s.module_id == module && s.agent == AgentKind::Ims)
            .expect("the agent must have a snapshot")
    }

    fn call(remote: &str) -> ActiveCallInfoWire {
        ActiveCallInfoWire {
            remote: remote.into(),
            direction: CallDirectionWire::Inbound,
            started_at_unix: 1,
            answered: true,
            loss_percent: None,
            jitter_ms: None,
            rtt_ms: None,
        }
    }

    #[test]
    fn some_overwrites_and_none_keeps() {
        let m = "ui-details-merge";
        apply_report(&report(
            m,
            1,
            AgentState {
                registered: Some(true),
                tunnel_up: Some(true),
                ..Default::default()
            },
            Some(AgentDetails {
                registration_state: Some(RegistrationStateWire::Registered),
                plmn: Some("405-857".into()),
                ..Default::default()
            }),
        ));
        // A heartbeat carrying nothing must not blank what was known.
        apply_report(&report(m, 2, AgentState::default(), None));
        let s = snap(m);
        assert_eq!(s.registered, Some(true));
        assert_eq!(s.tunnel_up, Some(true));
        assert_eq!(s.details.plmn.as_deref(), Some("405-857"));

        // A later Some replaces it, other fields stay.
        apply_report(&report(
            m,
            3,
            AgentState {
                tunnel_up: Some(false),
                ..Default::default()
            },
            Some(AgentDetails {
                registration_state: Some(RegistrationStateWire::Renewing),
                ..Default::default()
            }),
        ));
        let s = snap(m);
        assert_eq!(s.tunnel_up, Some(false));
        assert_eq!(s.registered, Some(true));
        assert_eq!(
            s.details.registration_state,
            Some(RegistrationStateWire::Renewing)
        );
        assert_eq!(s.details.plmn.as_deref(), Some("405-857"));
    }

    #[test]
    fn a_lapsed_registration_clears_registered_since_but_a_silent_report_does_not() {
        let m = "ui-details-regat";
        let with = |seq, state: Option<RegistrationStateWire>, at: Option<u64>| {
            report(
                m,
                seq,
                AgentState::default(),
                Some(AgentDetails {
                    registration_state: state,
                    registered_at: at,
                    ..Default::default()
                }),
            )
        };
        apply_report(&with(
            1,
            Some(RegistrationStateWire::Registered),
            Some(1_700_000_000),
        ));
        assert_eq!(snap(m).details.registered_at, Some(1_700_000_000));

        // An older agent that says nothing about the state changes nothing.
        apply_report(&with(2, None, None));
        assert_eq!(snap(m).details.registered_at, Some(1_700_000_000));

        // Renewing is still registered.
        apply_report(&with(3, Some(RegistrationStateWire::Renewing), None));
        assert_eq!(snap(m).details.registered_at, Some(1_700_000_000));

        // A failed line has no "registered since".
        apply_report(&with(4, Some(RegistrationStateWire::Failed), None));
        assert_eq!(snap(m).details.registered_at, None);
        assert_eq!(
            snap(m).details.registration_state,
            Some(RegistrationStateWire::Failed)
        );
    }

    #[test]
    fn active_call_details_replace_the_list() {
        let m = "ui-details-calls";
        let with_calls = |seq, calls: Vec<ActiveCallInfoWire>| {
            report(
                m,
                seq,
                AgentState::default(),
                Some(AgentDetails::default().with_active_calls(calls)),
            )
        };
        apply_report(&with_calls(
            1,
            vec![call("+919000000001"), call("+919000000002")],
        ));
        assert_eq!(snap(m).details.active_call_details.unwrap().len(), 2);
        apply_report(&with_calls(2, vec![call("+919000000001")]));
        assert_eq!(snap(m).details.active_call_details.unwrap().len(), 1);
        apply_report(&with_calls(3, vec![]));
        assert_eq!(snap(m).details.active_call_details.unwrap().len(), 0);
    }

    #[test]
    fn a_replayed_report_does_not_overwrite_details() {
        let m = "ui-details-replay";
        let plmn = |p: &str| {
            Some(AgentDetails {
                plmn: Some(p.into()),
                ..Default::default()
            })
        };
        apply_report(&report(m, 5, AgentState::default(), plmn("405-857")));
        // Same epoch, lower seq: a retry of something already applied.
        apply_report(&report(m, 4, AgentState::default(), plmn("404-45")));
        assert_eq!(snap(m).details.plmn.as_deref(), Some("405-857"));
    }

    #[test]
    fn clearing_a_stale_agent_empties_its_calls_but_keeps_its_signal() {
        let m = "ui-details-stale";
        apply_report(&report(
            m,
            1,
            AgentState::default(),
            Some(
                AgentDetails {
                    plmn: Some("405-857".into()),
                    ..Default::default()
                }
                .with_active_calls(vec![call("+919000000001")]),
            ),
        ));
        clear_active_call_details(AgentKind::Ims, m);
        let s = snap(m);
        assert_eq!(s.details.active_call_details.unwrap().len(), 0);
        assert_eq!(s.details.plmn.as_deref(), Some("405-857"));
    }
}

mod signal_gauges {
    use gsm_sip_bridge::control::protocol::{
        AgentDetails, AgentKind, AgentReport, AgentState, SignalReadingWire,
    };
    use gsm_sip_bridge::metrics::ingest::apply_report;

    fn scrape() -> String {
        gsm_sip_bridge::metrics::refresh_signal_age();
        prometheus::TextEncoder::new()
            .encode_to_string(&prometheus::gather())
            .unwrap()
    }

    fn report(
        agent: AgentKind,
        module: &str,
        seq: u64,
        signal: Option<SignalReadingWire>,
    ) -> AgentReport {
        AgentReport {
            agent,
            module_id: module.to_string(),
            epoch: 31,
            seq,
            state: AgentState::default(),
            events: vec![],
            dropped: 0,
            details: Some(AgentDetails {
                signal,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn an_ims_readings_land_as_vowifi_gauges_and_a_volte_one_as_volte() {
        let reading = SignalReadingWire {
            at_unix: 1_700_000_000,
            rssi_dbm: Some(-71),
            rat: Some("LTE".into()),
            rsrp_dbm: Some(-98),
            rsrq_db: None,
            sinr_db: Some(9),
        };
        apply_report(&report(AgentKind::Ims, "sig-ims", 1, Some(reading.clone())));
        apply_report(&report(AgentKind::Volte, "sig-volte", 1, Some(reading)));
        let out = scrape();
        assert!(out.contains(
            "gsm_sip_bridge_signal_rssi_dbm{module=\"sig-ims\",transport=\"vowifi\"} -71"
        ));
        assert!(out.contains(
            "gsm_sip_bridge_signal_rsrp_dbm{module=\"sig-ims\",transport=\"vowifi\"} -98"
        ));
        assert!(out.contains(
            "gsm_sip_bridge_signal_rssi_dbm{module=\"sig-volte\",transport=\"volte\"} -71"
        ));
        assert!(
            !out.contains("gsm_sip_bridge_signal_rsrq_db{module=\"sig-ims\""),
            "an unknown RSRQ exports no series"
        );
        assert!(out.contains("gsm_sip_bridge_signal_last_read_seconds{module=\"sig-ims\""));
    }

    #[test]
    fn the_sip_side_never_exports_signal() {
        apply_report(&report(
            AgentKind::Sip,
            "sig-sip-only",
            1,
            Some(SignalReadingWire {
                at_unix: 1,
                rssi_dbm: Some(-50),
                ..Default::default()
            }),
        ));
        assert!(!scrape().contains("module=\"sig-sip-only\",transport"));
    }
}
