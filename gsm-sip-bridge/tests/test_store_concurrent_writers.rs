mod common;

use gsm_sip_bridge::store::calls::{insert_call, CallRecord};
use gsm_sip_bridge::store::schema::init_schema;
use gsm_sip_bridge::store::sms::{insert_sms, SmsRecord};
use gsm_sip_bridge::store::Transport;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

#[test]
fn test_concurrent_writes_via_single_connection() {
    let conn = Arc::new(Mutex::new({
        let c = Connection::open_in_memory().unwrap();
        init_schema(&c).unwrap();
        c
    }));

    let total_writes = 1000;
    let mut handles = Vec::new();

    for i in 0..total_writes {
        let conn = conn.clone();
        let handle = std::thread::spawn(move || {
            let guard = conn.lock().unwrap();
            if i % 2 == 0 {
                let record = CallRecord {
                    module_id: format!("ec20-{:06X}", i),
                    caller_id: format!("+1555{:07}", i),
                    started_at: format!("2026-05-04T{:02}:{:02}:00Z", i / 60 % 24, i % 60),
                    duration_seconds: (i as f64) * 0.5,
                    status: "answered".into(),
                    sip_destination: "sip:100@pbx:5060".into(),
                    transport: Transport::Cs,
                };
                insert_call(&guard, &record).unwrap();
            } else {
                let record = SmsRecord {
                    module_id: format!("ec20-{:06X}", i),
                    sender: format!("+1555{:07}", i),
                    body: format!("Message #{i}"),
                    received_at: format!("2026-05-04T{:02}:{:02}:00Z", i / 60 % 24, i % 60),
                    forwarding_status: "sent".into(),
                    transport: Transport::Cs,
                };
                insert_sms(&guard, &record).unwrap();
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let guard = conn.lock().unwrap();
    let call_count: i64 = guard
        .query_row("SELECT COUNT(*) FROM calls", [], |r| r.get(0))
        .unwrap();
    let sms_count: i64 = guard
        .query_row("SELECT COUNT(*) FROM sms", [], |r| r.get(0))
        .unwrap();
    assert_eq!(call_count + sms_count, total_writes);
}
