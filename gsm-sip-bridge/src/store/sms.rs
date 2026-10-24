use crate::error::BridgeResult;
use crate::store::Transport;
use rusqlite::Connection;

#[derive(Debug, Clone)]
pub struct SmsRecord {
    pub module_id: String,
    pub sender: String,
    pub body: String,
    pub received_at: String,
    pub forwarding_status: String,
    pub transport: Transport,
    /// `true` for a text whose Discord notification is about to be attempted
    /// (a webhook is configured and retry is on). The row is then inserted
    /// already leased for [`FIRST_ATTEMPT_LEASE_SECS`] with its first attempt
    /// counted, so a concurrent sweep never double-posts a text whose first
    /// attempt is still in flight (specs/121-sms-alert-retry, R2).
    pub retry_eligible: bool,
}

/// How long a freshly inserted row stays leased for its own first attempt.
/// Longer than the Discord client's whole post (30 s budget, then at most one
/// 10 s request; a longer `Retry-After` returns instead of sleeping), so a live
/// sender's lease cannot expire under it.
pub const FIRST_ATTEMPT_LEASE_SECS: i64 = 120;

/// Lease taken by the retry sweep for each row it re-posts.
pub const RETRY_LEASE_SECS: i64 = 120;

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
    /// Stable failure category of this attempt (`None` on success); never a
    /// raw error string, which may contain the webhook URL.
    pub last_error: Option<String>,
}

