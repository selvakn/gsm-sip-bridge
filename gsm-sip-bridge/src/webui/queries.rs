//! Read-only queries behind the web UI's list pages (specs/122-management-
//! webui). The UI reads the message store and never the modem: `AT+CMGL`
//! marks messages read (research R10), so listing from the modem would
//! destroy the unread flag.
//!
//! The reader owns its own connection, opened `SQLITE_OPEN_READ_ONLY`, so
//! nothing here can write even by mistake, and a slow page cannot hold up the
//! store's writer thread: WAL lets the reader and the writer run together.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_PER_PAGE: u32 = 50;
const MAX_PER_PAGE: u32 = 200;

/// The store could not be read: missing, locked past the timeout, or a query
/// failed. The handlers answer `503 store_unavailable` and the rest of the UI
/// keeps working.
#[derive(Debug)]
pub struct StoreUnavailable;

/// Which page of a list, 1-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    pub page: u32,
    pub per_page: u32,
}

impl Page {
    /// Absent values take the defaults; a `per_page` above the maximum is
    /// clamped rather than refused, as the contract says.
    pub fn new(page: Option<u32>, per_page: Option<u32>) -> Self {
        Self {
            page: page.unwrap_or(1).max(1),
            per_page: per_page.unwrap_or(DEFAULT_PER_PAGE).clamp(1, MAX_PER_PAGE),
        }
    }

    fn offset(self) -> i64 {
        i64::from(self.page - 1) * i64::from(self.per_page)
    }
}

/// `{ "items": [...], "page": N, "per_page": M, "total": T }`.
#[derive(Debug, Serialize)]
pub struct Paged<T> {
    pub items: Vec<T>,
    pub page: u32,
    pub per_page: u32,
    pub total: u64,
}

#[derive(Debug, Default, Clone)]
pub struct SmsFilter {
    pub line: Option<String>,
    /// Substring of the sender or the body.
    pub q: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct SmsRow {
    pub id: i64,
    pub module_id: String,
    pub sender: String,
    pub body: String,
    pub received_at: String,
    pub forwarding_status: String,
    pub forwarded_at: Option<String>,
    pub transport: String,
}

#[derive(Debug, Default, Clone)]
pub struct CallFilter {
    pub line: Option<String>,
    pub direction: Option<String>,
    pub transport: Option<String>,
    pub status: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct CallRow {
    pub id: i64,
    pub module_id: String,
    pub direction: String,
    /// The caller for an inbound call, the dialled number for an outbound one.
    pub remote: String,
    pub started_at: String,
    pub duration_seconds: f64,
    pub status: String,
    pub transport: String,
    pub loss_percent: Option<f64>,
    pub jitter_ms: Option<f64>,
    pub rtt_ms: Option<f64>,
}

/// Counts for one window.
#[derive(Debug, Default, Serialize, Clone, PartialEq)]
pub struct CallCounts {
    pub answered: u64,
    pub missed: u64,
    pub failed: u64,
    /// `null` when nothing was answered in the window.
    pub avg_answered_seconds: Option<f64>,
    #[serde(skip)]
    answered_seconds: f64,
}

impl CallCounts {
    fn add(&mut self, status: &str, count: u64, answered_seconds: f64) {
        match status {
            "answered" => {
                self.answered += count;
                self.answered_seconds += answered_seconds;
            }
            "missed" => self.missed += count,
            "failed" => self.failed += count,
            _ => {}
        }
    }

