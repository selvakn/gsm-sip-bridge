use crate::error::BridgeResult;
use crate::store::Transport;
use rusqlite::Connection;

/// Which way a call was set up (specs/122-management-webui). Persisted as the
/// `direction` column added by the schema v5 migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallDirection {
    Inbound,
    Outbound,
}

impl CallDirection {
    pub fn as_str(&self) -> &'static str {
        match self {
            CallDirection::Inbound => "inbound",
            CallDirection::Outbound => "outbound",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CallRecord {
    pub module_id: String,
    pub caller_id: String,
    pub started_at: String,
    pub duration_seconds: f64,
    pub status: String,
    pub sip_destination: String,
    pub transport: Transport,
    pub direction: CallDirection,
    /// Local receive loss at teardown; `None` when unknown (always for CS).
    pub loss_percent: Option<f64>,
    pub jitter_ms: Option<f64>,
    pub rtt_ms: Option<f64>,
}

impl CallRecord {
    /// A call this bridge placed (specs/122-management-webui): `destination`
    /// is the number dialled and `module_id` the line that carried it, or `""`
    /// when it was refused before a line was chosen. Quality is unknown.
    pub fn outbound(
        module_id: &str,
        destination: &str,
        started_at: chrono::DateTime<chrono::Utc>,
        duration_seconds: f64,
        status: &str,
        transport: Transport,
    ) -> Self {
        Self {
            module_id: module_id.to_string(),
            caller_id: destination.to_string(),
            started_at: started_at.to_rfc3339(),
            duration_seconds,
            status: status.to_string(),
            sip_destination: String::new(),
            transport,
            direction: CallDirection::Outbound,
            loss_percent: None,
            jitter_ms: None,
            rtt_ms: None,
        }
    }
}

pub fn insert_call(conn: &Connection, record: &CallRecord) -> BridgeResult<()> {
    conn.execute(
        "INSERT INTO calls (module_id, caller_id, started_at, duration_seconds, status, sip_destination, transport, direction, loss_percent, jitter_ms, rtt_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            record.module_id,
            record.caller_id,
            record.started_at,
            record.duration_seconds,
            record.status,
            record.sip_destination,
            record.transport.as_str(),
            record.direction.as_str(),
            record.loss_percent,
            record.jitter_ms,
            record.rtt_ms,
        ],
    )?;
    Ok(())
}
