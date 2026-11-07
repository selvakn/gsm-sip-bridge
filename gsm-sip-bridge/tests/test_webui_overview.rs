// specs/122-management-webui, User Story 1: the Overview matches what the
// agents reported. Reports go through the real `metrics::ingest::apply_report`
// and are read back through the real router.

mod common;

use common::Fixture;
use gsm_sip_bridge::control::protocol::{
    ActiveCallInfoWire, AgentDetails, AgentKind, AgentReport, AgentState, CallDirectionWire,
    RegistrationStateWire, SignalReadingWire, UplinkState,
};
use gsm_sip_bridge::metrics::{self, ingest::apply_report};
use gsm_sip_bridge::modules::discovery::derive_module_id;
use std::sync::Once;
use std::time::Duration;

static INIT: Once = Once::new();

/// The staleness threshold is process-wide and first-writer-wins, so every
/// test sets it before anything reads it.
fn init() {
    INIT.call_once(|| {
        metrics::server::record_start_time();
        metrics::server::set_staleness_threshold(Duration::from_secs(2));
    });
}

fn report(
    agent: AgentKind,
    module: &str,
    seq: u64,
    state: AgentState,
    details: Option<AgentDetails>,
) -> AgentReport {
    AgentReport {
        agent,
        module_id: module.to_string(),
        epoch: 4242,
        seq,
        state,
        events: vec![],
        dropped: 0,
        details,
    }
}

fn line<'a>(overview: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    overview["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == id)
        .unwrap_or_else(|| panic!("no line {id} in {overview}"))
}

