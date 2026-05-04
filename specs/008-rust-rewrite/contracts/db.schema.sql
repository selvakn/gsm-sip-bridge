-- Contract — Persisted Store Schema (gsm-sip-bridge v5.0.0)
--
-- Engine:    SQLite 3 (bundled via `rusqlite` `bundled` feature)
-- Mode:      WAL, synchronous=NORMAL
-- File:      [sms].db_path config key (default /var/lib/gsm-sip-bridge/store.db)
-- Migrations: Forward-only; managed in `gsm-sip-bridge/src/store/schema.rs`.
-- Source of truth for shape: this file.
-- v4.1.x → v5.0.0 migration SQL: docs/migrating-from-v4.1.x.md (R-15)
--
-- Schema version 1 (initial v5.0.0 release).

PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;
PRAGMA foreign_keys = OFF;   -- module_id is a logical reference, not a SQL FK

-- -----------------------------------------------------------------------------
-- meta : single-row table tracking schema version
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- The bridge inserts ('schema_version', '1') on first init and refuses to
-- start if it finds a value it does not recognise.
INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', '1');

-- -----------------------------------------------------------------------------
-- calls : append-only record of every incoming GSM call
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS calls (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    module_id         TEXT    NOT NULL,
    caller_id         TEXT    NOT NULL DEFAULT '',
    started_at        TEXT    NOT NULL,        -- ISO 8601 UTC, e.g. 2026-05-05T10:00:00.000Z
    duration_seconds  REAL    NOT NULL DEFAULT 0.0,
    status            TEXT    NOT NULL CHECK (status IN ('answered','missed','failed')),
    sip_destination   TEXT    NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_calls_started_at ON calls(started_at);
CREATE INDEX IF NOT EXISTS idx_calls_module     ON calls(module_id);
CREATE INDEX IF NOT EXISTS idx_calls_status     ON calls(status);

-- -----------------------------------------------------------------------------
-- sms : append-only record of every incoming SMS
-- -----------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS sms (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    module_id           TEXT    NOT NULL,
    sender              TEXT    NOT NULL,
    body                TEXT    NOT NULL,
    received_at         TEXT    NOT NULL,
    forwarding_status   TEXT    NOT NULL CHECK (forwarding_status IN ('pending','sent','failed','skipped')),
    forwarded_at        TEXT,                            -- nullable; set when status leaves 'pending'
    discord_status_code INTEGER                          -- nullable; HTTP status of Discord POST when applicable
);

CREATE INDEX IF NOT EXISTS idx_sms_received_at ON sms(received_at);
CREATE INDEX IF NOT EXISTS idx_sms_module      ON sms(module_id);
CREATE INDEX IF NOT EXISTS idx_sms_status      ON sms(forwarding_status);

-- -----------------------------------------------------------------------------
-- Operator-facing convenience views (read-only queries from sqlite-web etc.)
-- -----------------------------------------------------------------------------

-- Recent activity, newest first
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

-- -----------------------------------------------------------------------------
-- Manual prune procedure (FR-042) — operators copy these from
-- docs/migrating-from-v4.1.x.md or docs/operations.md, NOT executed automatically.
-- -----------------------------------------------------------------------------
-- DELETE FROM calls WHERE started_at  < datetime('now', '-365 days');
-- DELETE FROM sms   WHERE received_at < datetime('now', '-365 days');
-- VACUUM;
