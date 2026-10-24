//! The persisted event timeline behind the web UI's Events page
//! (specs/122-management-webui, data-model.md `events`).
//!
//! Events originate in several processes -- the daemon, both VoWiFi/VoLTE
//! agents, and the supervisor -- and deep inside code (`alerts::dispatch`)
//! that has no store handle. So each process installs one sink at startup
//! ([`install_sink`]) and call sites just call [`record_alert`] or
//! [`record`]; every write goes through the store's ordered writer thread.
//!
//! Alert raise/resolve pairs are deduplicated here, in-process: a `Failure`
//! while one is open (a retried delivery) and a `Recovered` with nothing open
//! add no row, so the timeline reads once per outage, not once per attempt.

use crate::error::BridgeResult;
use crate::store::StoreCommand;
use chrono::{DateTime, SecondsFormat, Utc};
use crossbeam_channel::Sender;
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Longest `detail` stored, in characters.
pub const MAX_DETAIL_CHARS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

/// The closed set of things the timeline records. `as_str` is the stored
/// text; alert kinds carry their category after a `:`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    AlertRaised(String),
    AlertResolved(String),
    TunnelRecovered,
    DeadPeerLoss,
    UplinkHoldStarted,
    UplinkHoldEnded,
    ModuleInitialized,
    ModuleInitFailed,
    ScheduledRestart,
}

impl EventKind {
    pub fn as_str(&self) -> String {
        match self {
            EventKind::AlertRaised(c) => format!("alert_raised:{c}"),
            EventKind::AlertResolved(c) => format!("alert_resolved:{c}"),
            EventKind::TunnelRecovered => "tunnel_recovered".into(),
            EventKind::DeadPeerLoss => "dead_peer_loss".into(),
            EventKind::UplinkHoldStarted => "uplink_hold_started".into(),
            EventKind::UplinkHoldEnded => "uplink_hold_ended".into(),
            EventKind::ModuleInitialized => "module_initialized".into(),
            EventKind::ModuleInitFailed => "module_init_failed".into(),
            EventKind::ScheduledRestart => "scheduled_restart".into(),
        }
    }

    pub fn severity(&self) -> Severity {
        match self {
            EventKind::AlertRaised(_) => Severity::Critical,
            EventKind::AlertResolved(_)
            | EventKind::UplinkHoldEnded
            | EventKind::ModuleInitialized
            | EventKind::ScheduledRestart => Severity::Info,
            EventKind::TunnelRecovered
            | EventKind::DeadPeerLoss
            | EventKind::UplinkHoldStarted
            | EventKind::ModuleInitFailed => Severity::Warning,
        }
    }

    /// The part before the `:`, which keeps the metric label closed.
    pub fn group(&self) -> &'static str {
        match self {
            EventKind::AlertRaised(_) => "alert_raised",
            EventKind::AlertResolved(_) => "alert_resolved",
            EventKind::TunnelRecovered => "tunnel_recovered",
            EventKind::DeadPeerLoss => "dead_peer_loss",
            EventKind::UplinkHoldStarted => "uplink_hold_started",
            EventKind::UplinkHoldEnded => "uplink_hold_ended",
            EventKind::ModuleInitialized => "module_initialized",
            EventKind::ModuleInitFailed => "module_init_failed",
            EventKind::ScheduledRestart => "scheduled_restart",
        }
    }
}

/// One row to write.
#[derive(Debug, Clone)]
pub struct EventRecord {
    pub at: DateTime<Utc>,
    pub module_id: Option<String>,
    pub kind: EventKind,
    pub detail: String,
    pub duration_seconds: Option<f64>,
    /// For a resolve: the `at` of the raise it closes. The writer looks the
    /// raise's id up when it inserts, which is safe because the raise went
    /// down the same ordered channel to the same thread earlier.
    pub resolves_raise_at: Option<DateTime<Utc>>,
}

