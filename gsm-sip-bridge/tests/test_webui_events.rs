// specs/122-management-webui, User Story 4: a persisted, deduplicated event
// timeline fed by every process, with retention. Real store file, real
// `alerts::dispatch`, real `ingest::apply_report`, real HTTP.

mod common;

use chrono::{Duration, Utc};
use common::{open_store, wait_for_rows, Fixture};
use gsm_sip_bridge::alerts::discord::DiscordClient;
use gsm_sip_bridge::alerts::{self, AlertCategory, CriticalEvent, CriticalEventKind};
use gsm_sip_bridge::config::secret::Secret;
use gsm_sip_bridge::config::AlertsConfig;
use gsm_sip_bridge::control::protocol::{
    AgentKind, AgentReport, AgentState, DeadPeerVerdict, HoldEndedBy, ObservedEvent,
    TunnelRecoveryReason, UplinkHoldCause,
};
use gsm_sip_bridge::store::{events, StoreCommand};
use std::sync::OnceLock;
use tokio::sync::Mutex;

/// The event sink and the open-raise map are process-wide, so tests that use
/// them take turns.
fn turn() -> &'static Mutex<()> {
    static TURN: OnceLock<Mutex<()>> = OnceLock::new();
    TURN.get_or_init(|| Mutex::new(()))
}

fn event(
    category: AlertCategory,
    module: &str,
    kind: CriticalEventKind,
    secs: i64,
) -> CriticalEvent {
    CriticalEvent {
        category,
        unit_id: Some(module.to_string()),
        description: format!("{} {:?}", category.as_str(), kind),
        phone_number: None,
        at: Utc::now() + Duration::seconds(secs),
        kind,
    }
}

fn client() -> DiscordClient {
    DiscordClient::new(Secret::new(String::new()), "test".to_string()).unwrap()
}

/// `(kind, module_id, severity, raise_id, duration_seconds, id)`.
type Row = (
    String,
    Option<String>,
    String,
    Option<i64>,
    Option<f64>,
    i64,
);

fn rows(path: &std::path::Path) -> Vec<Row> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare("SELECT kind, module_id, severity, raise_id, duration_seconds, id FROM events ORDER BY id")
        .unwrap();
    let collected = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    collected
}

fn supervisor_report(module: &str, seq: u64, events: Vec<ObservedEvent>) -> AgentReport {
    AgentReport {
        agent: AgentKind::Supervisor,
        module_id: module.to_string(),
        epoch: 5150,
        seq,
        state: AgentState::default(),
        events,
        dropped: 0,
        details: None,
    }
}

#[tokio::test]
async fn a_raise_and_its_recovery_give_two_rows_linked_with_a_duration() {
    let _turn = turn().lock().await;
    let f = Fixture::start("").await;
    let store = open_store(&f.db_path);
    events::install_sink(store.sender());

    let config = AlertsConfig::default(); // registration_loss is disabled
    let c = client();
    alerts::dispatch(
        &c,
        &config,
        event(
            AlertCategory::RegistrationLoss,
            "line-a",
            CriticalEventKind::Failure,
            0,
        ),
    )
    .await;
    alerts::dispatch(
        &c,
        &config,
        event(
            AlertCategory::RegistrationLoss,
            "line-a",
            CriticalEventKind::Recovered,
            90,
        ),
    )
    .await;
    wait_for_rows(&f.db_path, "events", 2);

    let r = rows(&f.db_path);
    assert_eq!(r[0].0, "alert_raised:registration_loss");
    assert_eq!(r[0].2, "critical");
    assert_eq!(r[0].1.as_deref(), Some("line-a"));
    assert_eq!(r[1].0, "alert_resolved:registration_loss");
    assert_eq!(r[1].2, "info");
    assert_eq!(r[1].3, Some(r[0].5), "the resolve points at its raise");
    let seconds = r[1].4.unwrap();
    assert!((89.0..=91.0).contains(&seconds), "{seconds}");
}

