use crate::error::{BridgeError, BridgeResult};
use rusqlite::Connection;

const SCHEMA_VERSION: &str = "4";

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

/// Adds `'volte'` to the `transport` CHECK on `calls` and `sms`. The v3
/// constraint listed only `('cs','vowifi')`, so the host-side LTE path
/// (specs/017) could not record a single call or text — every insert failed
/// the check silently, surfaced only once a live text was actually delivered
/// over the modem route.
///
/// SQLite cannot alter a column CHECK in place, so each table is rebuilt: a new
/// table with the widened constraint, the rows copied across, the old one
/// dropped, the new one renamed. Foreign keys are already `OFF` (set in
/// `SCHEMA_SQL`) and nothing references these two tables, so the rebuild is
/// safe. Indexes and views are recreated to match.
const SCHEMA_V4_SQL: &str = r#"
-- Drop the dependent views first: SQLite validates a view against its base
-- table during the table rebuild below, and would fail on the dropped `calls`.
DROP VIEW IF EXISTS recent_calls;
DROP VIEW IF EXISTS recent_sms;

CREATE TABLE calls_v4 (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    module_id         TEXT    NOT NULL,
    caller_id         TEXT    NOT NULL DEFAULT '',
    started_at        TEXT    NOT NULL,
    duration_seconds  REAL    NOT NULL DEFAULT 0.0,
    status            TEXT    NOT NULL CHECK (status IN ('answered','missed','failed')),
    sip_destination   TEXT    NOT NULL DEFAULT '',
    transport         TEXT    NOT NULL DEFAULT 'cs' CHECK (transport IN ('cs','vowifi','volte'))
);
INSERT INTO calls_v4 (id, module_id, caller_id, started_at, duration_seconds, status, sip_destination, transport)
    SELECT id, module_id, caller_id, started_at, duration_seconds, status, sip_destination, transport FROM calls;
DROP TABLE calls;
ALTER TABLE calls_v4 RENAME TO calls;

CREATE INDEX IF NOT EXISTS idx_calls_started_at ON calls(started_at);
CREATE INDEX IF NOT EXISTS idx_calls_module     ON calls(module_id);
CREATE INDEX IF NOT EXISTS idx_calls_status     ON calls(status);
CREATE INDEX IF NOT EXISTS idx_calls_transport  ON calls(transport);

CREATE TABLE sms_v4 (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    module_id           TEXT    NOT NULL,
    sender              TEXT    NOT NULL,
    body                TEXT    NOT NULL,
    received_at         TEXT    NOT NULL,
    forwarding_status   TEXT    NOT NULL CHECK (forwarding_status IN ('pending','sent','failed','skipped')),
    forwarded_at        TEXT,
    discord_status_code INTEGER,
    transport           TEXT    NOT NULL DEFAULT 'cs' CHECK (transport IN ('cs','vowifi','volte'))
);
INSERT INTO sms_v4 (id, module_id, sender, body, received_at, forwarding_status, forwarded_at, discord_status_code, transport)
    SELECT id, module_id, sender, body, received_at, forwarding_status, forwarded_at, discord_status_code, transport FROM sms;
DROP TABLE sms;
ALTER TABLE sms_v4 RENAME TO sms;

CREATE INDEX IF NOT EXISTS idx_sms_received_at ON sms(received_at);
CREATE INDEX IF NOT EXISTS idx_sms_module      ON sms(module_id);
CREATE INDEX IF NOT EXISTS idx_sms_status      ON sms(forwarding_status);
CREATE INDEX IF NOT EXISTS idx_sms_transport   ON sms(transport);

CREATE VIEW recent_calls AS
    SELECT id, module_id, caller_id, started_at, duration_seconds, status, sip_destination, transport
    FROM calls
    ORDER BY id DESC
    LIMIT 200;

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

    if version == "3" {
        conn.execute_batch(SCHEMA_V4_SQL)
            .map_err(|e| BridgeError::Store(format!("schema v3→v4 migration failed: {e}")))?;
        conn.execute(
            "UPDATE meta SET value = '4' WHERE key = 'schema_version'",
            [],
        )
        .map_err(|e| BridgeError::Store(format!("failed to update schema_version: {e}")))?;
        version = "4".to_string();
    }

    if version != "4" {
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
    fn test_fresh_schema_is_v4() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let ver: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ver, "4");
        // Verify new tables exist
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM card_slots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_v1_to_v4_migration() {
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
        assert_eq!(ver, "4");
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
        assert_eq!(ver, "4");

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

    #[test]
    fn test_volte_transport_accepted_for_calls_and_sms() {
        // The v3 constraint listed only ('cs','vowifi'), so a VoLTE call or
        // text failed its insert silently. This is the exact insert that was
        // failing live — it must now succeed on both tables.
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO calls (module_id, caller_id, started_at, duration_seconds, status, sip_destination, transport)
             VALUES ('volte', '+919789063708', '2026-07-23T00:00:00Z', 5.0, 'answered', '1001', 'volte')",
            [],
        )
        .expect("a volte call must be recordable");
        conn.execute(
            "INSERT INTO sms (module_id, sender, body, received_at, forwarding_status, transport)
             VALUES ('volte', '+919789063708', 'Hello', '2026-07-23T00:00:00Z', 'sent', 'volte')",
            [],
        )
        .expect("a volte text must be recordable");
    }

    #[test]
    fn test_a_v3_store_that_rejected_volte_accepts_it_after_upgrade() {
        // Reproduces the deployed database: a v3 schema whose transport CHECK
        // excludes 'volte'. Before the upgrade the insert is refused; after
        // `init_schema` migrates it to v4 the same insert succeeds.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        conn.execute_batch(SCHEMA_V2_SQL).unwrap();
        conn.execute_batch(SCHEMA_V3_SQL).unwrap();
        conn.execute(
            "UPDATE meta SET value = '3' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();

        let refused = conn.execute(
            "INSERT INTO sms (module_id, sender, body, received_at, forwarding_status, transport)
             VALUES ('volte', '+919789063708', 'Hello', '2026-07-23T00:00:00Z', 'sent', 'volte')",
            [],
        );
        assert!(refused.is_err(), "the v3 constraint must reject 'volte'");

        init_schema(&conn).unwrap();

        conn.execute(
            "INSERT INTO sms (module_id, sender, body, received_at, forwarding_status, transport)
             VALUES ('volte', '+919789063708', 'Hello', '2026-07-23T00:00:00Z', 'sent', 'volte')",
            [],
        )
        .expect("after the v4 upgrade 'volte' must be accepted");
    }
}
