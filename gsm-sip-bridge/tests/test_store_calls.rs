mod common;

use gsm_sip_bridge::store::calls::{insert_call, CallRecord};
use gsm_sip_bridge::store::schema::init_schema;
use gsm_sip_bridge::store::Transport;
use rusqlite::Connection;

fn mem_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    conn
}

#[test]
fn test_insert_and_query_answered_call() {
    let conn = mem_db();
    let record = CallRecord {
        module_id: "ec20-A1B2C3".into(),
        caller_id: "+15551234567".into(),
        started_at: "2026-05-04T20:00:00Z".into(),
        duration_seconds: 42.5,
        status: "answered".into(),
        sip_destination: "sip:100@pbx:5060".into(),
        transport: Transport::Cs,
    };
    insert_call(&conn, &record).unwrap();

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM calls WHERE status = 'answered'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn test_insert_missed_call() {
    let conn = mem_db();
    let record = CallRecord {
        module_id: "ec20-D4E5F6".into(),
        caller_id: "+15559876543".into(),
        started_at: "2026-05-04T20:01:00Z".into(),
        duration_seconds: 0.0,
        status: "missed".into(),
        sip_destination: "".into(),
        transport: Transport::Cs,
    };
    insert_call(&conn, &record).unwrap();

    let status: String = conn
        .query_row(
            "SELECT status FROM calls WHERE module_id = 'ec20-D4E5F6'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "missed");
}

#[test]
fn test_insert_failed_call() {
    let conn = mem_db();
    let record = CallRecord {
        module_id: "ec20-A1B2C3".into(),
        caller_id: "+15551111111".into(),
        started_at: "2026-05-04T20:02:00Z".into(),
        duration_seconds: 0.0,
        status: "failed".into(),
        sip_destination: "sip:unreachable@pbx:5060".into(),
        transport: Transport::Cs,
    };
    insert_call(&conn, &record).unwrap();

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM calls WHERE status = 'failed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn test_recent_calls_view_newest_first() {
    let conn = mem_db();
    for i in 0..5 {
        let record = CallRecord {
            module_id: "ec20-A1B2C3".into(),
            caller_id: format!("+1555000000{i}"),
            started_at: format!("2026-05-04T20:0{i}:00Z"),
            duration_seconds: 10.0,
            status: "answered".into(),
            sip_destination: "sip:100@pbx:5060".into(),
            transport: Transport::Cs,
        };
        insert_call(&conn, &record).unwrap();
    }

    let first_caller: String = conn
        .query_row("SELECT caller_id FROM recent_calls LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(first_caller, "+15550000004");
}