#[tokio::test]
async fn a_retried_raise_and_an_unmatched_recovery_add_nothing() {
    let _turn = turn().lock().await;
    let f = Fixture::start("").await;
    let store = open_store(&f.db_path);
    events::install_sink(store.sender());
    let config = AlertsConfig::default();
    let c = client();

    // Nothing open: a recovery is dropped.
    alerts::dispatch(
        &c,
        &config,
        event(
            AlertCategory::TunnelFailure,
            "line-a",
            CriticalEventKind::Recovered,
            0,
        ),
    )
    .await;
    // One raise, then the same raise again (a retried delivery).
    alerts::dispatch(
        &c,
        &config,
        event(
            AlertCategory::TunnelFailure,
            "line-a",
            CriticalEventKind::Failure,
            1,
        ),
    )
    .await;
    alerts::dispatch(
        &c,
        &config,
        event(
            AlertCategory::TunnelFailure,
            "line-a",
            CriticalEventKind::Failure,
            2,
        ),
    )
    .await;
    wait_for_rows(&f.db_path, "events", 1);
    // A different line is its own incident.
    alerts::dispatch(
        &c,
        &config,
        event(
            AlertCategory::TunnelFailure,
            "line-b",
            CriticalEventKind::Failure,
            3,
        ),
    )
    .await;
    wait_for_rows(&f.db_path, "events", 2);
    assert_eq!(rows(&f.db_path).len(), 2);
    assert_eq!(events::open_raises().len(), 2);
}

#[tokio::test]
async fn one_shot_categories_are_not_incidents() {
    let _turn = turn().lock().await;
    let f = Fixture::start("").await;
    let store = open_store(&f.db_path);
    events::install_sink(store.sender());
    let config = AlertsConfig::default();
    let c = client();

    alerts::dispatch(
        &c,
        &config,
        event(AlertCategory::Sms, "line-a", CriticalEventKind::Failure, 0),
    )
    .await;
    alerts::dispatch(
        &c,
        &config,
        event(
            AlertCategory::MissedCall,
            "line-a",
            CriticalEventKind::Failure,
            0,
        ),
    )
    .await;
    // A recorded one afterwards proves the writer is caught up.
    alerts::dispatch(
        &c,
        &config,
        event(
            AlertCategory::GmConnectionLost,
            "line-a",
            CriticalEventKind::Failure,
            0,
        ),
    )
    .await;
    wait_for_rows(&f.db_path, "events", 1);
    assert_eq!(rows(&f.db_path)[0].0, "alert_raised:gm_connection_lost");
}

#[tokio::test]
async fn tunnel_recoveries_dead_peers_and_uplink_holds_are_recorded() {
    let _turn = turn().lock().await;
    let f = Fixture::start("").await;
    let store = open_store(&f.db_path);
    events::install_sink(store.sender());

    gsm_sip_bridge::metrics::ingest::apply_report(&supervisor_report(
        "events-line",
        1,
        vec![
            ObservedEvent::TunnelRecovered {
                reason: TunnelRecoveryReason::IkeSaLost,
                pcscfs: vec![],
                rebuilt_at_unix: None,
                dead_peer: Some(DeadPeerVerdict::Real),
            },
            ObservedEvent::TunnelRecovered {
                reason: TunnelRecoveryReason::ChildSaMissing,
                pcscfs: vec![],
                rebuilt_at_unix: None,
                dead_peer: None,
            },
            ObservedEvent::UplinkHoldStarted {
                cause: UplinkHoldCause::NoRoute,
            },
            ObservedEvent::UplinkHoldEnded {
                cause: UplinkHoldCause::NoRoute,
                held_secs: 75,
                ended_by: HoldEndedBy::Restored,
            },
        ],
    ));
    wait_for_rows(&f.db_path, "events", 5);

    let kinds: Vec<String> = rows(&f.db_path).into_iter().map(|r| r.0).collect();
    assert_eq!(kinds.iter().filter(|k| *k == "tunnel_recovered").count(), 2);
    assert!(kinds.contains(&"dead_peer_loss".to_string()));
    assert!(kinds.contains(&"uplink_hold_started".to_string()));
    let ended = rows(&f.db_path)
        .into_iter()
        .find(|r| r.0 == "uplink_hold_ended")
        .unwrap();
    assert_eq!(ended.4, Some(75.0), "the hold's length is the duration");
}

