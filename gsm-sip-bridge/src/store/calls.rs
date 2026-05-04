use crate::error::BridgeResult;
use rusqlite::Connection;

#[derive(Debug, Clone)]
pub struct CallRecord {
    pub module_id: String,
    pub caller_id: String,
    pub started_at: String,
    pub duration_seconds: f64,
    pub status: String,
    pub sip_destination: String,
}

pub fn insert_call(conn: &Connection, record: &CallRecord) -> BridgeResult<()> {
    conn.execute(
        "INSERT INTO calls (module_id, caller_id, started_at, duration_seconds, status, sip_destination) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            record.module_id,
            record.caller_id,
            record.started_at,
            record.duration_seconds,
            record.status,
            record.sip_destination,
        ],
    )?;
    Ok(())
}
