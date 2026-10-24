//! End-to-end retry of undelivered SMS Discord notifications
//! (specs/121-sms-alert-retry). Discord itself can't be reached from CI, so a
//! local wiremock server stands in for the webhook; the real `reqwest` client,
//! the real SQLite store and the real writer thread are all exercised. Failure
//! is simulated with HTTP 400 (no client-side retries) so tests stay fast.

use gsm_sip_bridge::config::secret::Secret;
use gsm_sip_bridge::config::SmsConfig;
use gsm_sip_bridge::sms::discord::DiscordClient;
use gsm_sip_bridge::sms::record_and_forward;
use gsm_sip_bridge::sms::retry::{self, SmsForwarder};
use gsm_sip_bridge::store::schema::init_schema;
use gsm_sip_bridge::store::sms::{insert_sms, SmsRecord};
use gsm_sip_bridge::store::{StoreHandle, Transport};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Rig {
    _dir: tempfile::TempDir,
    db: PathBuf,
    server: MockServer,
}

impl Rig {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("store.db");
        init_schema(&Connection::open(&db).unwrap()).unwrap();
        Rig {
            _dir: dir,
            db,
            server: MockServer::start().await,
        }
    }

    fn conn(&self) -> Connection {
        let c = Connection::open(&self.db).unwrap();
        c.busy_timeout(Duration::from_secs(5)).unwrap();
        c
    }

    fn forwarder_with(&self, enabled: bool, max_age: u64) -> SmsForwarder {
        let cfg = SmsConfig {
            enabled: true,
            discord_webhook_url: Secret::new(self.server.uri()),
            db_path: self.db.to_string_lossy().into_owned(),
            retry_enabled: enabled,
            retry_interval_sec: 3600,
            retry_max_age_sec: max_age,
        };
        let client = DiscordClient::new(Secret::new(self.server.uri()), "test".into()).unwrap();
        SmsForwarder::new(client, &cfg, HashMap::new())
    }

    fn forwarder(&self) -> SmsForwarder {
        self.forwarder_with(true, 7 * 86_400)
    }

    /// Discord answers every post with `status` from now on.
    async fn respond(&self, status: u16) {
        self.server.reset().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&self.server)
            .await;
    }

    async fn bodies(&self) -> Vec<serde_json::Value> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }

    /// A text whose first attempt failed `age_secs` ago, as the daemon would
    /// have left it.
    fn seed_failed(&self, body: &str, age_secs: i64) -> i64 {
        let conn = self.conn();
        insert_sms(
            &conn,
            &SmsRecord {
                module_id: "ec20-A1B2C3".into(),
                sender: "+919000000000".into(),
                body: body.into(),
                received_at: format!("2026-01-01T00:00:00Z#{body}"),
                forwarding_status: "pending".into(),
                transport: Transport::Cs,
                retry_eligible: true,
            },
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        let seen = chrono::Utc::now().timestamp() - age_secs;
        conn.execute(
            "UPDATE sms SET forwarding_status='failed', last_error='http_400', claimed_until=NULL, \
             first_seen_at=?1 WHERE id=?2",
            rusqlite::params![seen, id],
        )
        .unwrap();
        id
    }

    fn row(&self, id: i64) -> (String, i64, Option<String>, Option<i64>) {
        self.conn()
            .query_row(
                "SELECT forwarding_status, attempt_count, last_error, claimed_until FROM sms WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
    }

    fn status_of(&self, id: i64) -> String {
        self.row(id).0
    }
}

fn description(b: &serde_json::Value) -> &str {
    b["embeds"][0]["description"].as_str().unwrap()
}

fn title(b: &serde_json::Value) -> &str {
    b["embeds"][0]["title"].as_str().unwrap()
}

/// Feeds one fresh text through the production entry point and waits until its
/// first attempt (and the follow-up sweep) has finished.
async fn receive(rig: &Rig, store: &StoreHandle, fwd: Option<SmsForwarder>, body: &str) {
    let handle = tokio::runtime::Handle::current();
    record_and_forward(
        &handle,
        store.sender(),
        fwd,
        "ec20-A1B2C3".into(),
        "+919000000000".into(),
        body.into(),
        format!("2026-02-02T00:00:00Z#{body}"),
        Transport::Cs,
        None,
        None,
    );
    for _ in 0..400 {
        let st: Option<String> = rig
            .conn()
            .query_row(
                "SELECT forwarding_status FROM sms WHERE body=?1",
                [body],
                |r| r.get(0),
            )
            .ok();
        if matches!(st.as_deref(), Some(s) if s != "pending") {
            // Let the trailing sweep drain too.
            tokio::time::sleep(Duration::from_millis(300)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("text {body} never left pending");
}

#[tokio::test]
async fn a_failed_text_is_recorded_then_delivered_once_by_a_sweep() {
    let rig = Rig::new().await;
    let store = StoreHandle::open(&rig.db).unwrap();
    rig.respond(400).await;

    receive(&rig, &store, Some(rig.forwarder()), "first").await;
    let id: i64 = rig
        .conn()
        .query_row("SELECT id FROM sms WHERE body='first'", [], |r| r.get(0))
        .unwrap();
    let (st, attempts, err, lease) = rig.row(id);
    assert_eq!(st, "failed");
    assert_eq!(
        attempts, 1,
        "the failed text is not retried by its own alert"
    );
    assert!(err.unwrap().contains("400"));
    assert_eq!(lease, None);
    assert_eq!(rig.bodies().await.len(), 1);

    rig.respond(204).await;
    let out = retry::sweep(&rig.forwarder(), &None).await;
    assert_eq!((out.attempted, out.delivered), (1, 1));
    assert_eq!(rig.status_of(id), "sent");
    let posts = rig.bodies().await;
    assert_eq!(posts.len(), 1);
    assert_eq!(title(&posts[0]), "[Late] SMS from +919000000000");
    assert_eq!(description(&posts[0]), "first");

    let again = retry::sweep(&rig.forwarder(), &None).await;
    assert_eq!(again.attempted, 0);
    assert_eq!(
        rig.bodies().await.len(),
        1,
        "a delivered text is never re-sent"
    );
    store.shutdown();
}

#[tokio::test]
async fn a_retry_that_fails_again_stays_failed_and_counts_the_attempt() {
    let rig = Rig::new().await;
    rig.respond(400).await;
    let id = rig.seed_failed("stuck", 60);

    let out = retry::sweep(&rig.forwarder(), &None).await;
    assert_eq!((out.attempted, out.still_failing), (1, 1));
    let (st, attempts, err, lease) = rig.row(id);
    assert_eq!((st.as_str(), attempts, lease), ("failed", 2, None));
    assert!(err.unwrap().contains("400"));
}

#[tokio::test]
async fn a_new_text_triggers_delivery_of_an_older_failed_one() {
    let rig = Rig::new().await;
    let store = StoreHandle::open(&rig.db).unwrap();
    let old = rig.seed_failed("older", 600);
    rig.respond(204).await;

    receive(&rig, &store, Some(rig.forwarder()), "newer").await;

    assert_eq!(rig.status_of(old), "sent");
    let posts = rig.bodies().await;
    assert_eq!(posts.len(), 2);
    assert_eq!(description(&posts[0]), "newer", "the fresh text goes first");
    assert_eq!(title(&posts[0]), "SMS from +919000000000");
    assert_eq!(description(&posts[1]), "older");
    assert!(title(&posts[1]).starts_with("[Late] "));
    store.shutdown();
}

#[tokio::test]
async fn with_retry_disabled_nothing_is_ever_retried() {
    let rig = Rig::new().await;
    let store = StoreHandle::open(&rig.db).unwrap();
    rig.respond(400).await;
    receive(
        &rig,
        &store,
        Some(rig.forwarder_with(false, 86_400)),
        "once",
    )
    .await;
    rig.respond(204).await;

    let fwd = rig.forwarder_with(false, 86_400);
    assert_eq!(retry::sweep(&fwd, &None).await.attempted, 0);
    assert!(retry::spawn_timer(&tokio::runtime::Handle::current(), fwd, None).is_none());
    let eligible: i64 = rig
        .conn()
        .query_row(
            "SELECT retry_eligible FROM sms WHERE body='once'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(eligible, 0);
    assert!(rig.bodies().await.is_empty());
    store.shutdown();
}

#[tokio::test]
async fn texts_received_without_a_webhook_are_skipped_forever() {
    let rig = Rig::new().await;
    let store = StoreHandle::open(&rig.db).unwrap();
    let handle = tokio::runtime::Handle::current();
    record_and_forward(
        &handle,
        store.sender(),
        None,
        "ec20-A1B2C3".into(),
        "+919000000000".into(),
        "no-webhook".into(),
        "2026-02-02T00:00:00Z".into(),
        Transport::Cs,
        None,
        None,
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.respond(204).await;

    // A webhook is configured later: the old text must not be replayed.
    assert_eq!(retry::sweep(&rig.forwarder(), &None).await.attempted, 0);
    let st: String = rig
        .conn()
        .query_row("SELECT forwarding_status FROM sms", [], |r| r.get(0))
        .unwrap();
    assert_eq!(st, "skipped");
    assert!(rig.bodies().await.is_empty());
    store.shutdown();
}

// ---- timer ---------------------------------------------------------------

/// Paused clock, no HTTP: the observable effect of a sweep is that a text past
/// `retry_max_age_sec` becomes `abandoned`, which needs no network (a paused
/// clock and live sockets don't mix).
#[tokio::test(start_paused = true)]
async fn the_timer_first_fires_one_full_interval_after_start() {
    let rig = Rig::new().await;
    let id = rig.seed_failed("old", 10_000);
    let fwd = rig.forwarder_with(true, 5_000);
    let handle = tokio::runtime::Handle::current();
    let task = retry::spawn_timer(&handle, fwd, None).expect("timer");

    // Let the timer task start (and fix its first deadline) before the clock
    // moves; otherwise the deadline is computed from the advanced time.
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_secs(3599)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        rig.status_of(id),
        "failed",
        "no sweep before the first tick"
    );

    tokio::time::advance(Duration::from_secs(2)).await;
    let mut abandoned = false;
    for _ in 0..300 {
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(10));
        if rig.status_of(id) == "abandoned" {
            abandoned = true;
            break;
        }
    }
    assert!(abandoned, "the first tick sweeps");
    task.abort();
}

// ---- restart & concurrency ----------------------------------------------

#[tokio::test]
async fn failed_texts_survive_a_restart_and_are_delivered_once() {
    let rig = Rig::new().await;
    rig.respond(400).await;
    let a = rig.seed_failed("a", 300);
    let b = rig.seed_failed("b", 200);
    // "Restart": nothing in memory survives; only the DB file does.
    rig.respond(204).await;
    let out = retry::sweep(&rig.forwarder(), &None).await;
    assert_eq!(out.delivered, 2);
    assert_eq!(
        (rig.status_of(a), rig.status_of(b)),
        ("sent".into(), "sent".into())
    );
    assert_eq!(rig.bodies().await.len(), 2);
    assert_eq!(retry::sweep(&rig.forwarder(), &None).await.attempted, 0);
}

#[tokio::test]
async fn a_row_orphaned_by_a_crash_is_picked_up_once_its_lease_expires() {
    let rig = Rig::new().await;
    rig.respond(204).await;
    let dead = rig.seed_failed("crashed", 300);
    let live = rig.seed_failed("in-flight", 300);
    let now = chrono::Utc::now().timestamp();
    let conn = rig.conn();
    conn.execute(
        "UPDATE sms SET forwarding_status='pending', claimed_until=?1 WHERE id=?2",
        rusqlite::params![now - 5, dead],
    )
    .unwrap();
    conn.execute(
        "UPDATE sms SET forwarding_status='pending', claimed_until=?1 WHERE id=?2",
        rusqlite::params![now + 100, live],
    )
    .unwrap();

    let out = retry::sweep(&rig.forwarder(), &None).await;
    assert_eq!(out.delivered, 1);
    assert_eq!(rig.status_of(dead), "sent");
    assert_eq!(rig.status_of(live), "pending", "a live lease is respected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sweeps_post_each_text_exactly_once() {
    let rig = Rig::new().await;
    rig.respond(204).await;
    for i in 0..8 {
        rig.seed_failed(&format!("m{i}"), 100 + i);
    }
    let (f1, f2) = (rig.forwarder(), rig.forwarder());
    let (a, b) = tokio::join!(retry::sweep(&f1, &None), retry::sweep(&f2, &None));
    assert_eq!(a.delivered + b.delivered, 8);

    let mut seen: Vec<String> = rig
        .bodies()
        .await
        .iter()
        .map(|p| description(p).to_string())
        .collect();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 8);
    assert_eq!(rig.bodies().await.len(), 8);
}

// ---- backlog politeness --------------------------------------------------

#[tokio::test]
async fn a_rate_limit_stops_the_pass_and_leaves_the_rest_untouched() {
    let rig = Rig::new().await;
    let ids: Vec<i64> = (0..5)
        .map(|i| rig.seed_failed(&format!("r{i}"), 500 - i))
        .collect();
    // Two accepted, then Discord throttles. `Retry-After: 0` keeps the
    // client's own internal retries instant.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(204))
        .up_to_n_times(2)
        .mount(&rig.server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .mount(&rig.server)
        .await;

    let out = retry::sweep(&rig.forwarder(), &None).await;
    assert!(out.stopped_early);
    assert_eq!((out.delivered, out.still_failing), (2, 1));
    assert_eq!(rig.status_of(ids[0]), "sent");
    assert_eq!(rig.status_of(ids[1]), "sent");
    let (st, attempts, _, lease) = rig.row(ids[2]);
    assert_eq!((st.as_str(), lease), ("failed", None));
    assert_eq!(attempts, 2);
    for id in &ids[3..] {
        let (st, attempts, _, lease) = rig.row(*id);
        assert_eq!((st.as_str(), attempts, lease), ("failed", 1, None));
    }
}

#[tokio::test]
async fn a_large_backlog_drains_oldest_first_across_passes() {
    let rig = Rig::new().await;
    rig.respond(204).await;
    for i in 0..120 {
        rig.seed_failed(&format!("b{i:03}"), 10_000 - i);
    }
    let fwd = rig.forwarder();
    let first = retry::sweep(&fwd, &None).await;
    assert_eq!(first.attempted, 50, "one pass is bounded");
    while retry::sweep(&fwd, &None).await.attempted > 0 {}

    let posts = rig.bodies().await;
    assert_eq!(posts.len(), 120);
    let order: Vec<&str> = posts.iter().map(description).collect();
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(order, sorted, "oldest first");
    for p in &posts {
        assert!(title(p).starts_with("[Late] "));
        let fields = p["embeds"][0]["fields"].as_array().unwrap();
        assert!(fields.iter().any(|f| f["name"] == "Originally received"));
    }
}

#[tokio::test]
async fn a_fresh_text_is_never_queued_behind_a_backlog() {
    let rig = Rig::new().await;
    let store = StoreHandle::open(&rig.db).unwrap();
    rig.respond(204).await;
    for i in 0..30 {
        rig.seed_failed(&format!("old{i:02}"), 5_000 - i);
    }
    receive(&rig, &store, Some(rig.forwarder()), "fresh").await;

    let posts = rig.bodies().await;
    assert_eq!(description(&posts[0]), "fresh");
    assert_eq!(title(&posts[0]), "SMS from +919000000000");
    assert!(posts[1..].iter().all(|p| title(p).starts_with("[Late] ")));
    store.shutdown();
}

#[tokio::test]
async fn texts_older_than_the_max_age_are_abandoned_not_posted() {
    let rig = Rig::new().await;
    rig.respond(204).await;
    let stale = rig.seed_failed("stale", 10_000);
    let young = rig.seed_failed("young", 10);

    let out = retry::sweep(&rig.forwarder_with(true, 3_600), &None).await;
    assert_eq!((out.abandoned, out.delivered), (1, 1));
    let (st, _, err, _) = rig.row(stale);
    assert_eq!(st, "abandoned");
    assert_eq!(
        err.as_deref(),
        Some("http_400"),
        "the last error is preserved"
    );
    assert_eq!(rig.status_of(young), "sent");
    let posts = rig.bodies().await;
    assert_eq!(posts.len(), 1);
    assert_eq!(description(&posts[0]), "young");
}

#[tokio::test]
async fn a_long_retry_after_stops_the_pass_without_waiting_it_out() {
    let rig = Rig::new().await;
    let ids: Vec<i64> = (0..3)
        .map(|i| rig.seed_failed(&format!("t{i}"), 500 - i))
        .collect();
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
        .mount(&rig.server)
        .await;

    let started = std::time::Instant::now();
    let out = retry::sweep(&rig.forwarder(), &None).await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "must not sleep the pause"
    );
    assert!(out.stopped_early);
    assert_eq!(out.attempted, 1);
    assert_eq!(rig.bodies().await.len(), 1, "no further rows are posted");
    for id in &ids[1..] {
        assert_eq!(
            rig.row(*id).1,
            1,
            "untouched rows are not counted as attempts"
        );
    }
}

#[tokio::test]
async fn a_text_that_ages_out_during_a_pass_is_abandoned_not_posted() {
    let rig = Rig::new().await;
    let first = rig.seed_failed("first", 10);
    let second = rig.seed_failed("second", 999);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(204).set_delay(Duration::from_millis(2500)))
        .mount(&rig.server)
        .await;

    // `second` is 999 s old against a 1000 s limit when the pass starts, and
    // past it by the time the slow first post returns.
    let out = retry::sweep(&rig.forwarder_with(true, 1_000), &None).await;
    assert_eq!(rig.status_of(first), "sent");
    assert_eq!(rig.status_of(second), "abandoned");
    assert_eq!((out.delivered, out.abandoned), (1, 1));
    assert_eq!(rig.bodies().await.len(), 1);
}

#[tokio::test]
async fn a_throttled_webhook_is_left_alone_until_its_pause_has_passed() {
    let rig = Rig::new().await;
    rig.seed_failed("older", 500);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
        .mount(&rig.server)
        .await;
    let fwd = rig.forwarder();

    assert!(retry::sweep(&fwd, &None).await.stopped_early);
    assert_eq!(rig.bodies().await.len(), 1);

    // Later triggers (next alert, timer) inside the pause send nothing.
    let again = retry::sweep(&fwd, &None).await;
    assert_eq!(again.attempted, 0);
    assert!(again.stopped_early);
    assert_eq!(rig.bodies().await.len(), 1);
}

#[tokio::test]
async fn a_fresh_text_that_is_throttled_holds_back_the_backlog() {
    let rig = Rig::new().await;
    let store = StoreHandle::open(&rig.db).unwrap();
    let old = rig.seed_failed("older", 500);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
        .mount(&rig.server)
        .await;

    receive(&rig, &store, Some(rig.forwarder()), "fresh").await;

    let posts = rig.bodies().await;
    assert_eq!(posts.len(), 1, "only the fresh text's own attempt");
    assert_eq!(description(&posts[0]), "fresh");
    assert_eq!(rig.row(old).1, 1, "the older text was not touched");
    store.shutdown();
}

#[tokio::test]
async fn expiry_still_runs_while_the_webhook_is_cooling_down() {
    let rig = Rig::new().await;
    let stale = rig.seed_failed("stale", 10_000);
    rig.seed_failed("recent", 10);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
        .mount(&rig.server)
        .await;
    let fwd = rig.forwarder_with(true, 5_000);
    // Trip the cooldown with the recent text.
    retry::sweep(&fwd, &None).await;
    let stale_now = rig.seed_failed("stale2", 10_000);
    let posts = rig.bodies().await.len();

    let out = retry::sweep(&fwd, &None).await;
    assert!(out.stopped_early);
    assert_eq!(out.attempted, 0);
    assert_eq!(
        rig.bodies().await.len(),
        posts,
        "nothing posted while cooling"
    );
    assert_eq!(rig.status_of(stale_now), "abandoned");
    let _ = stale;
}
