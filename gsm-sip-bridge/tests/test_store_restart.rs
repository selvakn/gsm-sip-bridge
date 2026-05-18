mod common;

use gsm_sip_bridge::store::calls::{insert_call, CallRecord};
use gsm_sip_bridge::store::schema::init_schema;
use rusqlite::Connection;
use tempfile::NamedTempFile;

#[test]
fn test_store_survives_restart() {
    let db_file = NamedTempFile::new().unwrap();
    let db_path = db_file.path().to_str().unwrap();

    {
        let conn = Connection::open(db_path).unwrap();
        init_schema(&conn).unwrap();

        let record = CallRecord {
            module_id: "ec20-RESTART".into(),
            caller_id: "+15559999999".into(),
            started_at: "2026-05-04T22:00:00Z".into(),
            duration_seconds: 120.0,
            status: "answered".into(),
            sip_destination: "sip:200@pbx:5060".into(),
        };
        insert_call(&conn, &record).unwrap();
    }

    {
        let conn = Connection::open(db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let schema_version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(schema_version, "2");
    }
}