pub fn insert_sms(conn: &Connection, record: &SmsRecord) -> BridgeResult<()> {
    let now = chrono::Utc::now().timestamp();
    // `attempt_count > 0` is what marks a row as delivery-tracked (so
    // `sms-status` and the gauges count it even with retry off); rows that
    // predate v5 keep 0 and are history. Only retry-eligible rows take a lease.
    let attempted = record.forwarding_status == "pending";
    let eligible = i64::from(record.retry_eligible);
    let attempts = i64::from(attempted);
    let last_attempt = attempted.then_some(now);
    let lease = record
        .retry_eligible
        .then_some(now + FIRST_ATTEMPT_LEASE_SECS);
    conn.execute(
        "INSERT INTO sms (module_id, sender, body, received_at, forwarding_status, transport, \
                          attempt_count, last_attempt_at, first_seen_at, claimed_until, retry_eligible) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            record.module_id,
            record.sender,
            record.body,
            record.received_at,
            record.forwarding_status,
            record.transport.as_str(),
            attempts,
            last_attempt,
            now,
            lease,
            eligible,
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

/// Records the outcome of a text's first attempt. Matches only the still
/// `pending` row so a duplicate `(module_id, received_at)` can never overwrite
/// a row that is already `sent`; also releases the first-attempt lease.
pub fn update_sms_forwarding_by_time(
    conn: &Connection,
    update: &SmsForwardingByTimeUpdate,
) -> BridgeResult<()> {
    conn.execute(
        "UPDATE sms SET forwarding_status = ?1, forwarded_at = ?2, discord_status_code = ?3, \
                        last_error = ?4, claimed_until = NULL \
         WHERE module_id = ?5 AND received_at = ?6 AND forwarding_status = 'pending'",
        rusqlite::params![
            update.forwarding_status,
            update.forwarded_at,
            update.discord_status_code,
            update.last_error,
            update.module_id,
            update.received_at,
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------- retry ----
//
// Each function below is one atomic statement (or one read), so the daemon and
// both VoWiFi agents — separate processes sharing this file — can call them
// concurrently: SQLite serialises the writers and the conditional `WHERE`
// decides a single winner (specs/121-sms-alert-retry, R2).

/// A text awaiting (re)delivery, as read by the sweep.
#[derive(Debug, Clone)]
pub struct RetryCandidate {
    pub id: i64,
    pub module_id: String,
    pub sender: String,
    pub body: String,
    pub received_at: String,
    pub transport: String,
}

/// Oldest-first candidate list. Only a *list*: the sweep must still win
/// [`claim_for_retry`] for a row before posting it.
pub fn candidates(conn: &Connection, now: i64, limit: usize) -> BridgeResult<Vec<RetryCandidate>> {
    let mut stmt = conn.prepare(
        "SELECT id, module_id, sender, body, received_at, transport FROM sms \
         WHERE retry_eligible = 1 AND forwarding_status IN ('pending','failed') \
           AND (claimed_until IS NULL OR claimed_until <= ?1) \
         ORDER BY id ASC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![now, limit as i64], |r| {
            Ok(RetryCandidate {
                id: r.get(0)?,
                module_id: r.get(1)?,
                sender: r.get(2)?,
                body: r.get(3)?,
                received_at: r.get(4)?,
                transport: r.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Takes the lease on a row and counts the attempt. Returns `true` only for the
/// single caller whose conditional `UPDATE` changed the row.
pub fn claim_for_retry(
    conn: &Connection,
    id: i64,
    now: i64,
    lease_secs: i64,
) -> BridgeResult<bool> {
    let changed = conn.execute(
        "UPDATE sms SET claimed_until = ?2, attempt_count = attempt_count + 1, last_attempt_at = ?1 \
         WHERE id = ?3 AND retry_eligible = 1 AND forwarding_status IN ('pending','failed') \
           AND (claimed_until IS NULL OR claimed_until <= ?1)",
        rusqlite::params![now, now + lease_secs, id],
    )?;
    Ok(changed == 1)
}

/// Terminal success. A no-op on a row that is already `sent`.
pub fn mark_sent(conn: &Connection, id: i64, forwarded_at: &str, code: i32) -> BridgeResult<()> {
    conn.execute(
        "UPDATE sms SET forwarding_status = 'sent', forwarded_at = ?1, discord_status_code = ?2, \
                        last_error = NULL, claimed_until = NULL \
         WHERE id = ?3 AND forwarding_status != 'sent'",
        rusqlite::params![forwarded_at, code, id],
    )?;
    Ok(())
}

/// Records a failed attempt and releases the lease.
pub fn mark_failed(conn: &Connection, id: i64, error: &str, code: Option<i32>) -> BridgeResult<()> {
    conn.execute(
        "UPDATE sms SET forwarding_status = 'failed', last_error = ?1, \
                        discord_status_code = ?2, claimed_until = NULL \
         WHERE id = ?3 AND forwarding_status IN ('pending','failed')",
        rusqlite::params![error, code, id],
    )?;
    Ok(())
}

/// Releases a lease without recording an outcome (rate-limit stop, shutdown).
pub fn release_claim(conn: &Connection, id: i64) -> BridgeResult<()> {
    conn.execute("UPDATE sms SET claimed_until = NULL WHERE id = ?1", [id])?;
    Ok(())
}

/// Gives up on undelivered, un-leased rows older than `max_age_secs`.
pub fn abandon_expired(conn: &Connection, now: i64, max_age_secs: i64) -> BridgeResult<usize> {
    Ok(conn.execute(
        "UPDATE sms SET forwarding_status = 'abandoned', claimed_until = NULL \
         WHERE retry_eligible = 1 AND forwarding_status IN ('pending','failed') \
           AND first_seen_at < ?1 AND (claimed_until IS NULL OR claimed_until <= ?2)",
        rusqlite::params![now - max_age_secs, now],
    )?)
}

/// One undelivered/abandoned row for the status report. No body: it is PII.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UndeliveredRow {
    pub id: i64,
    pub module_id: String,
    pub transport: String,
    pub sender: String,
    pub received_at: String,
    pub status: String,
    pub attempt_count: i64,
    pub last_attempt_at: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SmsDeliveryCounts {
    pub sent: i64,
    pub pending: i64,
    pub failed: i64,
    pub abandoned: i64,
    pub skipped: i64,
    /// `failed` rows from before retry existed: reported, never retried.
    pub legacy_failed: i64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SmsDeliverySummary {
    pub counts: SmsDeliveryCounts,
    /// Age of the oldest `pending`/`failed` text; 0 when none.
    pub oldest_undelivered_age_sec: i64,
    pub rows: Vec<UndeliveredRow>,
}

pub const SUMMARY_ROW_LIMIT: usize = 20;

pub fn summary(conn: &Connection, now: i64) -> BridgeResult<SmsDeliverySummary> {
    let mut out = SmsDeliverySummary::default();

    let mut stmt = conn
        .prepare("SELECT forwarding_status, attempt_count > 0, COUNT(*) FROM sms GROUP BY 1, 2")?;
    let groups = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (status, eligible, n) in groups {
        let c = &mut out.counts;
        match (status.as_str(), eligible) {
            ("skipped", _) => c.skipped += n,
            ("failed", 0) => c.legacy_failed += n,
            ("sent", 1) => c.sent += n,
            ("pending", 1) => c.pending += n,
            ("failed", _) => c.failed += n,
            ("abandoned", 1) => c.abandoned += n,
            // Pre-v5 sent/pending rows are history, not delivery tracking.
            _ => {}
        }
    }

    let oldest: Option<i64> = conn.query_row(
        "SELECT MIN(first_seen_at) FROM sms \
         WHERE attempt_count > 0 AND forwarding_status IN ('pending','failed')",
        [],
        |r| r.get(0),
    )?;
    out.oldest_undelivered_age_sec = oldest.map(|t| (now - t).max(0)).unwrap_or(0);

    let mut stmt = conn.prepare(
        "SELECT id, module_id, transport, sender, received_at, forwarding_status, \
                attempt_count, last_attempt_at, last_error FROM sms \
         WHERE attempt_count > 0 AND forwarding_status IN ('pending','failed','abandoned') \
         ORDER BY id DESC LIMIT ?1",
    )?;
    out.rows = stmt
        .query_map([SUMMARY_ROW_LIMIT as i64], |r| {
            Ok(UndeliveredRow {
                id: r.get(0)?,
                module_id: r.get(1)?,
                transport: r.get(2)?,
                sender: r.get(3)?,
                received_at: r.get(4)?,
                status: r.get(5)?,
                attempt_count: r.get(6)?,
                last_attempt_at: r.get(7)?,
                last_error: r.get(8)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(out)
}
