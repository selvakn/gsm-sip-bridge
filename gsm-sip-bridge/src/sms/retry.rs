//! Retry of undelivered SMS Discord notifications (specs/121-sms-alert-retry).
//!
//! Delivery state lives in the shared SQLite `sms` table. The daemon and both
//! VoWiFi agents each own a [`SmsForwarder`] and may sweep concurrently, so
//! duplicate-send safety is the atomic lease claim in `store::sms`, never an
//! in-process lock.

use super::discord::DiscordClient;
use crate::config::SmsConfig;
use crate::control::protocol::SmsOutcome;
use crate::error::{BridgeError, BridgeResult};
use crate::observability::reporter::Reporter;
use crate::store::sms::{self, RETRY_LEASE_SECS};
use crate::store::Transport;
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Handle;

/// Rows re-posted per sweep pass. A long outage's backlog therefore drains
/// across several triggers rather than in one burst against Discord.
const SWEEP_BATCH: usize = 50;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// A Discord client plus everything needed to retry what it fails to deliver.
/// Replaces a bare `DiscordClient` wherever SMS is forwarded, so the retry
/// settings travel with the client instead of through every call chain.
#[derive(Clone)]
pub struct SmsForwarder {
    pub(super) client: DiscordClient,
    pub(super) db_path: String,
    pub(super) retry_enabled: bool,
    retry_interval: Duration,
    max_age_secs: i64,
    /// `module_id` → the SIM's own number, used for the `Phone` field of a
    /// retried embed. Seeded from configuration and extended by every fresh
    /// text that carries its number, so a retry names the right SIM.
    phones: Arc<Mutex<HashMap<String, String>>>,
}

impl SmsForwarder {
    pub fn new(client: DiscordClient, cfg: &SmsConfig, phones: HashMap<String, String>) -> Self {
        Self {
            client,
            db_path: cfg.db_path.clone(),
            retry_enabled: cfg.retry_enabled,
            retry_interval: Duration::from_secs(cfg.retry_interval_sec),
            max_age_secs: cfg.retry_max_age_sec as i64,
            phones: Arc::new(Mutex::new(phones)),
        }
    }

    pub(super) fn remember_phone(&self, module_id: &str, phone: &str) {
        if let Ok(mut map) = self.phones.lock() {
            map.insert(module_id.to_string(), phone.to_string());
        }
    }

    fn phone_for(&self, module_id: &str) -> Option<String> {
        self.phones.lock().ok()?.get(module_id).cloned()
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepOutcome {
    pub attempted: usize,
    pub delivered: usize,
    pub still_failing: usize,
    pub abandoned: usize,
    /// The pass stopped on a Discord rate limit; the next trigger continues.
    pub stopped_early: bool,
}

/// Runs a blocking store operation on a short-lived connection. Never held
/// across an `.await`, so a slow Discord post cannot pin a SQLite handle.
async fn with_conn<T, F>(db_path: &str, f: F) -> BridgeResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> BridgeResult<T> + Send + 'static,
{
    let path = db_path.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = open(&path)?;
        f(&conn)
    })
    .await
    .map_err(|e| BridgeError::Store(format!("retry store task failed: {e}")))?
}

fn open(db_path: &str) -> BridgeResult<Connection> {
    let conn = Connection::open(db_path)
        .map_err(|e| BridgeError::Store(format!("failed to open store: {e}")))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| BridgeError::Store(format!("failed to set busy_timeout: {e}")))?;
    Ok(conn)
}

/// Releases a row's lease if the sweep leaves a row without recording an
/// outcome — an early return or the task being cancelled mid-post. Without
/// it the row would stay unclaimable until the lease ran out.
struct LeaseGuard {
    db_path: String,
    id: i64,
    armed: bool,
}

