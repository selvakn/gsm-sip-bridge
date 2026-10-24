//! `sms-status`: read-only report of SMS Discord delivery state
//! (specs/121-sms-alert-retry, contracts/cli.md).

use crate::cli::Cli;
use crate::config::load_config;
use crate::store::sms::{summary, SmsDeliverySummary};
use rusqlite::{Connection, OpenFlags};
use std::process::ExitCode;

pub fn run(cli: &Cli, json: bool) -> ExitCode {
    let Some(path) = cli.config.as_deref() else {
        eprintln!("sms-status: --config is required");
        return ExitCode::FAILURE;
    };
    let config = match load_config(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sms-status: {e}");
            return ExitCode::FAILURE;
        }
    };
    let now = chrono::Utc::now().timestamp();
    let sum = match read(&config.sms.db_path, now) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sms-status: {e}");
            return ExitCode::FAILURE;
        }
    };
    if json {
        match serde_json::to_string_pretty(&sum) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("sms-status: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        print!("{}", render(&sum, now));
    }
    ExitCode::SUCCESS
}

pub fn read(db_path: &str, now: i64) -> Result<SmsDeliverySummary, String> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("cannot open store {db_path}: {e}"))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    summary(&conn, now).map_err(|e| e.to_string())
}

fn duration(secs: i64) -> String {
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    match (secs, h) {
        (0, _) => "none".into(),
        (_, 0) => format!("{m}m"),
        _ => format!("{h}h {m}m"),
    }
}

fn at(ts: Option<i64>) -> String {
    ts.and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|d| d.format("%Y-%m-%dT%H:%MZ").to_string())
        .unwrap_or_else(|| "-".into())
}

pub fn render(s: &SmsDeliverySummary, _now: i64) -> String {
    let c = &s.counts;
    let mut out = format!(
        "SMS Discord delivery\n  delivered: {}   pending: {}   failed: {}   abandoned: {}   skipped: {}\n",
        c.sent, c.pending, c.failed, c.abandoned, c.skipped
    );
    if c.legacy_failed > 0 {
        out.push_str(&format!(
            "  failed before retry existed (never retried): {}\n",
            c.legacy_failed
        ));
    }
    out.push_str(&format!(
        "  oldest undelivered: {}\n",
        duration(s.oldest_undelivered_age_sec)
    ));
    if !s.rows.is_empty() {
        out.push_str(&format!(
            "\nUndelivered / abandoned (newest first, max {}):\n",
            crate::store::sms::SUMMARY_ROW_LIMIT
        ));
        out.push_str("  id   module   transport  received_at           status     attempts  last_attempt      last_error\n");
        for r in &s.rows {
            out.push_str(&format!(
                "  {}  {}  {}  {}  {}  {}  {}  {}\n",
                r.id,
                r.module_id,
                r.transport,
                r.received_at,
                r.status,
                r.attempt_count,
                at(r.last_attempt_at),
                r.last_error.as_deref().unwrap_or("-")
            ));
        }
    }
    out
}
