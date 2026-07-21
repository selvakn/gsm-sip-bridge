use crate::error::{BridgeError, BridgeResult};
use rusqlite::Connection;

const SCHEMA_VERSION: &str = "3";

const SCHEMA_SQL: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;
PRAGMA foreign_keys = OFF;

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', '1');

CREATE TABLE IF NOT EXISTS calls (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    module_id         TEXT    NOT NULL,
    caller_id         TEXT    NOT NULL DEFAULT '',
    started_at        TEXT    NOT NULL,
    duration_seconds  REAL    NOT NULL DEFAULT 0.0,
    status            TEXT    NOT NULL CHECK (status IN ('answered','missed','failed')),
    sip_destination   TEXT    NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_calls_started_at ON calls(started_at);
CREATE INDEX IF NOT EXISTS idx_calls_module     ON calls(module_id);
CREATE INDEX IF NOT EXISTS idx_calls_status     ON calls(status);

CREATE TABLE IF NOT EXISTS sms (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    module_id           TEXT    NOT NULL,
    sender              TEXT    NOT NULL,
    body                TEXT    NOT NULL,
    received_at         TEXT    NOT NULL,
    forwarding_status   TEXT    NOT NULL CHECK (forwarding_status IN ('pending','sent','failed','skipped')),
    forwarded_at        TEXT,
    discord_status_code INTEGER
);

CREATE INDEX IF NOT EXISTS idx_sms_received_at ON sms(received_at);
CREATE INDEX IF NOT EXISTS idx_sms_module      ON sms(module_id);
CREATE INDEX IF NOT EXISTS idx_sms_status      ON sms(forwarding_status);

CREATE VIEW IF NOT EXISTS recent_calls AS
    SELECT id, module_id, caller_id, started_at, duration_seconds, status, sip_destination
    FROM calls
    ORDER BY id DESC
    LIMIT 200;

CREATE VIEW IF NOT EXISTS recent_sms AS
    SELECT id, module_id, sender, body, received_at, forwarding_status, forwarded_at, discord_status_code
    FROM sms
    ORDER BY id DESC
    LIMIT 200;
"#;

const SCHEMA_V2_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS card_slots (
    slot          INTEGER PRIMARY KEY,
    imei          TEXT    NOT NULL UNIQUE,
    usb_serial    TEXT    NOT NULL DEFAULT '',
    registered_at TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS card_mode_prefs (
    slot  INTEGER PRIMARY KEY REFERENCES card_slots(slot),
    mode  TEXT    NOT NULL CHECK (mode IN ('2g','3g','4g','auto'))
);
"#;

/// Adds `transport` to `calls`/`sms` so VoWiFi call/SMS records (specs/014)
/// can share the same tables as circuit-switched ones. `DEFAULT 'cs'` is what
/// backfills every pre-existing row for free — accurate by construction,
/// since the VoWiFi path has never written a row before this migration
/// exists. Views are dropped and recreated rather than left alone because
/// `CREATE VIEW IF NOT EXISTS` would otherwise pin them to the pre-v3 column
/// list forever.
const SCHEMA_V3_SQL: &str = r#"
ALTER TABLE calls ADD COLUMN transport TEXT NOT NULL DEFAULT 'cs'
    CHECK (transport IN ('cs','vowifi'));
ALTER TABLE sms   ADD COLUMN transport TEXT NOT NULL DEFAULT 'cs'
    CHECK (transport IN ('cs','vowifi'));

CREATE INDEX IF NOT EXISTS idx_calls_transport ON calls(transport);
CREATE INDEX IF NOT EXISTS idx_sms_transport   ON sms(transport);

DROP VIEW IF EXISTS recent_calls;
CREATE VIEW recent_calls AS
    SELECT id, module_id, caller_id, started_at, duration_seconds, status, sip_destination, transport
    FROM calls
    ORDER BY id DESC
    LIMIT 200;

DROP VIEW IF EXISTS recent_sms;
CREATE VIEW recent_sms AS
    SELECT id, module_id, sender, body, received_at, forwarding_status, forwarded_at, discord_status_code, transport
    FROM sms
    ORDER BY id DESC
    LIMIT 200;
"#;

pub fn init_schema(conn: &Connection) -> BridgeResult<()> {
    conn.execute_batch(SCHEMA_SQL)
        .map_err(|e| BridgeError::Store(format!("failed to initialize schema: {e}")))?;

    let mut version: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| BridgeError::Store(format!("failed to read schema_version: {e}")))?;

    if version == "1" {
        conn.execute_batch(SCHEMA_V2_SQL)
            .map_err(|e| BridgeError::Store(format!("schema v1→v2 migration failed: {e}")))?;
        conn.execute(
            "UPDATE meta SET value = '2' WHERE key = 'schema_version'",
            [],
        )
        .map_err(|e| BridgeError::Store(format!("failed to update schema_version: {e}")))?;
        version = "2".to_string();
    }

    if version == "2" {
        conn.execute_batch(SCHEMA_V3_SQL)
            .map_err(|e| BridgeError::Store(format!("schema v2→v3 migration failed: {e}")))?;
        conn.execute(
            "UPDATE meta SET value = '3' WHERE key = 'schema_version'",
            [],
        )
        .map_err(|e| BridgeError::Store(format!("failed to update schema_version: {e}")))?;
        version = "3".to_string();
    }

    if version != "3" {
        return Err(BridgeError::Store(format!(
            "incompatible schema version: expected {SCHEMA_VERSION}, found {version}"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fresh_schema_is_v3() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let ver: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ver, "3");
        // Verify new tables exist
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM card_slots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_v1_to_v3_migration() {
        let conn = Connection::open_in_memory().unwrap();
        // Bootstrap a v1 schema manually
        conn.execute_batch(SCHEMA_SQL).unwrap();
        // SCHEMA_SQL inserts version '1' — already at v1
        init_schema(&conn).unwrap();
        let ver: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ver, "3");
        // Tables should exist after migration
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM card_mode_prefs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_v2_to_v3_backfills_existing_rows_as_cs() {
        let conn = Connection::open_in_memory().unwrap();
        // Bootstrap a v2 schema manually (v1 base + v2 migration), with rows
        // already present in calls/sms — the pre-existing-history scenario.
        conn.execute_batch(SCHEMA_SQL).unwrap();
        conn.execute_batch(SCHEMA_V2_SQL).unwrap();
        conn.execute(
            "UPDATE meta SET value = '2' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO calls (module_id, caller_id, started_at, duration_seconds, status, sip_destination)
             VALUES ('ec20-AAAAAA', '+15551234', '2026-01-01T00:00:00Z', 12.5, 'answered', '1001')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sms (module_id, sender, body, received_at, forwarding_status)
             VALUES ('ec20-AAAAAA', '+15551234', 'hi', '2026-01-01T00:00:00Z', 'sent')",
            [],
        )
        .unwrap();

        init_schema(&conn).unwrap();

        let ver: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ver, "3");

        let call_transport: String = conn
            .query_row("SELECT transport FROM calls LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(call_transport, "cs");

        let sms_transport: String = conn
            .query_row("SELECT transport FROM sms LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sms_transport, "cs");

        let null_calls: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM calls WHERE transport IS NULL OR transport = ''",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(null_calls, 0);

        // Views were recreated with the new column.
        let view_transport: String = conn
            .query_row("SELECT transport FROM recent_calls LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(view_transport, "cs");
    }

    #[test]
    fn test_vowifi_transport_accepted() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO calls (module_id, caller_id, started_at, duration_seconds, status, sip_destination, transport)
             VALUES ('ec20-AAAAAA', '+15551234', '2026-01-01T00:00:00Z', 5.0, 'answered', '1001', 'vowifi')",
            [],
        )
        .unwrap();
        let transport: String = conn
            .query_row("SELECT transport FROM calls LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(transport, "vowifi");
    }
}