fn stamp(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn truncate(detail: &str) -> String {
    detail.chars().take(MAX_DETAIL_CHARS).collect()
}

pub fn insert_event(conn: &Connection, record: &EventRecord) -> BridgeResult<()> {
    let kind = record.kind.as_str();
    let raise_id: Option<i64> = match (&record.kind, record.resolves_raise_at) {
        (EventKind::AlertResolved(category), Some(raise_at)) => conn
            .query_row(
                "SELECT id FROM events WHERE kind = ?1 AND module_id IS ?2 AND at = ?3 \
                 ORDER BY id DESC LIMIT 1",
                rusqlite::params![
                    EventKind::AlertRaised(category.clone()).as_str(),
                    record.module_id,
                    stamp(raise_at)
                ],
                |r| r.get(0),
            )
            .ok(),
        _ => None,
    };
    conn.execute(
        "INSERT INTO events (at, module_id, kind, severity, detail, raise_id, duration_seconds) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            stamp(record.at),
            record.module_id,
            kind,
            record.kind.severity().as_str(),
            truncate(&record.detail),
            raise_id,
            record.duration_seconds,
        ],
    )?;
    Ok(())
}

/// Deletes rows older than `older_than`, returning how many went.
pub fn prune_events(conn: &Connection, older_than: DateTime<Utc>) -> BridgeResult<usize> {
    Ok(conn.execute("DELETE FROM events WHERE at < ?1", [stamp(older_than)])?)
}

// -------------------------------------------------------------- sink ------

fn sink() -> &'static Mutex<Option<Sender<StoreCommand>>> {
    static SINK: OnceLock<Mutex<Option<Sender<StoreCommand>>>> = OnceLock::new();
    SINK.get_or_init(|| Mutex::new(None))
}

/// `(category, module_id)` of an alert raised and not yet resolved, with when.
type OpenKey = (String, Option<String>);

fn open() -> &'static Mutex<HashMap<OpenKey, DateTime<Utc>>> {
    static OPEN: OnceLock<Mutex<HashMap<OpenKey, DateTime<Utc>>>> = OnceLock::new();
    OPEN.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Points this process's events at its store's writer thread. Installing a new
/// sink also forgets every open raise: they described the old store.
pub fn install_sink(tx: Sender<StoreCommand>) {
    *sink().lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    open().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

fn send(record: EventRecord) {
    let group = record.kind.group();
    let tx = sink().lock().unwrap_or_else(|e| e.into_inner()).clone();
    match tx {
        Some(tx) => {
            if tx.send(StoreCommand::InsertEvent(record)).is_ok() {
                crate::metrics::EVENTS_RECORDED_TOTAL
                    .with_label_values(&[group])
                    .inc();
            }
        }
        None => tracing::debug!(kind = group, "no event sink installed; event not recorded"),
    }
}

/// Records a non-alert event.
pub fn record(
    kind: EventKind,
    module_id: Option<&str>,
    detail: &str,
    duration_seconds: Option<f64>,
) {
    send(EventRecord {
        at: Utc::now(),
        module_id: module_id.map(str::to_string),
        kind,
        detail: detail.to_string(),
        duration_seconds,
        resolves_raise_at: None,
    });
}

/// Records an alert transition. `raised` is `true` for a `Failure`, `false` for
/// `Recovered`. A raise while one is open, and a resolve with nothing open,
/// are dropped (see the module docs).
pub fn record_alert(
    category: &str,
    module_id: Option<&str>,
    raised: bool,
    detail: &str,
    at: DateTime<Utc>,
) {
    let key = (category.to_string(), module_id.map(str::to_string));
    let mut open = open().lock().unwrap_or_else(|e| e.into_inner());
    if raised {
        if open.contains_key(&key) {
            return;
        }
        open.insert(key, at);
        drop(open);
        send(EventRecord {
            at,
            module_id: module_id.map(str::to_string),
            kind: EventKind::AlertRaised(category.to_string()),
            detail: detail.to_string(),
            duration_seconds: None,
            resolves_raise_at: None,
        });
    } else {
        let Some(raised_at) = open.remove(&key) else {
            return;
        };
        drop(open);
        let seconds = (at - raised_at).num_milliseconds() as f64 / 1000.0;
        send(EventRecord {
            at,
            module_id: module_id.map(str::to_string),
            kind: EventKind::AlertResolved(category.to_string()),
            detail: detail.to_string(),
            duration_seconds: Some(seconds.max(0.0)),
            resolves_raise_at: Some(raised_at),
        });
    }
}

/// An alert currently open in this process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRaise {
    pub category: String,
    pub module_id: Option<String>,
    pub since: DateTime<Utc>,
}

