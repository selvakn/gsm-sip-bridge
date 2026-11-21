// specs/122-management-webui, User Story 3: call totals, a filterable log in
// both directions, quality NULLs as JSON null, and live calls on the overview.

mod common;

use common::{open_store, wait_for_rows, Fixture};
use gsm_sip_bridge::control::protocol::{
    ActiveCallInfoWire, AgentDetails, AgentKind, AgentReport, AgentState, CallDirectionWire,
};
use gsm_sip_bridge::store::calls::{CallDirection, CallRecord};
use gsm_sip_bridge::store::{StoreCommand, Transport};
use jiff::{SignedDuration, Timestamp};

struct Seed {
    line: &'static str,
    direction: CallDirection,
    transport: Transport,
    status: &'static str,
    seconds: f64,
    age: SignedDuration,
    quality: Option<(f64, f64, f64)>,
}

fn record(s: &Seed) -> CallRecord {
    CallRecord {
        module_id: s.line.to_string(),
        caller_id: "+919000000001".to_string(),
        started_at: (Timestamp::now() - s.age).to_string(),
        duration_seconds: s.seconds,
        status: s.status.to_string(),
        sip_destination: String::new(),
        transport: s.transport,
        direction: s.direction,
        loss_percent: s.quality.map(|q| q.0),
        jitter_ms: s.quality.map(|q| q.1),
        rtt_ms: s.quality.map(|q| q.2),
    }
}

/// Today: two answered inbound (100s, 200s) on line-a, one missed inbound on
/// line-b, one failed outbound on line-b, one answered outbound (60s) on
/// line-a. Three days ago: one answered inbound (400s). Ten days ago: one
/// answered inbound that falls outside both windows.
fn seed(f: &Fixture) -> gsm_sip_bridge::store::StoreHandle {
    use CallDirection::{Inbound, Outbound};
    let store = open_store(&f.db_path);
    let tx = store.sender();
    let rows = [
        Seed {
            line: "line-a",
            direction: Inbound,
            transport: Transport::Vowifi,
            status: "answered",
            seconds: 100.0,
            age: SignedDuration::from_secs(30),
            quality: Some((0.5, 12.0, 80.0)),
        },
        Seed {
            line: "line-a",
            direction: Inbound,
            transport: Transport::Vowifi,
            status: "answered",
            seconds: 200.0,
            age: SignedDuration::from_secs(20),
            quality: None,
        },
        Seed {
            line: "line-b",
            direction: Inbound,
            transport: Transport::Cs,
            status: "missed",
            seconds: 0.0,
            age: SignedDuration::from_secs(10),
            quality: None,
        },
        Seed {
            line: "line-b",
            direction: Outbound,
            transport: Transport::Cs,
            status: "failed",
            seconds: 0.0,
            age: SignedDuration::from_secs(5),
            quality: None,
        },
        Seed {
            line: "line-a",
            direction: Outbound,
            transport: Transport::Vowifi,
            status: "answered",
            seconds: 60.0,
            age: SignedDuration::from_secs(2),
            quality: None,
        },
        Seed {
            line: "line-a",
            direction: Inbound,
            transport: Transport::Vowifi,
            status: "answered",
            seconds: 400.0,
            age: SignedDuration::from_hours(72),
            quality: None,
        },
        Seed {
            line: "line-a",
            direction: Inbound,
            transport: Transport::Vowifi,
            status: "answered",
            seconds: 999.0,
            age: SignedDuration::from_hours(240),
            quality: None,
        },
    ];
    for r in &rows {
        tx.send(StoreCommand::InsertCall(record(r))).unwrap();
    }
    wait_for_rows(&f.db_path, "calls", 7);
    store
}

#[tokio::test]
async fn the_summary_counts_today_and_seven_days_by_line_and_direction() {
    let f = Fixture::start("").await;
    let _store = seed(&f);
    let cookie = f.login().await;
    let (status, s) = f.get_json(&cookie, "/api/calls/summary").await;
    assert_eq!(status, 200);

    assert_eq!(s["today"]["answered"], 3);
    assert_eq!(s["today"]["missed"], 1);
    assert_eq!(s["today"]["failed"], 1);
    assert_eq!(s["today"]["avg_answered_seconds"], 120.0); // (100 + 200 + 60) / 3

    assert_eq!(
        s["last_7d"]["answered"], 4,
        "the 10-day-old call is outside the window"
    );
    assert_eq!(s["last_7d"]["avg_answered_seconds"], 190.0); // (100 + 200 + 60 + 400) / 4

    let by_line = s["by_line"].as_array().unwrap();
    let a = by_line.iter().find(|l| l["module_id"] == "line-a").unwrap();
    assert_eq!(a["today"]["answered"], 3);
    assert_eq!(a["last_7d"]["answered"], 4);
    let b = by_line.iter().find(|l| l["module_id"] == "line-b").unwrap();
    assert_eq!(b["today"]["missed"], 1);
    assert_eq!(b["today"]["failed"], 1);
    assert!(
        b["today"]["avg_answered_seconds"].is_null(),
        "nothing answered, so no average"
    );

    assert_eq!(s["by_direction"]["inbound"]["today"]["answered"], 2);
    assert_eq!(s["by_direction"]["inbound"]["last_7d"]["answered"], 3);
    assert_eq!(s["by_direction"]["outbound"]["today"]["answered"], 1);
    assert_eq!(s["by_direction"]["outbound"]["today"]["failed"], 1);
}

