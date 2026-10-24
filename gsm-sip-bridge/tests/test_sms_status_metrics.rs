//! Operator visibility into SMS delivery state (specs/121-sms-alert-retry):
//! the `sms-status` subcommand and the `/metrics` gauges, both fed from the
//! same seeded SQLite store.

use gsm_sip_bridge::metrics::server;
use gsm_sip_bridge::store::schema::init_schema;
use gsm_sip_bridge::store::sms::{insert_sms, SmsRecord};
use gsm_sip_bridge::store::Transport;
use rusqlite::Connection;
use std::process::Command;

const SECRET_BODY: &str = "one-time-code-482913";

fn seed(path: &std::path::Path) {
    let conn = Connection::open(path).unwrap();
    init_schema(&conn).unwrap();
    let now = chrono::Utc::now().timestamp();
    for (i, (status, age)) in [
        ("sent", 900),
        ("sent", 800),
        ("failed", 3_600),
        ("pending", 120),
        ("abandoned", 700_000),
    ]
    .iter()
    .enumerate()
    {
        insert_sms(
            &conn,
            &SmsRecord {
                module_id: "line-1".into(),
                sender: "+919000000001".into(),
                body: SECRET_BODY.into(),
                received_at: format!("2026-03-0{}T00:00:00Z", i + 1),
                forwarding_status: "pending".into(),
                transport: Transport::Vowifi,
                retry_eligible: true,
            },
        )
        .unwrap();
        conn.execute(
            "UPDATE sms SET forwarding_status=?1, first_seen_at=?2, claimed_until=NULL, \
             last_error=CASE WHEN ?1 IN ('failed','abandoned') THEN 'unreachable' END WHERE id=?3",
            rusqlite::params![status, now - age, conn.last_insert_rowid()],
        )
        .unwrap();
    }
}

fn config_for(dir: &std::path::Path, db: &std::path::Path) -> std::path::PathBuf {
    std::env::set_var("SMS_STATUS_TEST_PW", "pw");
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        format!(
            "[sip]\nserver = \"127.0.0.1\"\nusername = \"u\"\npassword = \"env:SMS_STATUS_TEST_PW\"\n\
             [sms]\ndb_path = \"{}\"\n",
            db.display()
        ),
    )
    .unwrap();
    path
}

fn run_cli(cfg: &std::path::Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gsm-sip-bridge"))
        .env("SMS_STATUS_TEST_PW", "pw")
        .arg("--config")
        .arg(cfg)
        .arg("sms-status")
        .args(extra)
        .output()
        .unwrap()
}

#[test]
fn sms_status_reports_counts_and_never_prints_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("store.db");
    seed(&db);
    let cfg = config_for(dir.path(), &db);

    let out = run_cli(&cfg, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("delivered: 2"), "{text}");
    assert!(text.contains("pending: 1"), "{text}");
    assert!(text.contains("failed: 1"), "{text}");
    assert!(text.contains("abandoned: 1"), "{text}");
    assert!(text.contains("oldest undelivered: 1h 0m"), "{text}");
    assert!(text.contains("unreachable"), "{text}");
    assert!(!text.contains(SECRET_BODY));

    let out = run_cli(&cfg, &["--json"]);
    assert!(out.status.success());
    let raw = String::from_utf8_lossy(&out.stdout);
    assert!(!raw.contains(SECRET_BODY));
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["counts"]["sent"], 2);
    assert_eq!(v["counts"]["failed"], 1);
    assert_eq!(v["rows"].as_array().unwrap().len(), 3);
    let age = v["oldest_undelivered_age_sec"].as_i64().unwrap();
    assert!((3_600..3_700).contains(&age), "{age}");
}

#[test]
fn sms_status_fails_only_when_the_store_cannot_be_opened() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_for(dir.path(), &dir.path().join("missing.db"));
    let out = run_cli(&cfg, &[]);
    assert!(!out.status.success());
}

#[tokio::test]
async fn metrics_expose_the_same_state() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("store.db");
    seed(&db);

    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let task = tokio::spawn(server::serve(
        port,
        10,
        Some(db.to_string_lossy().into_owned()),
    ));
    let url = format!("http://127.0.0.1:{port}/metrics");
    let mut body = String::new();
    for _ in 0..50 {
        if let Ok(r) = reqwest::get(&url).await {
            body = r.text().await.unwrap();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    task.abort();

    assert!(
        body.contains("gsm_sip_bridge_sms_undelivered{state=\"failed\"} 1"),
        "{body}"
    );
    assert!(body.contains("gsm_sip_bridge_sms_undelivered{state=\"pending\"} 1"));
    assert!(body.contains("gsm_sip_bridge_sms_abandoned 1"));
    let age_line = body
        .lines()
        .find(|l| l.starts_with("gsm_sip_bridge_sms_oldest_undelivered_age_seconds "))
        .expect("age gauge");
    let age: f64 = age_line.rsplit(' ').next().unwrap().parse().unwrap();
    assert!((3_600.0..3_700.0).contains(&age), "{age}");
    assert!(!body.contains(SECRET_BODY));
}