/// Alerts raised and not yet resolved in this process, oldest first.
pub fn open_raises() -> Vec<OpenRaise> {
    let mut raises: Vec<OpenRaise> = open()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|((category, module_id), since)| OpenRaise {
            category: category.clone(),
            module_id: module_id.clone(),
            since: *since,
        })
        .collect();
    raises.sort_by_key(|r| r.since);
    raises
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::schema::init_schema;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn rec(kind: EventKind, module: Option<&str>, at: DateTime<Utc>) -> EventRecord {
        EventRecord {
            at,
            module_id: module.map(str::to_string),
            kind,
            detail: "d".into(),
            duration_seconds: None,
            resolves_raise_at: None,
        }
    }

    #[test]
    fn kinds_have_stable_text_severity_and_group() {
        let raised = EventKind::AlertRaised("registration_loss".into());
        assert_eq!(raised.as_str(), "alert_raised:registration_loss");
        assert_eq!(raised.group(), "alert_raised");
        assert_eq!(raised.severity(), Severity::Critical);
        assert_eq!(
            EventKind::AlertResolved("x".into()).severity(),
            Severity::Info
        );
        assert_eq!(EventKind::DeadPeerLoss.severity(), Severity::Warning);
        assert_eq!(EventKind::ScheduledRestart.as_str(), "scheduled_restart");
    }

    #[test]
    fn detail_is_truncated_to_500_characters() {
        let c = conn();
        let mut r = rec(EventKind::ModuleInitFailed, Some("m"), Utc::now());
        r.detail = "é".repeat(900);
        insert_event(&c, &r).unwrap();
        let stored: String = c
            .query_row("SELECT detail FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored.chars().count(), MAX_DETAIL_CHARS);
    }

    #[test]
    fn a_resolve_finds_its_raise_by_kind_module_and_time() {
        let c = conn();
        let t0 = Utc::now();
        insert_event(
            &c,
            &rec(
                EventKind::AlertRaised("tunnel_failure".into()),
                Some("m1"),
                t0,
            ),
        )
        .unwrap();
        // A different module's raise at the same instant must not be matched.
        insert_event(
            &c,
            &rec(
                EventKind::AlertRaised("tunnel_failure".into()),
                Some("m2"),
                t0,
            ),
        )
        .unwrap();
        let mut resolve = rec(
            EventKind::AlertResolved("tunnel_failure".into()),
            Some("m1"),
            t0 + chrono::Duration::seconds(90),
        );
        resolve.resolves_raise_at = Some(t0);
        resolve.duration_seconds = Some(90.0);
        insert_event(&c, &resolve).unwrap();

        let (raise_id, m1_raise): (Option<i64>, i64) = c
            .query_row(
                "SELECT raise_id, (SELECT id FROM events WHERE kind = 'alert_raised:tunnel_failure' AND module_id = 'm1') \
                 FROM events WHERE kind = 'alert_resolved:tunnel_failure'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(raise_id, Some(m1_raise));
    }

    #[test]
    fn prune_deletes_only_older_rows() {
        let c = conn();
        let now = Utc::now();
        for days in [40, 31, 10, 0] {
            insert_event(
                &c,
                &rec(
                    EventKind::ScheduledRestart,
                    None,
                    now - chrono::Duration::days(days),
                ),
            )
            .unwrap();
        }
        let gone = prune_events(&c, now - chrono::Duration::days(30)).unwrap();
        assert_eq!(gone, 2);
        let left: i64 = c
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 2);
    }
}