#[tokio::test]
async fn the_log_filters_by_every_field_and_comes_newest_first() {
    let f = Fixture::start("").await;
    let _store = seed(&f);
    let cookie = f.login().await;

    let (_, all) = f.get_json(&cookie, "/api/calls").await;
    assert_eq!(all["total"], 7);
    let items = all["items"].as_array().unwrap();
    assert_eq!(
        items[0]["direction"], "outbound",
        "the newest call is the outbound answered one"
    );
    assert_eq!(items[6]["duration_seconds"], 999.0);

    for (query, expected) in [
        ("line=line-b", 2),
        ("direction=outbound", 2),
        ("direction=inbound", 5),
        ("transport=cs", 2),
        ("transport=vowifi", 5),
        ("status=answered", 5),
        ("status=missed", 1),
        ("line=line-a&direction=inbound&status=answered", 4),
    ] {
        let (_, body) = f.get_json(&cookie, &format!("/api/calls?{query}")).await;
        assert_eq!(body["total"], expected, "{query}");
    }

    let from = (Timestamp::now() - SignedDuration::from_hours(120)).to_string();
    let (_, recent) = f
        .get_json(
            &cookie,
            &format!("/api/calls?from={}", from.replace('+', "%2B")),
        )
        .await;
    assert_eq!(recent["total"], 6, "from= excludes the 10-day-old call");

    let to = (Timestamp::now() - SignedDuration::from_hours(24)).to_string();
    let (_, old) = f
        .get_json(
            &cookie,
            &format!("/api/calls?to={}", to.replace('+', "%2B")),
        )
        .await;
    assert_eq!(old["total"], 2, "to= keeps only the calls before it");

    let (_, page) = f.get_json(&cookie, "/api/calls?per_page=3&page=3").await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["total"], 7);

    let (status, _) = f.get_json(&cookie, "/api/calls?from=yesterday").await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn unknown_quality_is_json_null_and_known_quality_is_a_number() {
    let f = Fixture::start("").await;
    let _store = seed(&f);
    let cookie = f.login().await;
    let (_, all) = f
        .get_json(
            &cookie,
            "/api/calls?direction=inbound&status=answered&line=line-a",
        )
        .await;
    let items = all["items"].as_array().unwrap();

    let with = items
        .iter()
        .find(|c| c["duration_seconds"] == 100.0)
        .unwrap();
    assert_eq!(with["loss_percent"], 0.5);
    assert_eq!(with["jitter_ms"], 12.0);
    assert_eq!(with["rtt_ms"], 80.0);
    assert_eq!(with["remote"], "+919000000001");

    let without = items
        .iter()
        .find(|c| c["duration_seconds"] == 200.0)
        .unwrap();
    assert!(without["loss_percent"].is_null());
    assert!(without["jitter_ms"].is_null());
    assert!(without["rtt_ms"].is_null());
}

