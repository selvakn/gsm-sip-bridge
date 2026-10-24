//! Delivery-state storage for SMS Discord retry (specs/121-sms-alert-retry):
//! the v4→v5 migration and the atomic claim/transition queries. Real SQLite
//! files in a temp dir — the cross-connection races are the point.

use gsm_sip_bridge::store::schema::init_schema;
use gsm_sip_bridge::store::sms::{
    abandon_expired, candidates, claim_for_retry, insert_sms, mark_failed, mark_sent,
    release_claim, summary, SmsRecord,
};
use gsm_sip_bridge::store::Transport;
use rusqlite::Connection;
use std::sync::{Arc, Barrier};

const NOW: i64 = 1_800_000_000;

fn open(dir: &tempfile::TempDir) -> Connection {
    let conn = Connection::open(dir.path().join("store.db")).unwrap();
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    conn
}

fn fresh(dir: &tempfile::TempDir) -> Connection {
    let conn = open(dir);
    init_schema(&conn).unwrap();
    conn
}

/// Inserts a text the way the daemon does and returns its row id.
fn add(conn: &Connection, status: &str, eligible: bool, received_at: &str) -> i64 {
    insert_sms(
        conn,
        &SmsRecord {
            module_id: "ec20-A1B2C3".into(),
            sender: "+919000000000".into(),
            body: "hello".into(),
            received_at: received_at.into(),
            forwarding_status: status.into(),
            transport: Transport::Cs,
            retry_eligible: eligible,
        },
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// Makes a row look like it failed `age_secs` ago with no live lease.
fn settle(conn: &Connection, id: i64, status: &str, age_secs: i64) {
    conn.execute(
        "UPDATE sms SET forwarding_status = ?1, claimed_until = NULL, first_seen_at = ?2 WHERE id = ?3",
        rusqlite::params![status, NOW - age_secs, id],
    )
    .unwrap();
}

fn version(conn: &Connection) -> String {
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'schema_version'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn a_v4_database_migrates_with_every_row_history_only() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(&dir);
    conn.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO meta VALUES ('schema_version','4');
         CREATE TABLE sms (
             id INTEGER PRIMARY KEY AUTOINCREMENT, module_id TEXT NOT NULL, sender TEXT NOT NULL,
             body TEXT NOT NULL, received_at TEXT NOT NULL,
             forwarding_status TEXT NOT NULL CHECK (forwarding_status IN ('pending','sent','failed','skipped')),
             forwarded_at TEXT, discord_status_code INTEGER,
             transport TEXT NOT NULL DEFAULT 'cs' CHECK (transport IN ('cs','vowifi','volte')));
         INSERT INTO sms (module_id,sender,body,received_at,forwarding_status,discord_status_code)
             VALUES ('m','+919000000000','a','t1','sent',204),
                    ('m','+919000000000','b','t2','failed',500),
                    ('m','+919000000000','c','t3','pending',NULL),
                    ('m','+919000000000','d','t4','skipped',NULL);",
    )
    .unwrap();

    init_schema(&conn).unwrap();

    assert_eq!(version(&conn), "5");
    let (n, eligible): (i64, i64) = conn
        .query_row("SELECT COUNT(*), SUM(retry_eligible) FROM sms", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((n, eligible), (4, 0));
    let code: i64 = conn
        .query_row(
            "SELECT discord_status_code FROM sms WHERE body='b'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(code, 500);
    // Pre-v5 failures are history: never offered for retry, never abandoned.
    assert!(candidates(&conn, NOW, 10).unwrap().is_empty());
    assert_eq!(abandon_expired(&conn, NOW + 10_000_000, 1).unwrap(), 0);
    assert_eq!(summary(&conn, NOW).unwrap().counts.legacy_failed, 1);
}

#[test]
fn a_fresh_database_is_v5_and_init_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh(&dir);
    assert_eq!(version(&conn), "5");
    init_schema(&conn).unwrap();
    assert_eq!(version(&conn), "5");
}

#[test]
fn the_status_check_accepts_abandoned() {
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh(&dir);
    let id = add(&conn, "pending", true, "t1");
    settle(&conn, id, "abandoned", 0);
    let s: String = conn
        .query_row("SELECT forwarding_status FROM sms WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(s, "abandoned");
}

#[test]
fn two_connections_racing_for_one_row_yield_exactly_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let conn = fresh(&dir);
        let id = add(&conn, "pending", true, "t1");
        settle(&conn, id, "failed", 60);
        id
    };
    let path = dir.path().join("store.db");
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let (path, barrier) = (path.clone(), barrier.clone());
            std::thread::spawn(move || {
                let conn = Connection::open(path).unwrap();
                conn.busy_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                barrier.wait();
                claim_for_retry(&conn, id, NOW, 120).unwrap()
            })
        })
        .collect();
    let wins = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|w| *w)
        .count();
    assert_eq!(wins, 1);
}

