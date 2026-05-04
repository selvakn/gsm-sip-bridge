use crate::error::{BridgeError, BridgeResult};
use rusqlite::Connection;

const SCHEMA_VERSION: &str = "1";

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

pub fn init_schema(conn: &Connection) -> BridgeResult<()> {
    conn.execute_batch(SCHEMA_SQL)
        .map_err(|e| BridgeError::Store(format!("failed to initialize schema: {e}")))?;

    let version: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| BridgeError::Store(format!("failed to read schema_version: {e}")))?;

    if version != SCHEMA_VERSION {
        return Err(BridgeError::Store(format!(
            "incompatible schema version: expected {SCHEMA_VERSION}, found {version}"
        )));
    }

    Ok(())
}
