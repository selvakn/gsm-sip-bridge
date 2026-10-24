// specs/122-management-webui, review follow-up: a timeline incident is closed
// when its condition clears, whether or not Discord was ever told. Driven
// through the real `ingest::apply_report`, as the daemon runs it.

mod common;

use common::{open_store, wait_for_rows, Fixture};
use gsm_sip_bridge::alerts::discord::DiscordClient;
use gsm_sip_bridge::config::secret::Secret;
use gsm_sip_bridge::config::AlertsConfig;
use gsm_sip_bridge::control::protocol::{AgentKind, AgentReport, AgentState};
use gsm_sip_bridge::metrics::ingest::{apply_report, init_alerts};
use gsm_sip_bridge::store::events;

/// The alert configuration is process-wide and the first call wins, so every
/// test here asks for the same one: thresholds of zero, every Discord category
/// off (the default), so nothing is ever sent.
fn init() {
    let mut cfg = AlertsConfig::default();
    cfg.registration_loss_thresholds.unhealthy_sec = 0;
    cfg.tunnel_failure_thresholds.unhealthy_sec = 0;
    init_alerts(
        cfg,
        DiscordClient::new(Secret::new(String::new()), "test".into()).unwrap(),
        Default::default(),
    );
}

/// The event sink is process-wide, so these tests take turns.
fn turn() -> &'static tokio::sync::Mutex<()> {
    static TURN: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    TURN.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn report(agent: AgentKind, module: &str, seq: u64, state: AgentState) -> AgentReport {
    AgentReport {
        agent,
        module_id: module.to_string(),
        epoch: 8800,
        seq,
        state,
        events: vec![],
        dropped: 0,
        details: None,
    }
}

fn kinds(path: &std::path::Path) -> Vec<(String, Option<i64>)> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare("SELECT kind, raise_id FROM events ORDER BY id")
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    rows
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recovery_is_recorded_even_when_discord_never_heard_of_the_outage() {
    // Registration-loss alerts are off, which is the default: nothing is ever
    // sent, so the Discord state machine never reaches "alerted".
    init();

    let _turn = turn().lock().await;
    let f = Fixture::start("").await;
    let store = open_store(&f.db_path);
    events::install_sink(store.sender());

    let module = "ingest-tl-1";
    let state = |registered| AgentState {
        registered: Some(registered),
        ..Default::default()
    };
    let wait_kinds = |n: usize| {
        let path = f.db_path.clone();
        async move {
            for _ in 0..200 {
                if kinds(&path).len() >= n {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("never reached {n} events: {:?}", kinds(&path));
        }
    };

    // Down long enough to count (threshold 0): the raise is recorded.
    apply_report(&report(AgentKind::Ims, module, 1, state(false)));
    apply_report(&report(AgentKind::Ims, module, 2, state(false)));
    wait_kinds(1).await;
    assert_eq!(kinds(&f.db_path)[0].0, "alert_raised:registration_loss");

    // Back up: the incident closes, with no Discord notice involved.
    apply_report(&report(AgentKind::Ims, module, 3, state(true)));
    wait_kinds(2).await;
    let rows = kinds(&f.db_path);
    assert_eq!(rows[1].0, "alert_resolved:registration_loss");
    assert!(rows[1].1.is_some(), "the resolve points at its raise");
    assert!(
        !events::open_raises()
            .iter()
            .any(|r| r.module_id.as_deref() == Some(module)),
        "nothing left open for this line"
    );

    // A later outage is its own incident, not folded into the first.
    apply_report(&report(AgentKind::Ims, module, 4, state(false)));
    apply_report(&report(AgentKind::Ims, module, 5, state(false)));
    wait_kinds(3).await;
    assert_eq!(kinds(&f.db_path)[2].0, "alert_raised:registration_loss");
    wait_for_rows(&f.db_path, "events", 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sip_sides_heartbeats_do_not_close_a_line_s_incident() {
    init();
    let _turn = turn().lock().await;
    let f = Fixture::start("").await;
    let store = open_store(&f.db_path);
    events::install_sink(store.sender());

    let module = "ingest-tl-2";
    let down = AgentState {
        registered: Some(false),
        ..Default::default()
    };
    apply_report(&report(AgentKind::Ims, module, 1, down));
    apply_report(&report(AgentKind::Ims, module, 2, down));
    wait_for_rows(&f.db_path, "events", 1);

    // The SIP-side agent never reports registration; its heartbeats must not
    // read as "registered again".
    for seq in 1..=3 {
        apply_report(&report(
            AgentKind::Sip,
            module,
            seq,
            AgentState {
                pbx_registered: Some(true),
                ..Default::default()
            },
        ));
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(kinds(&f.db_path).len(), 1, "{:?}", kinds(&f.db_path));
    assert!(events::open_raises()
        .iter()
        .any(|r| r.module_id.as_deref() == Some(module)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_volte_pdn_outage_closes_on_the_timeline_too() {
    init();
    let _turn = turn().lock().await;
    let f = Fixture::start("").await;
    let store = open_store(&f.db_path);
    events::install_sink(store.sender());

    // The cellular path reports its PDN as `tunnel_up`, with no supervisor.
    let module = "ingest-tl-volte";
    let pdn = |up| AgentState {
        tunnel_up: Some(up),
        ..Default::default()
    };
    apply_report(&report(AgentKind::Volte, module, 1, pdn(false)));
    apply_report(&report(AgentKind::Volte, module, 2, pdn(false)));
    wait_for_rows(&f.db_path, "events", 1);
    assert_eq!(kinds(&f.db_path)[0].0, "alert_raised:tunnel_failure");

    apply_report(&report(AgentKind::Volte, module, 3, pdn(true)));
    wait_for_rows(&f.db_path, "events", 2);
    let rows = kinds(&f.db_path);
    assert_eq!(rows[1].0, "alert_resolved:tunnel_failure");
    assert!(rows[1].1.is_some());
}