    fn finish(mut self) -> Self {
        self.avg_answered_seconds =
            (self.answered > 0).then(|| self.answered_seconds / self.answered as f64);
        self
    }
}

#[derive(Debug, Default, Serialize, Clone, PartialEq)]
pub struct Windowed {
    pub today: CallCounts,
    pub last_7d: CallCounts,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct LineCallSummary {
    pub module_id: String,
    pub today: CallCounts,
    pub last_7d: CallCounts,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct DirectionSummary {
    pub inbound: Windowed,
    pub outbound: Windowed,
    /// Calls recorded before direction was (schema v6 backfill).
    pub unknown: Windowed,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct CallSummary {
    pub today: CallCounts,
    pub last_7d: CallCounts,
    pub by_line: Vec<LineCallSummary>,
    pub by_direction: DirectionSummary,
}

/// `started_at` is stored as RFC 3339 text in UTC, with `Z` or `+00:00` and
/// sometimes a fraction. Comparing against this fixed-width prefix is correct
/// for every one of those forms.
fn ts(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S").to_string()
}

#[derive(Debug, Default, Clone)]
pub struct EventFilter {
    pub line: Option<String>,
    /// An exact kind, a group (`alert_raised` matches every
    /// `alert_raised:<category>`), or a prefix ending in `:`.
    pub kind: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct EventRow {
    pub id: i64,
    pub at: String,
    pub module_id: Option<String>,
    pub kind: String,
    pub severity: String,
    pub detail: String,
    pub raise_id: Option<i64>,
    pub duration_seconds: Option<f64>,
}

pub struct StoreReader {
    path: PathBuf,
    conn: Mutex<Option<Connection>>,
}

impl StoreReader {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            conn: Mutex::new(None),
        }
    }

    fn open(&self) -> rusqlite::Result<Connection> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        Ok(conn)
    }

    /// Runs `f` on the connection, opening it on first use. A failure drops
    /// the connection so the next request starts clean, which is what makes a
    /// store that appears (or is restored) later start working again.
    fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> Result<T, StoreUnavailable> {
        let mut slot = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            match self.open() {
                Ok(conn) => *slot = Some(conn),
                Err(e) => {
                    tracing::debug!(error = %e, path = %self.path.display(), "web UI cannot open the store");
                    return Err(StoreUnavailable);
                }
            }
        }
        let result = f(slot.as_ref().expect("opened above"));
        match result {
            Ok(v) => Ok(v),
            Err(e) => {
                tracing::warn!(error = %e, "web UI store query failed");
                *slot = None;
                Err(StoreUnavailable)
            }
        }
    }

    pub fn list_sms(
        &self,
        filter: &SmsFilter,
        page: Page,
    ) -> Result<Paged<SmsRow>, StoreUnavailable> {
        let (where_sql, params) = sms_where(filter);
        self.with_conn(|conn| {
            let total: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM sms {where_sql}"),
                rusqlite::params_from_iter(params.iter()),
                |r| r.get(0),
            )?;
            let mut stmt = conn.prepare(&format!(
                "SELECT id, module_id, sender, body, received_at, forwarding_status, forwarded_at, transport \
                 FROM sms {where_sql} ORDER BY received_at DESC, id DESC LIMIT {} OFFSET {}",
                page.per_page,
                page.offset()
            ))?;
            let items = stmt
                .query_map(rusqlite::params_from_iter(params.iter()), |r| {
                    Ok(SmsRow {
                        id: r.get(0)?,
                        module_id: r.get(1)?,
                        sender: r.get(2)?,
                        body: r.get(3)?,
                        received_at: r.get(4)?,
                        forwarding_status: r.get(5)?,
                        forwarded_at: r.get(6)?,
                        transport: r.get(7)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Paged {
                items,
                page: page.page,
                per_page: page.per_page,
                total: u64::try_from(total).unwrap_or(0),
            })
        })
    }
}

impl StoreReader {
    /// Totals for "today" (from `today_start`) and the window from
    /// `week_start`, overall, per line and per direction.
    pub fn call_summary(
        &self,
        today_start: DateTime<Utc>,
        week_start: DateTime<Utc>,
    ) -> Result<CallSummary, StoreUnavailable> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT module_id, direction, status, started_at >= ?1 AS is_today, \
                        COUNT(*), COALESCE(SUM(duration_seconds), 0.0) \
                 FROM calls WHERE started_at >= ?2 \
                 GROUP BY module_id, direction, status, is_today",
            )?;
            let rows = stmt.query_map([ts(today_start), ts(week_start)], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, bool>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, f64>(5)?,
                ))
            })?;