#[tokio::test]
async fn a_call_in_progress_appears_on_its_overview_line() {
    let f = Fixture::start("").await;
    gsm_sip_bridge::metrics::ingest::apply_report(&AgentReport {
        agent: AgentKind::Ims,
        module_id: "calls-live-1".into(),
        epoch: 77,
        seq: 1,
        state: AgentState {
            registered: Some(true),
            active_calls: Some(1),
            ..Default::default()
        },
        events: vec![],
        dropped: 0,
        details: Some(
            AgentDetails::default().with_active_calls(vec![ActiveCallInfoWire {
                remote: "+919000000001".into(),
                direction: CallDirectionWire::Inbound,
                started_at_unix: 1_999_999_000,
                answered: true,
                loss_percent: Some(0.4),
                jitter_ms: Some(11.0),
                rtt_ms: None,
            }]),
        ),
    });
    let cookie = f.login().await;
    let (_, overview) = f.get_json(&cookie, "/api/overview").await;
    let line = overview["lines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == "calls-live-1")
        .unwrap();
    let call = &line["active_calls"][0];
    assert_eq!(call["remote"], "+919000000001");
    assert_eq!(call["direction"], "inbound");
    assert_eq!(call["loss_percent"], 0.4);
    assert!(call["rtt_ms"].is_null(), "unmeasured stays null, never 0");
}

#[tokio::test]
async fn the_upper_date_bound_includes_its_own_second() {
    let f = Fixture::start("").await;
    let store = open_store(&f.db_path);
    let at: Timestamp = "2026-10-20T23:59:59Z".parse().unwrap();
    // Stored the way the bridge writes it: with a zone suffix.
    store
        .sender()
        .send(StoreCommand::InsertCall(CallRecord {
            started_at: at.to_string(),
            ..record(&Seed {
                line: "line-a",
                direction: CallDirection::Inbound,
                transport: Transport::Vowifi,
                status: "answered",
                seconds: 5.0,
                age: SignedDuration::ZERO,
                quality: None,
            })
        }))
        .unwrap();
    wait_for_rows(&f.db_path, "calls", 1);
    let cookie = f.login().await;

    let count = |to: &str| {
        let path = format!("/api/calls?to={}", to.replace('+', "%2B"));
        let f = &f;
        let cookie = cookie.clone();
        async move {
            f.get_json(&cookie, &path).await.1["total"]
                .as_u64()
                .unwrap()
        }
    };
    assert_eq!(
        count("2026-10-20T23:59:59Z").await,
        1,
        "the day's last second is in"
    );
    assert_eq!(count("2026-10-20T23:59:59.500Z").await, 1);
    assert_eq!(
        count("2026-10-20T23:59:58Z").await,
        0,
        "the second before it is not"
    );
    assert_eq!(
        count("2026-10-21T05:29:59+05:30").await,
        1,
        "an offset is honoured"
    );
}

#[tokio::test]
async fn calls_from_before_direction_was_recorded_are_unknown_not_inbound() {
    let f = Fixture::start("").await;
    let store = open_store(&f.db_path);
    // A row written without a direction, as every pre-upgrade row was.
    let conn = rusqlite::Connection::open(&f.db_path).unwrap();
    conn.execute(
        "INSERT INTO calls (module_id, caller_id, started_at, duration_seconds, status, sip_destination, transport) \
         VALUES ('line-a', '+919000000001', ?1, 30.0, 'answered', '', 'vowifi')",
        [Timestamp::now().to_string()],
    )
    .unwrap();
    drop(store);
    let cookie = f.login().await;

    let (_, s) = f.get_json(&cookie, "/api/calls/summary").await;
    assert_eq!(s["today"]["answered"], 1, "it still counts in the totals");
    assert_eq!(s["by_direction"]["unknown"]["today"]["answered"], 1);
    assert_eq!(s["by_direction"]["inbound"]["today"]["answered"], 0);
    assert_eq!(s["by_direction"]["outbound"]["today"]["answered"], 0);

    let (_, list) = f.get_json(&cookie, "/api/calls?direction=unknown").await;
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["direction"], "unknown");
    let (_, inbound) = f.get_json(&cookie, "/api/calls?direction=inbound").await;
    assert_eq!(inbound["total"], 0);
}

#[tokio::test]
async fn the_summary_counts_today_from_the_viewers_midnight() {
    let f = Fixture::start("").await;
    let _store = seed(&f);
    let cookie = f.login().await;
    let summary = |since: Timestamp| format!("/api/calls/summary?since={since:.0}");

    // A "midnight" in the future is clamped to now: nothing is today yet.
    let (status, s) = f
        .get_json(
            &cookie,
            &summary(Timestamp::now() + SignedDuration::from_hours(1)),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(s["today"]["answered"], 0);
    assert_eq!(
        s["last_7d"]["answered"], 4,
        "the 7-day window ignores since"
    );

    // One more than two days back is clamped to two days: the 3-day-old call
    // stays out of today.
    let (_, s) = f
        .get_json(
            &cookie,
            &summary(Timestamp::now() - SignedDuration::from_hours(72)),
        )
        .await;
    assert_eq!(s["today"]["answered"], 3);
}

#[tokio::test]
async fn the_summary_rejects_a_since_that_is_not_an_rfc3339_time() {
    let f = Fixture::start("").await;
    let cookie = f.login().await;
    for bad in ["yesterday", "2026-10-20T00:00:00"] {
        let (status, body) = f
            .get_json(&cookie, &format!("/api/calls/summary?since={bad}"))
            .await;
        assert_eq!(status, 400, "{bad}: {body}");
    }
}