#[test]
fn a_live_lease_blocks_a_claim_and_an_expired_one_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh(&dir);
    let id = add(&conn, "pending", true, "t1");
    settle(&conn, id, "failed", 60);

    assert!(claim_for_retry(&conn, id, NOW, 120).unwrap());
    assert!(!claim_for_retry(&conn, id, NOW + 60, 120).unwrap());
    assert!(candidates(&conn, NOW + 60, 10).unwrap().is_empty());
    assert!(claim_for_retry(&conn, id, NOW + 121, 120).unwrap());

    let attempts: i64 = conn
        .query_row("SELECT attempt_count FROM sms WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    // One from the insert (first attempt) plus the two successful claims.
    assert_eq!(attempts, 3);
}

#[test]
fn release_claim_makes_the_row_claimable_again() {
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh(&dir);
    let id = add(&conn, "pending", true, "t1");
    settle(&conn, id, "failed", 60);
    assert!(claim_for_retry(&conn, id, NOW, 120).unwrap());
    release_claim(&conn, id).unwrap();
    assert!(claim_for_retry(&conn, id, NOW, 120).unwrap());
}

#[test]
fn only_undelivered_eligible_rows_are_candidates_oldest_first() {
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh(&dir);
    let a = add(&conn, "pending", true, "t1");
    let sent = add(&conn, "pending", true, "t2");
    let skipped = add(&conn, "skipped", false, "t3");
    let gone = add(&conn, "pending", true, "t4");
    let b = add(&conn, "pending", true, "t5");
    let legacy = add(&conn, "failed", false, "t6");
    settle(&conn, a, "failed", 10);
    settle(&conn, sent, "sent", 10);
    settle(&conn, gone, "abandoned", 10);
    settle(&conn, b, "pending", 10);
    let _ = (skipped, legacy);

    let ids: Vec<i64> = candidates(&conn, NOW, 10)
        .unwrap()
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(ids, vec![a, b]);
    assert_eq!(candidates(&conn, NOW, 1).unwrap().len(), 1);
    // A terminal row cannot be claimed either.
    assert!(!claim_for_retry(&conn, sent, NOW, 120).unwrap());
    assert!(!claim_for_retry(&conn, gone, NOW, 120).unwrap());
    assert!(!claim_for_retry(&conn, skipped, NOW, 120).unwrap());
    assert!(!claim_for_retry(&conn, legacy, NOW, 120).unwrap());
}

#[test]
fn abandon_expired_flips_only_old_unleased_rows() {
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh(&dir);
    let old = add(&conn, "pending", true, "t1");
    let young = add(&conn, "pending", true, "t2");
    let leased = add(&conn, "pending", true, "t3");
    settle(&conn, old, "failed", 1000);
    settle(&conn, young, "failed", 10);
    settle(&conn, leased, "failed", 1000);
    assert!(claim_for_retry(&conn, leased, NOW, 120).unwrap());

    assert_eq!(abandon_expired(&conn, NOW, 500).unwrap(), 1);
    let status = |id: i64| -> String {
        conn.query_row("SELECT forwarding_status FROM sms WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .unwrap()
    };
    assert_eq!(status(old), "abandoned");
    assert_eq!(status(young), "failed");
    assert_eq!(status(leased), "failed");
}

#[test]
fn mark_failed_keeps_the_category_and_mark_sent_clears_it() {
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh(&dir);
    let id = add(&conn, "pending", true, "t1");
    mark_failed(&conn, id, "http_500", Some(500)).unwrap();
    let (st, err, lease): (String, Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT forwarding_status, last_error, claimed_until FROM sms WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (st.as_str(), err.as_deref(), lease),
        ("failed", Some("http_500"), None)
    );

    mark_sent(&conn, id, "2026-01-01T00:00:00Z", 204).unwrap();
    // A late mark_failed must not resurrect a delivered text.
    mark_failed(&conn, id, "timeout", None).unwrap();
    let (st, err): (String, Option<String>) = conn
        .query_row(
            "SELECT forwarding_status, last_error FROM sms WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((st.as_str(), err), ("sent", None));
}

#[test]
fn summary_counts_states_and_reports_the_oldest_age_without_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let conn = fresh(&dir);
    let f = add(&conn, "pending", true, "t1");
    let s = add(&conn, "pending", true, "t2");
    let ab = add(&conn, "pending", true, "t3");
    add(&conn, "skipped", false, "t4");
    settle(&conn, f, "failed", 300);
    settle(&conn, s, "sent", 900);
    settle(&conn, ab, "abandoned", 5000);

    let sum = summary(&conn, NOW).unwrap();
    assert_eq!(sum.counts.failed, 1);
    assert_eq!(sum.counts.sent, 1);
    assert_eq!(sum.counts.abandoned, 1);
    assert_eq!(sum.counts.skipped, 1);
    assert_eq!(sum.oldest_undelivered_age_sec, 300);
    assert_eq!(sum.rows.len(), 2);
    assert!(!serde_json::to_string(&sum).unwrap().contains("hello"));
}