            let mut total = Windowed::default();
            let mut by_line: BTreeMap<String, Windowed> = BTreeMap::new();
            let mut inbound = Windowed::default();
            let mut outbound = Windowed::default();
            let mut unknown = Windowed::default();
            for row in rows {
                let (module, direction, status, is_today, count, seconds) = row?;
                let count = u64::try_from(count).unwrap_or(0);
                let by_dir = match direction.as_str() {
                    "outbound" => &mut outbound,
                    "inbound" => &mut inbound,
                    _ => &mut unknown,
                };
                let line = by_line.entry(module).or_default();
                for target in [&mut total, line, by_dir] {
                    target.last_7d.add(&status, count, seconds);
                    if is_today {
                        target.today.add(&status, count, seconds);
                    }
                }
            }
            let done = |w: Windowed| Windowed {
                today: w.today.finish(),
                last_7d: w.last_7d.finish(),
            };
            let total = done(total);
            Ok(CallSummary {
                today: total.today,
                last_7d: total.last_7d,
                by_line: by_line
                    .into_iter()
                    .map(|(module_id, w)| {
                        let w = done(w);
                        LineCallSummary {
                            module_id,
                            today: w.today,
                            last_7d: w.last_7d,
                        }
                    })
                    .collect(),
                by_direction: DirectionSummary {
                    inbound: done(inbound),
                    outbound: done(outbound),
                    unknown: done(unknown),
                },
            })
        })
    }

    pub fn list_calls(
        &self,
        filter: &CallFilter,
        page: Page,
    ) -> Result<Paged<CallRow>, StoreUnavailable> {
        let (where_sql, params) = calls_where(filter);
        self.with_conn(|conn| {
            let total: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM calls {where_sql}"),
                rusqlite::params_from_iter(params.iter()),
                |r| r.get(0),
            )?;
            let mut stmt = conn.prepare(&format!(
                "SELECT id, module_id, direction, caller_id, started_at, duration_seconds, status, \
                        transport, loss_percent, jitter_ms, rtt_ms \
                 FROM calls {where_sql} ORDER BY started_at DESC, id DESC LIMIT {} OFFSET {}",
                page.per_page,
                page.offset()
            ))?;
            let items = stmt
                .query_map(rusqlite::params_from_iter(params.iter()), |r| {
                    Ok(CallRow {
                        id: r.get(0)?,
                        module_id: r.get(1)?,
                        direction: r.get(2)?,
                        remote: r.get(3)?,
                        started_at: r.get(4)?,
                        duration_seconds: r.get(5)?,
                        status: r.get(6)?,
                        transport: r.get(7)?,
                        loss_percent: r.get(8)?,
                        jitter_ms: r.get(9)?,
                        rtt_ms: r.get(10)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Paged {
                items,
                page: page.page,
                per_page: page.per_page,
                total: u64::try_from(total).unwrap_or(0),
            })
        })
    }
}

impl StoreReader {
    pub fn list_events(
        &self,
        filter: &EventFilter,
        page: Page,
    ) -> Result<Paged<EventRow>, StoreUnavailable> {
        let (where_sql, params) = events_where(filter);
        self.with_conn(|conn| {
            let total: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM events {where_sql}"),
                rusqlite::params_from_iter(params.iter()),
                |r| r.get(0),
            )?;
            let mut stmt = conn.prepare(&format!(
                "SELECT id, at, module_id, kind, severity, detail, raise_id, duration_seconds \
                 FROM events {where_sql} ORDER BY at DESC, id DESC LIMIT {} OFFSET {}",
                page.per_page,
                page.offset()
            ))?;
            let items = stmt
                .query_map(rusqlite::params_from_iter(params.iter()), |r| {
                    Ok(EventRow {
                        id: r.get(0)?,
                        at: r.get(1)?,
                        module_id: r.get(2)?,
                        kind: r.get(3)?,
                        severity: r.get(4)?,
                        detail: r.get(5)?,
                        raise_id: r.get(6)?,
                        duration_seconds: r.get(7)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Paged {
                items,
                page: page.page,
                per_page: page.per_page,
                total: u64::try_from(total).unwrap_or(0),
            })
        })
    }
}

fn events_where(filter: &EventFilter) -> (String, Vec<String>) {
    let mut clauses = Vec::new();
    let mut params = Vec::new();
    if let Some(line) = filter.line.as_deref().filter(|l| !l.is_empty()) {
        clauses.push("module_id = ?".to_string());
        params.push(line.to_string());
    }
    if let Some(kind) = filter.kind.as_deref().filter(|k| !k.is_empty()) {
        if kind.ends_with(':') {
            clauses.push("kind LIKE ? ESCAPE '\\'".to_string());
            params.push(like_prefix(kind));
        } else {
            clauses.push("(kind = ? OR kind LIKE ? ESCAPE '\\')".to_string());
            params.push(kind.to_string());
            params.push(like_prefix(&format!("{kind}:")));
        }
    }
    let sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    (sql, params)
}

/// `prefix%`, with the prefix matched literally.
fn like_prefix(prefix: &str) -> String {
    let mut pattern = like_pattern(prefix);
    pattern.pop(); // the leading-and-trailing form's closing %
    pattern.remove(0); // its opening %
    pattern.push('%');
    pattern
}

fn calls_where(filter: &CallFilter) -> (String, Vec<String>) {
    let mut clauses = Vec::new();
    let mut params = Vec::new();
    for (column, value) in [
        ("module_id", &filter.line),
        ("direction", &filter.direction),
        ("transport", &filter.transport),
        ("status", &filter.status),
    ] {
        if let Some(v) = value.as_deref().filter(|v| !v.is_empty()) {
            clauses.push(format!("{column} = ?"));
            params.push(v.to_string());
        }
    }
    if let Some(from) = filter.from {
        clauses.push("started_at >= ?".to_string());
        params.push(ts(from));
    }
    if let Some(to) = filter.to {
        // `started_at` carries a zone suffix and sometimes a fraction, so it
        // sorts *after* the bare prefix of its own second. Compare against the
        // start of the next second, exclusive, which keeps everything up to
        // and including `to`'s own second.
        clauses.push("started_at < ?".to_string());
        params.push(ts(to + chrono::Duration::seconds(1)));
    }
    let sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    (sql, params)
}

/// Escapes `%`, `_` and the escape character itself, so the search text is
/// matched literally.
fn like_pattern(q: &str) -> String {
    let mut out = String::with_capacity(q.len() + 2);
    out.push('%');
    for c in q.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// The `WHERE` clause and its bound parameters. Values are only ever bound,
/// never spliced into the SQL.
fn sms_where(filter: &SmsFilter) -> (String, Vec<String>) {
    let mut clauses = Vec::new();
    let mut params = Vec::new();
    if let Some(line) = filter.line.as_deref().filter(|l| !l.is_empty()) {
        clauses.push("module_id = ?".to_string());
        params.push(line.to_string());
    }
    if let Some(q) = filter.q.as_deref().filter(|q| !q.is_empty()) {
        clauses.push("(sender LIKE ? ESCAPE '\\' OR body LIKE ? ESCAPE '\\')".to_string());
        let pattern = like_pattern(q);
        params.push(pattern.clone());
        params.push(pattern);
    }
    let sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    (sql, params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paging_defaults_and_clamps() {
        assert_eq!(
            Page::new(None, None),
            Page {
                page: 1,
                per_page: 50
            }
        );
        assert_eq!(
            Page::new(Some(0), Some(0)),
            Page {
                page: 1,
                per_page: 1
            }
        );
        assert_eq!(Page::new(Some(3), Some(9999)).per_page, 200);
        assert_eq!(Page::new(Some(3), Some(10)).offset(), 20);
    }

    #[test]
    fn like_wildcards_in_the_search_text_are_literal() {
        assert_eq!(like_pattern("50%_off"), "%50\\%\\_off%");
        assert_eq!(like_pattern("a\\b"), "%a\\\\b%");
    }

    #[test]
    fn a_kind_filter_is_exact_or_a_group_or_a_literal_prefix() {
        let (sql, params) = events_where(&EventFilter {
            line: None,
            kind: Some("alert_raised:".into()),
        });
        assert!(sql.contains("kind LIKE"), "{sql}");
        assert_eq!(params, vec!["alert\\_raised:%".to_string()]);

        let (_, params) = events_where(&EventFilter {
            line: None,
            kind: Some("tunnel_recovered".into()),
        });
        assert_eq!(
            params,
            vec![
                "tunnel_recovered".to_string(),
                "tunnel\\_recovered:%".to_string()
            ]
        );
    }

    #[test]
    fn filter_values_are_bound_not_spliced() {
        let (sql, params) = sms_where(&SmsFilter {
            line: Some("x'; DROP TABLE sms; --".into()),
            q: Some("hi".into()),
        });
        assert!(!sql.contains("DROP"), "{sql}");
        assert_eq!(params.len(), 3);
    }

    #[test]
    fn a_missing_store_is_unavailable_not_a_panic() {
        let reader = StoreReader::new("/nonexistent/dir/store.db");
        assert!(reader
            .list_sms(&SmsFilter::default(), Page::new(None, None))
            .is_err());
    }
}
