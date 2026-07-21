mod common;

use gsm_sip_bridge::store::schema::init_schema;
use gsm_sip_bridge::store::sms::{
    insert_sms, update_sms_forwarding, SmsForwardingUpdate, SmsRecord,
};
use gsm_sip_bridge::store::Transport;
use rusqlite::Connection;

fn mem_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    conn
}

#[test]
fn test_sms_full_path_persist_then_forward() {
    let conn = mem_db();

    let record = SmsRecord {
        module_id: "ec20-A1B2C3".into(),
        sender: "+15551234567".into(),
        body: "Test message".into(),
        received_at: "2026-05-04T20:00:00Z".into(),
        forwarding_status: "pending".into(),
        transport: Transport::Cs,
    };
    insert_sms(&conn, &record).unwrap();

    let (id, status): (i64, String) = conn
        .query_row(
            "SELECT id, forwarding_status FROM sms WHERE sender = '+15551234567'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "pending");

    let update = SmsForwardingUpdate {
        sms_id: id,
        forwarding_status: "sent".into(),
        forwarded_at: Some("2026-05-04T20:00:02Z".into()),
        discord_status_code: Some(200),
    };
    update_sms_forwarding(&conn, &update).unwrap();

    let final_status: String = conn
        .query_row(
            "SELECT forwarding_status FROM sms WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(final_status, "sent");
}

#[test]
fn test_sms_discord_unreachable_marks_failed() {
    let conn = mem_db();

    let record = SmsRecord {
        module_id: "ec20-D4E5F6".into(),
        sender: "+15559876543".into(),
        body: "Another message".into(),
        received_at: "2026-05-04T21:00:00Z".into(),
        forwarding_status: "pending".into(),
        transport: Transport::Cs,
    };
    insert_sms(&conn, &record).unwrap();
    let id: i64 = conn.last_insert_rowid();

    let update = SmsForwardingUpdate {
        sms_id: id,
        forwarding_status: "failed".into(),
        forwarded_at: None,
        discord_status_code: Some(503),
    };
    update_sms_forwarding(&conn, &update).unwrap();

    let (status, code): (String, Option<i32>) = conn
        .query_row(
            "SELECT forwarding_status, discord_status_code FROM sms WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(code, Some(503));
}

#[test]
fn test_sms_disabled_skips_forwarding() {
    let conn = mem_db();

    let record = SmsRecord {
        module_id: "ec20-A1B2C3".into(),
        sender: "+15550000000".into(),
        body: "Skipped".into(),
        received_at: "2026-05-04T22:00:00Z".into(),
        forwarding_status: "skipped".into(),
        transport: Transport::Cs,
    };
    insert_sms(&conn, &record).unwrap();

    let status: String = conn
        .query_row(
            "SELECT forwarding_status FROM sms WHERE sender = '+15550000000'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "skipped");
}
