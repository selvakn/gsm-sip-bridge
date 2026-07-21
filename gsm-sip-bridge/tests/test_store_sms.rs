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
fn test_insert_sms_pending() {
    let conn = mem_db();
    let record = SmsRecord {
        module_id: "ec20-A1B2C3".into(),
        sender: "+15551234567".into(),
        body: "Hello world".into(),
        received_at: "2026-05-04T20:00:00Z".into(),
        forwarding_status: "pending".into(),
        transport: Transport::Cs,
    };
    insert_sms(&conn, &record).unwrap();

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sms WHERE forwarding_status = 'pending'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn test_update_forwarding_to_sent() {
    let conn = mem_db();
    let record = SmsRecord {
        module_id: "ec20-A1B2C3".into(),
        sender: "+15551234567".into(),
        body: "Test SMS".into(),
        received_at: "2026-05-04T20:00:00Z".into(),
        forwarding_status: "pending".into(),
        transport: Transport::Cs,
    };
    insert_sms(&conn, &record).unwrap();

    let sms_id: i64 = conn
        .query_row(
            "SELECT id FROM sms WHERE sender = '+15551234567'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    let update = SmsForwardingUpdate {
        sms_id,
        forwarding_status: "sent".into(),
        forwarded_at: Some("2026-05-04T20:00:01Z".into()),
        discord_status_code: Some(200),
    };
    update_sms_forwarding(&conn, &update).unwrap();

    let status: String = conn
        .query_row(
            "SELECT forwarding_status FROM sms WHERE id = ?1",
            [sms_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "sent");
}

#[test]
fn test_update_forwarding_to_failed() {
    let conn = mem_db();
    let record = SmsRecord {
        module_id: "ec20-D4E5F6".into(),
        sender: "+15559876543".into(),
        body: "Another message".into(),
        received_at: "2026-05-04T20:10:00Z".into(),
        forwarding_status: "pending".into(),
        transport: Transport::Cs,
    };
    insert_sms(&conn, &record).unwrap();

    let sms_id: i64 = conn.last_insert_rowid();

    let update = SmsForwardingUpdate {
        sms_id,
        forwarding_status: "failed".into(),
        forwarded_at: None,
        discord_status_code: Some(503),
    };
    update_sms_forwarding(&conn, &update).unwrap();

    let (status, code): (String, Option<i32>) = conn
        .query_row(
            "SELECT forwarding_status, discord_status_code FROM sms WHERE id = ?1",
            [sms_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(code, Some(503));
}

#[test]
fn test_skipped_status() {
    let conn = mem_db();
    let record = SmsRecord {
        module_id: "ec20-A1B2C3".into(),
        sender: "+15550001111".into(),
        body: "Skipped message".into(),
        received_at: "2026-05-04T21:00:00Z".into(),
        forwarding_status: "skipped".into(),
        transport: Transport::Cs,
    };
    insert_sms(&conn, &record).unwrap();

    let status: String = conn
        .query_row(
            "SELECT forwarding_status FROM sms WHERE sender = '+15550001111'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "skipped");
}