impl LeaseGuard {
    fn new(db_path: &str, id: i64) -> Self {
        Self {
            db_path: db_path.to_string(),
            id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        match open(&self.db_path).and_then(|c| sms::release_claim(&c, self.id)) {
            Ok(()) => {}
            Err(e) => tracing::warn!(id = self.id, error = %e, "failed to release SMS retry lease"),
        }
    }
}

/// One retry pass: abandon what has outlived `retry_max_age_sec`, then
/// re-post undelivered texts oldest-first, stopping early on a rate limit.
///
/// At-least-once, by design: if the process dies after Discord accepts a post
/// but before [`sms::mark_sent`] lands, the lease expires and the text is
/// posted again. That window is the accepted limit in the spec's edge cases;
/// closing it would need Discord-side idempotency, which webhooks lack.
pub async fn sweep(fwd: &SmsForwarder, reporter: &Option<Reporter>) -> SweepOutcome {
    sweep_excluding(fwd, reporter, None).await
}

/// [`sweep`] that leaves one text alone: the `(module_id, received_at)` whose
/// first attempt just failed. The "next alert" trigger runs right after that
/// attempt, and whether its lease has already been cleared by the store
/// writer is a race; without the exclusion a failing webhook would be hit
/// twice back to back instead of on the next alert or timer tick.
pub async fn sweep_excluding(
    fwd: &SmsForwarder,
    reporter: &Option<Reporter>,
    just_attempted: Option<(&str, &str)>,
) -> SweepOutcome {
    let mut out = SweepOutcome::default();
    if !fwd.retry_enabled {
        return out;
    }
    let db = fwd.db_path.as_str();
    let now = chrono::Utc::now().timestamp();

    let max_age = fwd.max_age_secs;
    match with_conn(db, move |c| sms::abandon_expired(c, now, max_age)).await {
        Ok(n) => out.abandoned = n,
        Err(e) => {
            tracing::warn!(error = %e, "SMS retry sweep: could not read the store");
            return out;
        }
    }

    let candidates = match with_conn(db, move |c| sms::candidates(c, now, SWEEP_BATCH)).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "SMS retry sweep: could not list candidates");
            return out;
        }
    };

    for cand in candidates {
        if just_attempted == Some((cand.module_id.as_str(), cand.received_at.as_str())) {
            continue;
        }
        let id = cand.id;
        let now = chrono::Utc::now().timestamp();
        match with_conn(db, move |c| {
            sms::claim_for_retry(c, id, now, RETRY_LEASE_SECS)
        })
        .await
        {
            Ok(true) => {}
            // Another process (or a concurrent sweep) owns it now.
            Ok(false) => continue,
            Err(e) => {
                tracing::warn!(id, error = %e, "SMS retry sweep: claim failed");
                continue;
            }
        }
        let mut guard = LeaseGuard::new(db, id);
        out.attempted += 1;

        let phone = fwd.phone_for(&cand.module_id);
        let result = fwd
            .client
            .forward_sms(
                &cand.module_id,
                &cand.sender,
                &cand.body,
                &cand.received_at,
                phone.as_deref(),
                true,
            )
            .await;

        let transport = Transport::parse(&cand.transport).unwrap_or(Transport::Cs);
        match result {
            Ok(code) => {
                let forwarded_at = chrono::Utc::now().to_rfc3339();
                let stored = with_conn(db, move |c| {
                    sms::mark_sent(c, id, &forwarded_at, code as i32)
                })
                .await;
                match stored {
                    Ok(()) => guard.disarm(),
                    // The guard still releases the lease; the row stays
                    // undelivered in the store and is re-sent next pass.
                    Err(e) => tracing::error!(id, error = %e, "delivered but could not record it"),
                }
                out.delivered += 1;
                super::report_outcome(&cand.module_id, transport, SmsOutcome::Sent, reporter);
                tracing::info!(id, module = %cand.module_id, "late SMS delivered to Discord");
            }
            Err(e) => {
                let category = e.category();
                let code = e.status_code().map(i32::from);
                let stored = with_conn(db, move |c| sms::mark_failed(c, id, &category, code)).await;
                if stored.is_ok() {
                    guard.disarm();
                }
                out.still_failing += 1;
                super::report_outcome(&cand.module_id, transport, SmsOutcome::Failed, reporter);
                tracing::warn!(id, module = %cand.module_id, error = %e, "SMS retry failed");
                if matches!(e, super::discord::PostError::RateLimited { .. }) {
                    out.stopped_early = true;
                    break;
                }
            }
        }
    }

    if out.attempted > 0 || out.abandoned > 0 {
        tracing::info!(
            attempted = out.attempted,
            delivered = out.delivered,
            still_failing = out.still_failing,
            abandoned = out.abandoned,
            stopped_early = out.stopped_early,
            "SMS retry sweep finished"
        );
    }
    out
}

/// Sweeps every `retry_interval_sec`, first tick one full interval after
/// start: a restart storm must not hammer Discord, and a text waits at most
/// one interval after a restart. `Delay` turns a stalled runtime or a clock
/// jump into one late tick instead of a burst.
pub fn spawn_timer(
    handle: &Handle,
    fwd: SmsForwarder,
    reporter: Option<Reporter>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !fwd.retry_enabled {
        return None;
    }
    let period = fwd.retry_interval;
    Some(handle.spawn(async move {
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            sweep(&fwd, &reporter).await;
        }
    }))
}
