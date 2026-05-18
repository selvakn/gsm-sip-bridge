use crate::error::{BridgeError, BridgeResult};
use crate::modules::at_commander::NetworkMode;
use rusqlite::Connection;

const MAX_SLOTS: u32 = 7;

pub fn lookup_slot(conn: &Connection, imei: &str) -> BridgeResult<Option<u32>> {
    let mut stmt = conn
        .prepare("SELECT slot FROM card_slots WHERE imei = ?1")
        .map_err(|e| BridgeError::Store(format!("prepare lookup_slot: {e}")))?;
    let mut rows = stmt
        .query([imei])
        .map_err(|e| BridgeError::Store(format!("query lookup_slot: {e}")))?;
    if let Some(row) = rows
        .next()
        .map_err(|e| BridgeError::Store(format!("next lookup_slot: {e}")))?
    {
        let slot: u32 = row
            .get(0)
            .map_err(|e| BridgeError::Store(format!("get slot: {e}")))?;
        Ok(Some(slot))
    } else {
        Ok(None)
    }
}

pub fn assign_slot(conn: &Connection, imei: &str, usb_serial: &str) -> BridgeResult<u32> {
    // Re-check in case concurrent assign happened
    if let Some(existing) = lookup_slot(conn, imei)? {
        return Ok(existing);
    }

    let next_slot: u32 = conn
        .query_row(
            "SELECT COALESCE(MAX(slot) + 1, 0) FROM card_slots",
            [],
            |r| r.get(0),
        )
        .map_err(|e| BridgeError::Store(format!("compute next slot: {e}")))?;

    if next_slot > MAX_SLOTS {
        return Err(BridgeError::Store(format!(
            "slot limit reached ({MAX_SLOTS}); cannot assign slot for IMEI {imei}"
        )));
    }

    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO card_slots(slot, imei, usb_serial, registered_at) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![next_slot, imei, usb_serial, now],
    )
    .map_err(|e| BridgeError::Store(format!("insert card_slot: {e}")))?;

    Ok(next_slot)
}

pub fn get_mode_pref(conn: &Connection, slot: u32) -> BridgeResult<Option<NetworkMode>> {
    let mut stmt = conn
        .prepare("SELECT mode FROM card_mode_prefs WHERE slot = ?1")
        .map_err(|e| BridgeError::Store(format!("prepare get_mode_pref: {e}")))?;
    let mut rows = stmt
        .query([slot])
        .map_err(|e| BridgeError::Store(format!("query get_mode_pref: {e}")))?;
    if let Some(row) = rows
        .next()
        .map_err(|e| BridgeError::Store(format!("next get_mode_pref: {e}")))?
    {
        let mode_str: String = row
            .get(0)
            .map_err(|e| BridgeError::Store(format!("get mode: {e}")))?;
        let mode = mode_str
            .parse::<NetworkMode>()
            .map_err(|e| BridgeError::Store(format!("parse NetworkMode '{mode_str}': {e}")))?;
        Ok(Some(mode))
    } else {
        Ok(None)
    }
}

pub fn set_mode_pref(conn: &Connection, slot: u32, mode: NetworkMode) -> BridgeResult<()> {
    conn.execute(
        "INSERT OR REPLACE INTO card_mode_prefs(slot, mode) VALUES (?1, ?2)",
        rusqlite::params![slot, mode.to_string()],
    )
    .map_err(|e| BridgeError::Store(format!("set_mode_pref: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::schema::init_schema;
    use rusqlite::Connection;

    fn open_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn test_assign_and_lookup_slot() {
        let conn = open_db();
        let slot = assign_slot(&conn, "123456789012345", "1-1.2").unwrap();
        assert_eq!(slot, 0);
        let found = lookup_slot(&conn, "123456789012345").unwrap();
        assert_eq!(found, Some(0));
    }

    #[test]
    fn test_assign_idempotent() {
        let conn = open_db();
        let s1 = assign_slot(&conn, "111111111111111", "usb0").unwrap();
        let s2 = assign_slot(&conn, "111111111111111", "usb0").unwrap();
        assert_eq!(s1, s2);
    }

    #[test]
    fn test_multiple_slots_sequential() {
        let conn = open_db();
        let s0 = assign_slot(&conn, "000000000000000", "usb0").unwrap();
        let s1 = assign_slot(&conn, "111111111111111", "usb1").unwrap();
        let s2 = assign_slot(&conn, "222222222222222", "usb2").unwrap();
        assert_eq!(s0, 0);
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
    }

    #[test]
    fn test_mode_pref_crud() {
        let conn = open_db();
        assign_slot(&conn, "123456789012345", "usb0").unwrap();
        assert_eq!(get_mode_pref(&conn, 0).unwrap(), None);

        set_mode_pref(&conn, 0, NetworkMode::Lte).unwrap();
        assert_eq!(get_mode_pref(&conn, 0).unwrap(), Some(NetworkMode::Lte));

        set_mode_pref(&conn, 0, NetworkMode::Gsm).unwrap();
        assert_eq!(get_mode_pref(&conn, 0).unwrap(), Some(NetworkMode::Gsm));
    }

    #[test]
    fn test_lookup_unknown_imei() {
        let conn = open_db();
        assert_eq!(lookup_slot(&conn, "999999999999999").unwrap(), None);
    }
}
