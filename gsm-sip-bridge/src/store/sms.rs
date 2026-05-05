use crate::error::BridgeResult;
use rusqlite::Connection;

#[derive(Debug, Clone)]
pub struct SmsRecord {
    pub module_id: String,
    pub sender: String,
    pub body: String,
    pub received_at: String,
    pub forwarding_status: String,
}

#[derive(Debug, Clone)]
pub struct SmsForwardingUpdate {
    pub sms_id: i64,
    pub forwarding_status: String,
    pub forwarded_at: Option<String>,
    pub discord_status_code: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct SmsForwardingByTimeUpdate {
    pub module_id: String,
    pub received_at: String,
    pub forwarding_status: String,
    pub forwarded_at: Option<String>,
    pub discord_status_code: Option<i32>,
}

pub fn insert_sms(conn: &Connection, record: &SmsRecord) -> BridgeResult<()> {
    conn.execute(
        "INSERT INTO sms (module_id, sender, body, received_at, forwarding_status) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            record.module_id,
            record.sender,
            record.body,
            record.received_at,
            record.forwarding_status,
        ],
    )?;
    Ok(())
}

pub fn update_sms_forwarding(conn: &Connection, update: &SmsForwardingUpdate) -> BridgeResult<()> {
    conn.execute(
        "UPDATE sms SET forwarding_status = ?1, forwarded_at = ?2, discord_status_code = ?3 WHERE id = ?4",
        rusqlite::params![
            update.forwarding_status,
            update.forwarded_at,
            update.discord_status_code,
            update.sms_id,
        ],
    )?;
    Ok(())
}

pub fn update_sms_forwarding_by_time(
    conn: &Connection,
    update: &SmsForwardingByTimeUpdate,
) -> BridgeResult<()> {
    conn.execute(
        "UPDATE sms SET forwarding_status = ?1, forwarded_at = ?2, discord_status_code = ?3 WHERE module_id = ?4 AND received_at = ?5",
        rusqlite::params![
            update.forwarding_status,
            update.forwarded_at,
            update.discord_status_code,
            update.module_id,
            update.received_at,
        ],
    )?;
    Ok(())
}
