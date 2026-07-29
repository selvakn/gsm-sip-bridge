use rusqlite::Connection;
use tempfile::NamedTempFile;

fn create_legacy_sms_db() -> NamedTempFile {
    let f = NamedTempFile::new().unwrap();
    let conn = Connection::open(f.path()).unwrap();
    conn.execute_batch(
        "CREATE TABLE sms (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            module_id TEXT,
            sender TEXT NOT NULL,
            body TEXT NOT NULL,
            received_at TEXT NOT NULL,
            forwarded_at TEXT,
            discord_status_code INTEGER
        );
        INSERT INTO sms (module_id, sender, body, received_at, forwarded_at, discord_status_code)
        VALUES ('ec20-A1B2C3', '+15551234567', 'Hello', '2026-01-01T12:00:00Z', '2026-01-01T12:00:01Z', 200);
        INSERT INTO sms (module_id, sender, body, received_at, forwarded_at, discord_status_code)
        VALUES (NULL, '+15559876543', 'Failed msg', '2026-01-02T12:00:00Z', NULL, 503);
        INSERT INTO sms (module_id, sender, body, received_at, forwarded_at, discord_status_code)
        VALUES ('ec20-D4E5F6', '+15550001111', 'No discord', '2026-01-03T12:00:00Z', NULL, NULL);",
    )
    .unwrap();
    drop(conn);
    f
}

fn run_migration(legacy_path: &str, new_path: &str) {
    let conn = Connection::open(new_path).unwrap();
    conn.execute_batch(&format!(
        "ATTACH DATABASE '{legacy_path}' AS old;

        CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', '1');

        CREATE TABLE IF NOT EXISTS calls (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            module_id TEXT NOT NULL,
            caller_id TEXT NOT NULL DEFAULT '',
            started_at TEXT NOT NULL,
            duration_seconds REAL NOT NULL DEFAULT 0.0,
            status TEXT NOT NULL CHECK (status IN ('answered','missed','failed')),
            sip_destination TEXT NOT NULL DEFAULT ''
        );

        CREATE TABLE IF NOT EXISTS sms (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            module_id TEXT NOT NULL,
            sender TEXT NOT NULL,
            body TEXT NOT NULL,
            received_at TEXT NOT NULL,
            forwarding_status TEXT NOT NULL CHECK (forwarding_status IN ('pending','sent','failed','skipped')),
            forwarded_at TEXT,
            discord_status_code INTEGER
        );

        INSERT INTO sms (module_id, sender, body, received_at, forwarding_status, forwarded_at, discord_status_code)
        SELECT
            COALESCE(module_id, 'ec20-LEGACY'),
            sender,
            body,
            received_at,
            CASE
                WHEN discord_status_code BETWEEN 200 AND 299 THEN 'sent'
                WHEN discord_status_code IS NOT NULL THEN 'failed'
                ELSE 'skipped'
            END,
            forwarded_at,
            discord_status_code
        FROM old.sms;

        DETACH DATABASE old;
        VACUUM;"
    ))
    .unwrap();
}

#[test]
fn test_migration_preserves_all_rows() {
    let legacy = create_legacy_sms_db();
    let new_db = NamedTempFile::new().unwrap();

    run_migration(
        legacy.path().to_str().unwrap(),
        new_db.path().to_str().unwrap(),
    );

    let conn = Connection::open(new_db.path()).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM sms", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 3);

    let schema_version: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(schema_version, "1");
}

#[test]
fn test_migration_status_mapping() {
    let legacy = create_legacy_sms_db();
    let new_db = NamedTempFile::new().unwrap();

    run_migration(
        legacy.path().to_str().unwrap(),
        new_db.path().to_str().unwrap(),
    );

    let conn = Connection::open(new_db.path()).unwrap();

    let sent: String = conn
        .query_row(
            "SELECT forwarding_status FROM sms WHERE sender = '+15551234567'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sent, "sent");

    let failed: String = conn
        .query_row(
            "SELECT forwarding_status FROM sms WHERE sender = '+15559876543'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(failed, "failed");

    let skipped: String = conn
        .query_row(
            "SELECT forwarding_status FROM sms WHERE sender = '+15550001111'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(skipped, "skipped");
}

#[test]
fn test_migration_null_module_id_becomes_legacy() {
    let legacy = create_legacy_sms_db();
    let new_db = NamedTempFile::new().unwrap();

    run_migration(
        legacy.path().to_str().unwrap(),
        new_db.path().to_str().unwrap(),
    );

    let conn = Connection::open(new_db.path()).unwrap();
    let module: String = conn
        .query_row(
            "SELECT module_id FROM sms WHERE sender = '+15559876543'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(module, "ec20-LEGACY");
}

#[test]
fn test_migration_idempotent() {
    let legacy = create_legacy_sms_db();
    let new_db = NamedTempFile::new().unwrap();

    run_migration(
        legacy.path().to_str().unwrap(),
        new_db.path().to_str().unwrap(),
    );

    // Second run should not duplicate
    let conn = Connection::open(new_db.path()).unwrap();
    conn.execute("DELETE FROM sms", []).unwrap();
    drop(conn);

    run_migration(
        legacy.path().to_str().unwrap(),
        new_db.path().to_str().unwrap(),
    );

    let conn = Connection::open(new_db.path()).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM sms", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 3);
}

#[test]
fn test_migration_nondestructive_to_source() {
    let legacy = create_legacy_sms_db();
    let new_db = NamedTempFile::new().unwrap();

    let original_content = std::fs::read(legacy.path()).unwrap();

    run_migration(
        legacy.path().to_str().unwrap(),
        new_db.path().to_str().unwrap(),
    );

    let after_content = std::fs::read(legacy.path()).unwrap();
    assert_eq!(
        original_content, after_content,
        "source DB was modified during migration"
    );
}