#[tokio::test]
async fn a_vowifi_line_matches_what_its_agents_reported() {
    init();
    let serial = "SYNTH-OVERVIEW-AAA111";
    let module = derive_module_id(serial);
    let f = Fixture::start(&format!(
        "[[vowifi.line]]\nmodem_serial = \"{serial}\"\nmsisdn = \"+919000000000\"\n"
    ))
    .await;

    apply_report(&report(
        AgentKind::Ims,
        &module,
        1,
        AgentState {
            registered: Some(true),
            tunnel_up: Some(true),
            gm_connection_up: Some(true),
            registration_expires_at: Some(2_000_000_000),
            active_calls: Some(1),
            ..Default::default()
        },
        Some(AgentDetails {
            registration_state: Some(RegistrationStateWire::Registered),
            registered_at: Some(1_999_990_000),
            plmn: Some("405-857".into()),
            // An operator's configured number takes priority over IMS.
            msisdn: Some("+919000000002".into()),
            signal: Some(SignalReadingWire {
                at_unix: 1_999_999_000,
                rssi_dbm: Some(-71),
                rat: Some("LTE".into()),
                rsrp_dbm: Some(-98),
                rsrq_db: Some(-11),
                sinr_db: Some(9),
            }),
            active_call_details: None,
        }),
    ));
    apply_report(&report(
        AgentKind::Sip,
        &module,
        1,
        AgentState {
            pbx_registered: Some(true),
            ..Default::default()
        },
        Some(
            AgentDetails::default().with_active_calls(vec![ActiveCallInfoWire {
                remote: "+919000000001".into(),
                direction: CallDirectionWire::Outbound,
                started_at_unix: 1_999_999_500,
                answered: true,
                loss_percent: Some(0.4),
                jitter_ms: Some(12.0),
                rtt_ms: None,
            }]),
        ),
    ));
    apply_report(&report(
        AgentKind::Supervisor,
        &module,
        1,
        AgentState {
            child_sa_up: Some(true),
            uplink: Some(UplinkState::Up),
            ..Default::default()
        },
        None,
    ));

    let cookie = f.login().await;
    let (status, overview) = f.get_json(&cookie, "/api/overview").await;
    assert_eq!(status, 200);
    let l = line(&overview, &module);

    assert_eq!(l["transport"], "vowifi");
    assert_eq!(l["registration_state"], "registered");
    assert_eq!(l["msisdn"], "+919000000000");
    assert_eq!(l["tunnel_up"], true);
    assert_eq!(l["child_sa_up"], true);
    assert_eq!(l["gm_connection_up"], true);
    assert_eq!(l["uplink"], "up");
    assert!(l["pdn_up"].is_null(), "a VoWiFi line has no PDN");
    assert_eq!(l["expires_at"], "2033-05-18T03:33:20Z");
    assert!(l["registered_at"].is_string());
    assert_eq!(l["signal"]["rssi_dbm"], -71);
    assert_eq!(l["signal"]["rsrp_dbm"], -98);
    assert!(l["signal_note"].is_null());
    assert_eq!(l["active_calls"][0]["remote"], "+919000000001");
    assert_eq!(l["active_calls"][0]["direction"], "outbound");
    assert_eq!(l["active_call_count"], 1);
    assert_eq!(l["stale"], false);
    assert_eq!(l["health"], "ok");
    assert_eq!(l["health_reasons"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn ims_numbers_reach_overview_without_config_and_follow_refreshes() {
    init();
    let f = Fixture::start("").await;
    let cookie = f.login().await;

    for (carrier, sip, module, transport) in [
        (AgentKind::Ims, AgentKind::Sip, "ov-number-vowifi", "vowifi"),
        (
            AgentKind::Volte,
            AgentKind::VolteSip,
            "ov-number-volte",
            "volte",
        ),
    ] {
        let number_details = |number: &str| AgentDetails {
            registration_state: Some(RegistrationStateWire::Registered),
            msisdn: Some(number.into()),
            ..Default::default()
        };
        let send = |seq, details| {
            // Exercise the actual agent wire representation before ingesting.
            let wire = serde_json::to_vec(&report(
                carrier,
                module,
                seq,
                AgentState {
                    registered: Some(true),
                    ..Default::default()
                },
                details,
            ))
            .unwrap();
            apply_report(&serde_json::from_slice::<AgentReport>(&wire).unwrap());
        };
        send(1, Some(number_details("+919000000000")));
        apply_report(&report(
            sip,
            module,
            1,
            AgentState::default(),
            Some(number_details("+919000000009")),
        ));
        let (_, overview) = f.get_json(&cookie, "/api/overview").await;
        let l = line(&overview, module);
        assert_eq!(l["transport"], transport);
        assert_eq!(l["msisdn"], "+919000000000");

        // A new registration/refresh can replace the previously learned number.
        send(2, Some(number_details("+919000000001")));
        let (_, overview) = f.get_json(&cookie, "/api/overview").await;
        assert_eq!(line(&overview, module)["msisdn"], "+919000000001");

        // Both generations of older reports must leave the number intact:
        // before details existed, and with details but no number field.
        for (seq, details) in [(3, None), (4, Some(AgentDetails::default()))] {
            send(seq, details);
            let (_, overview) = f.get_json(&cookie, "/api/overview").await;
            assert_eq!(line(&overview, module)["msisdn"], "+919000000001");
        }

        // A current registration without a number explicitly clears the value.
        // A SIP-side number is never a fallback for the carrier's own number.
        send(5, Some(number_details("")));
        let (_, overview) = f.get_json(&cookie, "/api/overview").await;
        assert!(line(&overview, module)["msisdn"].is_null());
        send(6, None);
        let (_, overview) = f.get_json(&cookie, "/api/overview").await;
        assert!(line(&overview, module)["msisdn"].is_null());
    }
}

#[tokio::test]
async fn a_configured_number_survives_ims_number_changes_on_renewal() {
    init();
    for (carrier, section, serial) in [
        (AgentKind::Ims, "vowifi", "SYNTH-NUMBER-CFG111"),
        (AgentKind::Volte, "volte", "SYNTH-NUMBER-CFG222"),
    ] {
        let module = derive_module_id(serial);
        let f = Fixture::start(&format!(
            "[[{section}.line]]\nmodem_serial = \"{serial}\"\nmsisdn = \"+919000000002\"\n"
        ))
        .await;
        let cookie = f.login().await;

        for (seq, learned) in [(1, "+919000000000"), (2, "+919000000001"), (3, "")] {
            apply_report(&report(
                carrier,
                &module,
                seq,
                AgentState {
                    registered: Some(true),
                    ..Default::default()
                },
                Some(AgentDetails {
                    registration_state: Some(RegistrationStateWire::Registered),
                    msisdn: Some(learned.into()),
                    ..Default::default()
                }),
            ));

            // The carrier's learned identity still follows each accepted
            // renewal; the configured number overrides only the UI value.
            let snapshot = metrics::ingest::snapshot_lines()
                .into_iter()
                .find(|s| s.agent == carrier && s.module_id == module)
                .unwrap();
            assert_eq!(snapshot.details.msisdn.as_deref(), Some(learned));
            let (status, overview) = f.get_json(&cookie, "/api/overview").await;
            assert_eq!(status, 200);
            assert_eq!(line(&overview, &module)["msisdn"], "+919000000002");
        }
    }
}

#[tokio::test]
async fn a_volte_line_reports_its_pdn() {
    init();
    let f = Fixture::start("").await;
    apply_report(&report(
        AgentKind::Volte,
        "ov-volte-1",
        1,
        AgentState {
            registered: Some(true),
            tunnel_up: Some(true),
            gm_connection_up: Some(true),
            ..Default::default()
        },
        Some(AgentDetails {
            registration_state: Some(RegistrationStateWire::Registered),
            ..Default::default()
        }),
    ));
    apply_report(&report(
        AgentKind::VolteSip,
        "ov-volte-1",
        1,
        AgentState {
            pbx_registered: Some(true),
            ..Default::default()
        },
        None,
    ));

    let cookie = f.login().await;
    let (_, overview) = f.get_json(&cookie, "/api/overview").await;
    let l = line(&overview, "ov-volte-1");
    assert_eq!(l["transport"], "volte");
    assert_eq!(l["pdn_up"], true);
    assert!(l["tunnel_up"].is_null());
    assert!(l["child_sa_up"].is_null());
    assert_eq!(l["health"], "ok");
}

#[tokio::test]
async fn a_child_sa_going_down_turns_the_card_down_and_names_the_reason() {
    init();
    let f = Fixture::start("").await;
    apply_report(&report(
        AgentKind::Ims,
        "ov-childsa",
        1,
        AgentState {
            registered: Some(true),
            tunnel_up: Some(true),
            ..Default::default()
        },
        None,
    ));
    apply_report(&report(
        AgentKind::Supervisor,
        "ov-childsa",
        1,
        AgentState {
            child_sa_up: Some(false),
            ..Default::default()
        },
        None,
    ));

    let cookie = f.login().await;
    let (_, overview) = f.get_json(&cookie, "/api/overview").await;
    let l = line(&overview, "ov-childsa");
    assert_eq!(l["health"], "down");
    assert!(l["health_reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r == "child_sa_down"));
}

#[tokio::test]
async fn silence_past_the_threshold_turns_a_card_stale() {
    init();
    let f = Fixture::start("").await;
    apply_report(&report(
        AgentKind::Ims,
        "ov-stale",
        1,
        AgentState {
            registered: Some(true),
            active_calls: Some(1),
            ..Default::default()
        },
        Some(
            AgentDetails::default().with_active_calls(vec![ActiveCallInfoWire {
                remote: "+919000000001".into(),
                direction: CallDirectionWire::Inbound,
                started_at_unix: 1,
                answered: true,
                loss_percent: None,
                jitter_ms: None,
                rtt_ms: None,
            }]),
        ),
    ));
    let cookie = f.login().await;
    let (_, fresh) = f.get_json(&cookie, "/api/overview").await;
    assert_eq!(line(&fresh, "ov-stale")["stale"], false);

    tokio::time::sleep(Duration::from_millis(2300)).await;

    let (_, overview) = f.get_json(&cookie, "/api/overview").await;
    let l = line(&overview, "ov-stale");
    assert_eq!(l["stale"], true);
    assert_eq!(l["health"], "stale");
    assert_eq!(
        l["active_call_count"], 0,
        "the count goes with the details, or the Calls page keeps a live row"
    );
    assert_eq!(
        l["active_calls"].as_array().unwrap().len(),
        0,
        "a call is not shown live on a line nobody has heard from"
    );
}

#[tokio::test]
async fn a_pcsc_line_has_no_signal_and_says_why() {
    init();
    let f = Fixture::start("").await;
    apply_report(&report(
        AgentKind::Ims,
        "pcsc0",
        1,
        AgentState {
            registered: Some(true),
            ..Default::default()
        },
        None,
    ));
    let cookie = f.login().await;
    let (_, overview) = f.get_json(&cookie, "/api/overview").await;
    let l = line(&overview, "pcsc0");
    assert!(l["signal"].is_null());
    assert_eq!(l["signal_note"], "not_available_pcsc");
}

#[tokio::test]
async fn the_bridge_block_has_version_uptime_and_pbx_state() {
    init();
    let f = Fixture::start("[cs]\nenabled = false\n").await;
    apply_report(&report(
        AgentKind::Sip,
        "ov-pbx",
        1,
        AgentState {
            pbx_registered: Some(true),
            ..Default::default()
        },
        None,
    ));
    let cookie = f.login().await;
    let (_, overview) = f.get_json(&cookie, "/api/overview").await;
    let bridge = &overview["bridge"];
    assert_eq!(bridge["version"], env!("CARGO_PKG_VERSION"));
    assert!(bridge["uptime_seconds"].is_u64());
    assert_eq!(bridge["pbx_registered"], true);
    assert!(bridge["grafana_url"].is_null());
    assert!(overview["active_alerts"].is_array());
}

#[tokio::test]
async fn the_grafana_link_appears_only_when_configured() {
    init();
    let f = Fixture::start("grafana_url = \"https://grafana.example/d/abc\"\n").await;
    let cookie = f.login().await;
    let (_, overview) = f.get_json(&cookie, "/api/overview").await;
    assert_eq!(
        overview["bridge"]["grafana_url"],
        "https://grafana.example/d/abc"
    );
}

#[tokio::test]
async fn health_reports_module_and_store_counters() {
    init();
    let f = Fixture::start("").await;
    metrics::MODULES_ACTIVE.set(2.0);
    metrics::MODULES_FAILED.set(1.0);
    let cookie = f.login().await;
    let (status, health) = f.get_json(&cookie, "/api/health").await;
    assert_eq!(status, 200);
    assert_eq!(health["modules_active"], 2);
    assert_eq!(health["modules_failed"], 1);
    assert!(health["store_queue_depth"].is_i64());
    assert!(health["observability_events_dropped"].is_i64());
    assert!(health["cs_enabled"].is_boolean());
}