#[tokio::test]
async fn rows_survive_a_reopen_and_pruning_keeps_the_recent_ones() {
    let _turn = turn().lock().await;
    let f = Fixture::start("").await;
    {
        let store = open_store(&f.db_path);
        events::install_sink(store.sender());
        let tx = store.sender();
        for days in [45, 31, 5, 0] {
            tx.send(StoreCommand::InsertEvent(events::EventRecord {
                at: Utc::now() - Duration::days(days),
                module_id: Some("line-a".into()),
                kind: events::EventKind::ScheduledRestart,
                detail: format!("{days} days ago"),
                duration_seconds: None,
                resolves_raise_at: None,
            }))
            .unwrap();
        }
        wait_for_rows(&f.db_path, "events", 4);
        store.shutdown();
    }

    // A second open of the same file: nothing was lost.
    let store = open_store(&f.db_path);
    assert_eq!(rows(&f.db_path).len(), 4);

    store
        .sender()
        .send(StoreCommand::PruneEvents {
            older_than: Utc::now() - Duration::days(30),
        })
        .unwrap();
    wait_for_rows(&f.db_path, "events", 2);
}

#[tokio::test]
async fn the_events_api_filters_by_line_and_by_kind_prefix() {
    let _turn = turn().lock().await;
    let f = Fixture::start("").await;
    let store = open_store(&f.db_path);
    events::install_sink(store.sender());
    let config = AlertsConfig::default();
    let c = client();

    alerts::dispatch(
        &c,
        &config,
        event(
            AlertCategory::RegistrationLoss,
            "line-a",
            CriticalEventKind::Failure,
            0,
        ),
    )
    .await;
    alerts::dispatch(
        &c,
        &config,
        event(
            AlertCategory::RegistrationLoss,
            "line-a",
            CriticalEventKind::Recovered,
            30,
        ),
    )
    .await;
    alerts::dispatch(
        &c,
        &config,
        event(
            AlertCategory::TunnelFailure,
            "line-b",
            CriticalEventKind::Failure,
            40,
        ),
    )
    .await;
    events::record(
        events::EventKind::ScheduledRestart,
        Some("line-b"),
        "success",
        Some(12.0),
    );
    wait_for_rows(&f.db_path, "events", 4);

    let cookie = f.login().await;
    let (status, all) = f.get_json(&cookie, "/api/events").await;
    assert_eq!(status, 200);
    assert_eq!(all["total"], 4);
    let items = all["items"].as_array().unwrap();
    assert!(
        items[0]["at"].as_str().unwrap() >= items[3]["at"].as_str().unwrap(),
        "newest first"
    );
    assert!(items
        .iter()
        .all(|i| i["detail"].is_string() && i["severity"].is_string()));

    let (_, by_line) = f.get_json(&cookie, "/api/events?line=line-b").await;
    assert_eq!(by_line["total"], 2);

    let (_, raised) = f.get_json(&cookie, "/api/events?kind=alert_raised:").await;
    assert_eq!(raised["total"], 2, "a trailing colon is a prefix");

    let (_, group) = f.get_json(&cookie, "/api/events?kind=alert_resolved").await;
    assert_eq!(group["total"], 1, "a bare group name matches its members");

    let (_, exact) = f
        .get_json(&cookie, "/api/events?kind=scheduled_restart")
        .await;
    assert_eq!(exact["total"], 1);
    assert_eq!(exact["items"][0]["duration_seconds"], 12.0);

    let (_, resolve) = f
        .get_json(&cookie, "/api/events?kind=alert_resolved:registration_loss")
        .await;
    assert!(resolve["items"][0]["raise_id"].is_i64());
}

#[tokio::test]
async fn open_alerts_on_the_overview_carry_their_start_time() {
    let _turn = turn().lock().await;
    let f = Fixture::start("").await;
    let store = open_store(&f.db_path);
    events::install_sink(store.sender());
    let config = AlertsConfig::default();
    let c = client();
    alerts::dispatch(
        &c,
        &config,
        event(
            AlertCategory::PcscfPoolExhausted,
            "line-z",
            CriticalEventKind::Failure,
            0,
        ),
    )
    .await;

    let cookie = f.login().await;
    let (_, overview) = f.get_json(&cookie, "/api/overview").await;
    let alert = overview["active_alerts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["category"] == "pcscf_pool_exhausted" && a["module_id"] == "line-z")
        .expect("the open alert is listed");
    assert!(alert["since"].is_string(), "{alert}");
}
