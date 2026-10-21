//! specs/091: which advertised P-CSCF a line registers through, and which it
//! is avoiding.
//!
//! TS 24.229 §5.1.1.2 and §5.1.2A.1.6: when the P-CSCF itself stops answering
//! while the access is fine, the UE marks it unavailable and registers through
//! another one it was given. The supervisor publishes the whole advertised pool
//! beside its single selection (`ims::pcscf_files`); this module turns that into
//! a choice per registration attempt.
//!
//! Pure apart from [`PcscfPool::load`]/[`PcscfPool::save`]: the clock is passed
//! in as a `SystemTime` on every call, so every rule is testable without a
//! socket or a sleep. Wall-clock time, not `Instant`, because the marks outlive
//! the process (FR-015): the supervisor restarts this agent, and a restart must
//! not walk straight back into a dead P-CSCF.
//!
//! What counts as a failure of the P-CSCF is decided by the caller
//! (`classify_register_failure`, the route probe, spec 090's qualifying
//! endpoint failure, the Gm keepalive escalation). This module only records it.

use crate::ims::pcscf_files::{Outage, PoolState, PoolStateMark, POOL_STATE_VERSION};
use crate::ims::PcscfSnapshot;
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// TS 24.229's Timer F (64 × T1). Only FR-008's threshold uses it: a `503`
/// whose `Retry-After` is longer marks the P-CSCF for that long.
pub(crate) const TIMER_F: Duration = Duration::from_secs(128);

/// RFC 5626 §4.5 base and cap for the retry delay; the spec adds 5 minutes.
const MARK_BASE: Duration = Duration::from_secs(30);
const MARK_CAP: Duration = Duration::from_secs(1800);
const MARK_EXTRA: Duration = Duration::from_secs(300);

/// Where a mark ends when `now + delay` cannot be represented: an absurd
/// `Retry-After` must not overflow `SystemTime` and panic the agent, and a
/// genuine one, however long, is honoured in full (Greptile, PR #130).
const FAR_FUTURE: Duration = Duration::from_secs(100 * 365 * 24 * 3600);

/// A failure attributable to the P-CSCF itself (FR-007, FR-008).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FailureCause {
    /// A REGISTER transaction timed out.
    RegisterTimeout,
    /// Connect refused, connect timed out, or a send on a new connection failed.
    Transport,
    /// Spec 090's qualifying endpoint failure, carrying its cause.
    EndpointFailure(String),
    /// The Gm keepalive went unanswered and every reconnect failed.
    GmKeepalive,
    /// `503` to REGISTER with a `Retry-After` longer than [`TIMER_F`].
    RetryAfter(Duration),
}

impl std::fmt::Display for FailureCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FailureCause::RegisterTimeout => f.write_str("register-timeout"),
            FailureCause::Transport => f.write_str("transport"),
            FailureCause::EndpointFailure(c) => write!(f, "endpoint-failure:{c}"),
            FailureCause::GmKeepalive => f.write_str("gm-keepalive"),
            FailureCause::RetryAfter(_) => f.write_str(RETRY_AFTER_CAUSE),
        }
    }
}

/// How a [`FailureCause::RetryAfter`] mark's cause is written, and recognised
/// once persisted.
const RETRY_AFTER_CAUSE: &str = "retry-after";

/// Whether a mark with this persisted cause could have been caused by the path
/// to the ePDG going down: every cause is a silence (a timeout, a failed
/// connect or send, an unanswered keepalive or probe) except `retry-after`,
/// which is the P-CSCF answering. An answer proves the path worked when it
/// came, so it is the P-CSCF's own word, whenever it came.
fn could_be_the_path(cause: &str) -> bool {
    cause != RETRY_AFTER_CAUSE
}

/// The P-CSCF failure a registration attempt amounts to, if any (FR-007–FR-009,
/// research R4, R12): a timed-out transaction, a connect or send that failed
/// over a live route, or a `503` whose `Retry-After` outlasts [`TIMER_F`].
/// Every other outcome — no route, a peer close, a SIM or AKA fault, any other
/// final response — is `None` and never marks.
///
/// Whether a route existed is the caller's second check (`ims::route_to`): a
/// connect timing out with the tunnel gone is still the tunnel's fault.
pub(crate) fn cause_of(f: &crate::ims::session::RegisterFailure) -> Option<FailureCause> {
    match f.status {
        Some(503) => f
            .retry_after
            .filter(|d| *d > TIMER_F)
            .map(FailureCause::RetryAfter),
        Some(_) => None,
        None => match crate::ims::classify_register_failure(&f.error) {
            crate::ims::FailureClass::Timeout => Some(FailureCause::RegisterTimeout),
            crate::ims::FailureClass::Transport => Some(FailureCause::Transport),
            _ => None,
        },
    }
}

/// Why a candidate was chosen; carried into the `"P-CSCF selected"` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectReason {
    /// The agent's first choice.
    Startup,
    /// The active P-CSCF is still eligible, so it is kept (FR-012).
    Sticky,
    /// The active P-CSCF was just marked unavailable.
    Failover,
    /// The active P-CSCF is no longer advertised.
    PoolReplaced,
    /// Every candidate is marked; the soonest-expiring one is tried (FR-013).
    AllUnavailable,
}

impl std::fmt::Display for SelectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SelectReason::Startup => "startup",
            SelectReason::Sticky => "sticky",
            SelectReason::Failover => "failover",
            SelectReason::PoolReplaced => "pool-replaced",
            SelectReason::AllUnavailable => "all-unavailable",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) addr: IpAddr,
    pub(crate) reason: SelectReason,
}

/// Failure bookkeeping for one address. Kept after it expires, so the next
/// failure grows the delay; dropped by a successful registration through the
/// address, or when the address leaves the pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mark {
    pub(crate) until: SystemTime,
    pub(crate) failures: u32,
    pub(crate) cause: String,
    /// When this mark was last set or renewed. Persisted so the supervisor
    /// can tell a mark against its current tunnel from one that outlived the
    /// rebuild it caused (specs/097); `UNIX_EPOCH` for a mark loaded from a
    /// file written before the field existed.
    pub(crate) marked_at: SystemTime,
}

/// How long a P-CSCF stays unavailable after its `failures`-th consecutive
/// failure: the RFC 5626 §4.5 delay (30 s doubling, capped at 1800 s, taking
/// the spec's base as the first failure's delay) plus 5 minutes. No jitter:
/// there is one UE per line, so no herd to spread, and the tests stay exact.
pub(crate) fn mark_duration(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    (MARK_BASE * (1u32 << doublings)).min(MARK_CAP) + MARK_EXTRA
}

fn to_unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// RFC 3339 for a log line, range-checked: `DateTime::from(SystemTime)`
/// panics past chrono's last representable date, which a hostile
/// `Retry-After` can reach without overflowing `SystemTime` itself (Greptile,
/// PR #130). Out of range, the raw unix seconds are logged instead.
fn rfc3339(t: SystemTime) -> String {
    let secs = to_unix(t);
    i64::try_from(secs)
        .ok()
        .and_then(|s| chrono::DateTime::<chrono::Utc>::from_timestamp(s, 0))
        .map_or_else(|| format!("unix:{secs}"), |d| d.to_rfc3339())
}

/// One line's P-CSCF candidates, marks and choice.
#[derive(Debug)]
pub(crate) struct PcscfPool {
    /// The latest negotiation's addresses, in publication order.
    pool: Vec<IpAddr>,
    /// The supervisor's single selection (`/tmp/pcscf-N`).
    selection: IpAddr,
    active: Option<IpAddr>,
    last_registered: Option<IpAddr>,
    marks: BTreeMap<IpAddr, Mark>,
    /// `None` keeps the state in memory only.
    state_path: Option<PathBuf>,
    /// The address the previous selection returned, so an unchanged sticky
    /// choice is not logged on every renewal.
    last_selected: Option<IpAddr>,
    /// Set by [`Self::replace_pool`] when it removed the active P-CSCF.
    active_replaced: bool,
}

impl PcscfPool {
    /// A pool for `selection` and the published `pool`, with any state a
    /// previous run of this agent left at `state_path` reconciled against it
    /// (FR-014, FR-015). An empty `pool` means none was published: the
    /// selection alone (FR-006).
    pub(crate) fn new(selection: IpAddr, pool: Vec<IpAddr>, state_path: Option<PathBuf>) -> Self {
        let mut p = Self {
            pool: dedup(if pool.is_empty() {
                vec![selection]
            } else {
                pool
            }),
            selection,
            active: None,
            last_registered: None,
            marks: BTreeMap::new(),
            state_path,
            last_selected: None,
            active_replaced: false,
        };
        p.load();
        p
    }

    /// Addresses this line may register through: the pool plus the
    /// selection, which may be missing from an inconsistent pool file.
    fn known(&self, addr: IpAddr) -> bool {
        addr == self.selection || self.pool.contains(&addr)
    }

    pub(crate) fn is_marked(&self, addr: IpAddr, now: SystemTime) -> bool {
        self.marks.get(&addr).is_some_and(|m| m.until > now)
    }

    /// Candidates in selection order (FR-005): the last P-CSCF that registered
    /// successfully when it is still advertised and unmarked (clarification
    /// Q3), then the supervisor's selection, then the rest of the pool, IPv4
    /// before IPv6.
    pub(crate) fn candidates(&self, now: SystemTime) -> Vec<IpAddr> {
        let mut out = Vec::with_capacity(self.pool.len() + 1);
        if let Some(l) = self.last_registered {
            if self.known(l) && !self.is_marked(l, now) {
                out.push(l);
            }
        }
        out.push(self.selection);
        out.extend(self.pool.iter().filter(|a| a.is_ipv4()));
        out.extend(self.pool.iter().filter(|a| a.is_ipv6()));
        dedup(out)
    }

    /// Whether any candidate is eligible now: decides between failing over at
    /// once and the backoff ladder (research R7).
    pub(crate) fn has_unmarked(&self, now: SystemTime) -> bool {
        self.candidates(now)
            .iter()
            .any(|a| !self.is_marked(*a, now))
    }

    /// The candidate a failover would move to now, if any is eligible.
    pub(crate) fn next_eligible(&self, now: SystemTime) -> Option<IpAddr> {
        self.candidates(now)
            .into_iter()
            .find(|a| !self.is_marked(*a, now))
    }

    /// How long until the soonest mark among the candidates ends; zero when a
    /// candidate is already eligible.
    pub(crate) fn soonest_expiry(&self, now: SystemTime) -> Duration {
        self.candidates(now)
            .iter()
            .map(|a| {
                self.marks
                    .get(a)
                    .and_then(|m| m.until.duration_since(now).ok())
                    .unwrap_or_default()
            })
            .min()
            .unwrap_or_default()
    }

    /// The P-CSCF for the next registration attempt, which becomes the active
    /// one. Keeps a working active P-CSCF (FR-012); otherwise the first
    /// eligible candidate (FR-011); otherwise, with every candidate marked, the
    /// one whose mark ends soonest, ties going to candidate order (FR-013,
    /// clarification Q4).
    pub(crate) fn select(&mut self, now: SystemTime) -> Selection {
        let candidates = self.candidates(now);
        let selection = match self.active {
            Some(a) if self.known(a) && !self.is_marked(a, now) => Selection {
                addr: a,
                reason: SelectReason::Sticky,
            },
            _ => match candidates.iter().find(|a| !self.is_marked(**a, now)) {
                Some(&addr) => Selection {
                    addr,
                    reason: if self.active_replaced {
                        SelectReason::PoolReplaced
                    } else if self.active.is_some() {
                        SelectReason::Failover
                    } else {
                        SelectReason::Startup
                    },
                },
                None => {
                    let addr = candidates
                        .iter()
                        .enumerate()
                        .min_by_key(|(i, a)| (self.marks.get(a).map(|m| m.until), *i))
                        .map(|(_, a)| *a)
                        .unwrap_or(self.selection);
                    Selection {
                        addr,
                        reason: SelectReason::AllUnavailable,
                    }
                }
            },
        };
        let previous = self.last_selected;
        if previous != Some(selection.addr) || selection.reason != SelectReason::Sticky {
            let cause = previous
                .and_then(|p| self.marks.get(&p))
                .filter(|_| selection.reason == SelectReason::Failover)
                .map(|m| m.cause.clone());
            tracing::info!(
                pcscf = %selection.addr,
                reason = %selection.reason,
                cause = cause.as_deref().unwrap_or(""),
                previous = %previous.map(|p| p.to_string()).unwrap_or_default(),
                candidates = %join(&candidates),
                "P-CSCF selected"
            );
        }
        self.active = Some(selection.addr);
        self.last_selected = Some(selection.addr);
        self.active_replaced = false;
        selection
    }

    /// Records a qualifying failure of `addr` (FR-007, FR-008, FR-010). A
    /// failure observed on any address but the active one belongs to an
    /// earlier registration and is ignored (FR-017). Returns the mark applied.
    pub(crate) fn on_failure(
        &mut self,
        addr: IpAddr,
        cause: FailureCause,
        now: SystemTime,
    ) -> Option<Mark> {
        if self.active != Some(addr) {
            tracing::info!(
                pcscf = %addr,
                cause = %cause,
                why = "not-active",
                "failure not attributed to the P-CSCF; not marking"
            );
            return None;
        }
        let failures = self.marks.get(&addr).map_or(0, |m| m.failures) + 1;
        let unavailable = match &cause {
            FailureCause::RetryAfter(d) => *d,
            _ => mark_duration(failures),
        };
        let mark = Mark {
            until: now
                .checked_add(unavailable)
                .unwrap_or_else(|| now + FAR_FUTURE),
            failures,
            cause: cause.to_string(),
            marked_at: now,
        };
        self.marks.insert(addr, mark.clone());
        let remaining = self
            .candidates(now)
            .iter()
            .filter(|a| !self.is_marked(**a, now))
            .count();
        tracing::warn!(
            pcscf = %addr,
            cause = %cause,
            failures,
            until = %rfc3339(mark.until),
            unavailable_secs = unavailable.as_secs(),
            remaining,
            "P-CSCF marked unavailable"
        );
        self.save();
        Some(mark)
    }

    /// A REGISTER through `addr` succeeded: it is the active and the last
    /// working P-CSCF, and its failure count starts over (FR-010).
    pub(crate) fn on_registered(&mut self, addr: IpAddr) {
        self.active = Some(addr);
        self.last_selected = Some(addr);
        self.last_registered = Some(addr);
        self.marks.remove(&addr);
        self.save();
    }

    /// A newly published pool replaces the candidates (FR-014). Marks and the
    /// remembered working P-CSCF go with any address that is no longer
    /// advertised; a reorder of the same set changes nothing. Returns whether
    /// anything changed.
    pub(crate) fn replace_pool(&mut self, pool: Vec<IpAddr>, selection: IpAddr) -> bool {
        let pool = dedup(if pool.is_empty() {
            vec![selection]
        } else {
            pool
        });
        let same_set = pool.len() == self.pool.len() && pool.iter().all(|a| self.pool.contains(a));
        if same_set && selection == self.selection {
            return false;
        }
        self.pool = pool;
        self.selection = selection;
        let before = self.marks.len();
        self.marks
            .retain(|a, _| a == &selection || self.pool.contains(a));
        let dropped_marks = before - self.marks.len();
        if self.last_registered.is_some_and(|l| !self.known(l)) {
            self.last_registered = None;
        }
        if self.active.is_some_and(|a| !self.known(a)) {
            self.active = None;
            self.active_replaced = true;
        }
        tracing::info!(
            pool = %join(&self.pool),
            dropped_marks,
            "P-CSCF pool replaced"
        );
        self.save();
        true
    }

    /// Drops the marks set during `outage`, the last one the path to the
    /// ePDG came back from (specs/100 FR-011): those P-CSCFs went silent
    /// because the path did, and a mark on them would only hold the agent back
    /// once the path is back. Kept: a mark of unknown age, and a `Retry-After`
    /// the P-CSCF actually sent, which is a pause it asked for, not an outage
    /// (the window reaches back before the hold, so such a reply can fall in
    /// it). Returns how many went.
    pub(crate) fn forget_outage(&mut self, outage: Option<Outage>) -> usize {
        let Some(outage) = outage else { return 0 };
        let before = self.marks.len();
        self.marks
            .retain(|_, m| !(outage.contains(m.marked_at) && could_be_the_path(&m.cause)));
        let dropped = before - self.marks.len();
        if dropped > 0 {
            tracing::info!(
                dropped,
                outage_from = %rfc3339(UNIX_EPOCH + Duration::from_secs(outage.from)),
                outage_to = %rfc3339(UNIX_EPOCH + Duration::from_secs(outage.to)),
                "dropped P-CSCF marks set while the path to the ePDG was down"
            );
            self.save();
        }
        dropped
    }

    /// For status (FR-020): only marks still in force, in candidate order.
    pub(crate) fn snapshot(&self, now: SystemTime) -> PcscfSnapshot {
        let pool = self.candidates(now);
        let unavailable = pool
            .iter()
            .filter_map(|a| {
                self.marks
                    .get(a)
                    .filter(|m| m.until > now)
                    .map(|m| (*a, m.until))
            })
            .collect();
        PcscfSnapshot {
            active: self.active,
            pool,
            unavailable,
        }
    }

    /// Reads what a previous run of this agent left, keeping only what still
    /// applies to the current pool. A missing file is a clean start; a corrupt
    /// or foreign one is too, said once.
    fn load(&mut self) {
        let Some(path) = &self.state_path else { return };
        let raw = match std::fs::read_to_string(path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "P-CSCF state unreadable; starting clean");
                return;
            }
        };
        let persisted: PoolState = match serde_json::from_str(&raw) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "P-CSCF state unreadable; starting clean");
                return;
            }
        };
        if persisted.version != POOL_STATE_VERSION {
            tracing::warn!(
                path = %path.display(),
                error = %format!("unsupported version {}", persisted.version),
                "P-CSCF state unreadable; starting clean"
            );
            return;
        }
        self.last_registered = persisted.last_registered.filter(|a| self.known(*a));
        for m in persisted.marks {
            // An expiry SystemTime cannot hold is a corrupt entry, not a
            // reason to crash every startup.
            let Some(until) = UNIX_EPOCH.checked_add(Duration::from_secs(m.until_unix)) else {
                tracing::warn!(
                    path = %path.display(),
                    error = %format!("mark for {} has an impossible expiry", m.addr),
                    "P-CSCF state unreadable; starting clean"
                );
                continue;
            };
            if self.known(m.addr) {
                self.marks.insert(
                    m.addr,
                    Mark {
                        until,
                        failures: m.failures,
                        cause: m.cause,
                        // 0 (a file from before specs/097) or an impossible
                        // value both read as "unknown", which never counts.
                        marked_at: UNIX_EPOCH
                            .checked_add(Duration::from_secs(m.marked_at_unix))
                            .unwrap_or(UNIX_EPOCH),
                    },
                );
            }
        }
    }

    /// Writes the state atomically. A failure costs only restart survival, so
    /// it is logged and otherwise ignored.
    fn save(&self) {
        let Some(path) = &self.state_path else { return };
        let persisted = PoolState {
            version: POOL_STATE_VERSION,
            last_registered: self.last_registered,
            marks: self
                .marks
                .iter()
                .map(|(a, m)| PoolStateMark {
                    addr: *a,
                    until_unix: to_unix(m.until),
                    failures: m.failures,
                    cause: m.cause.clone(),
                    marked_at_unix: to_unix(m.marked_at),
                })
                .collect(),
        };
        let result = serde_json::to_string(&persisted)
            .map_err(std::io::Error::other)
            .and_then(|json| crate::ims::pcscf_files::write_atomic(path, &json));
        if let Err(e) = result {
            tracing::warn!(path = %path.display(), error = %e, "could not save P-CSCF state");
        }
    }
}

fn dedup(addrs: Vec<IpAddr>) -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = Vec::with_capacity(addrs.len());
    for a in addrs {
        if !out.contains(&a) {
            out.push(a);
        }
    }
    out
}

fn join(addrs: &[IpAddr]) -> String {
    addrs
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "10.0.0.10";
    const B: &str = "10.0.1.10";
    const C: &str = "10.0.2.10";
    const V6: &str = "2402:8100::10";

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn ips(a: &[&str]) -> Vec<IpAddr> {
        a.iter().map(|s| ip(s)).collect()
    }

    fn t0() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_792_000_000)
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn mem(selection: &str, pool: &[&str]) -> PcscfPool {
        PcscfPool::new(ip(selection), ips(pool), None)
    }

    fn temp_state() -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pcscf-pool-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("pcscf-0.state")
    }

    // --- candidates and plain selection (T008) ---

    #[test]
    fn candidates_put_the_selection_first_then_ipv4_then_ipv6() {
        let p = mem(B, &[V6, A, B, C]);
        assert_eq!(p.candidates(t0()), ips(&[B, A, C, V6]));
    }

    #[test]
    fn a_selection_missing_from_the_pool_is_still_tried_first() {
        let p = mem(C, &[A, B]);
        assert_eq!(p.candidates(t0()), ips(&[C, A, B]));
    }

    #[test]
    fn no_published_pool_means_the_selection_alone() {
        let p = mem(A, &[]);
        assert_eq!(p.candidates(t0()), ips(&[A]));
    }

    #[test]
    fn the_first_choice_is_the_selection_and_then_it_sticks() {
        let mut p = mem(A, &[A, B]);
        assert_eq!(
            p.select(t0()),
            Selection {
                addr: ip(A),
                reason: SelectReason::Startup
            }
        );
        p.on_registered(ip(A));
        assert_eq!(p.select(t0()).reason, SelectReason::Sticky);
    }

    // --- marks and failover (T012, US1) ---

    #[test]
    fn mark_durations_grow_per_rfc5626_plus_five_minutes_and_cap() {
        let got: Vec<u64> = (1..=9).map(|k| mark_duration(k).as_secs()).collect();
        assert_eq!(got, vec![330, 360, 420, 540, 780, 1260, 2100, 2100, 2100]);
        assert_eq!(mark_duration(u32::MAX).as_secs(), 2100);
    }

    #[test]
    fn a_failure_marks_the_active_pcscf_and_moves_to_the_next() {
        let mut p = mem(A, &[A, B]);
        p.select(t0());
        let m = p
            .on_failure(ip(A), FailureCause::RegisterTimeout, t0())
            .unwrap();
        assert_eq!(m.until, t0() + secs(330));
        assert_eq!(m.failures, 1);
        assert!(p.has_unmarked(t0()));
        assert_eq!(
            p.select(t0()),
            Selection {
                addr: ip(B),
                reason: SelectReason::Failover
            }
        );
    }

    #[test]
    fn consecutive_failures_of_one_address_grow_its_mark() {
        let mut p = mem(A, &[A]);
        let mut until = Vec::new();
        for i in 0..3u64 {
            let now = t0() + secs(i * 10_000);
            p.select(now);
            until.push(
                p.on_failure(ip(A), FailureCause::Transport, now)
                    .unwrap()
                    .until
                    .duration_since(now)
                    .unwrap()
                    .as_secs(),
            );
        }
        assert_eq!(until, vec![330, 360, 420]);
    }

    #[test]
    fn retry_after_sets_the_mark_and_still_counts() {
        let mut p = mem(A, &[A, B]);
        p.select(t0());
        let m = p
            .on_failure(ip(A), FailureCause::RetryAfter(secs(600)), t0())
            .unwrap();
        assert_eq!(m.until, t0() + secs(600));
        assert_eq!(m.failures, 1);
    }

    #[test]
    fn a_working_pcscf_is_kept_after_an_earlier_one_recovers() {
        // US1 AS4 / FR-012.
        let mut p = mem(A, &[A, B]);
        p.select(t0());
        p.on_failure(ip(A), FailureCause::Transport, t0());
        p.select(t0());
        p.on_registered(ip(B));
        let later = t0() + secs(3600);
        assert!(!p.is_marked(ip(A), later));
        assert_eq!(
            p.select(later),
            Selection {
                addr: ip(B),
                reason: SelectReason::Sticky
            }
        );
    }

    #[test]
    fn a_success_resets_the_failure_count() {
        let mut p = mem(A, &[A, B]);
        p.select(t0());
        p.on_failure(ip(A), FailureCause::Transport, t0());
        p.on_registered(ip(A));
        p.select(t0());
        let m = p.on_failure(ip(A), FailureCause::Transport, t0()).unwrap();
        assert_eq!(m.failures, 1);
    }

    // --- stale events (T024, FR-017) ---

    #[test]
    fn a_failure_on_an_address_that_is_not_active_marks_nothing() {
        let mut p = mem(A, &[A, B]);
        p.select(t0());
        p.on_failure(ip(A), FailureCause::Transport, t0());
        p.select(t0());
        p.on_registered(ip(B));
        // A late keepalive verdict about A's old registration.
        assert_eq!(p.on_failure(ip(A), FailureCause::GmKeepalive, t0()), None);
        assert_eq!(p.marks[&ip(A)].failures, 1);
        assert!(!p.is_marked(ip(B), t0()));
        assert_eq!(p.select(t0()).addr, ip(B));
    }

    #[test]
    fn a_failure_before_any_selection_marks_nothing() {
        let mut p = mem(A, &[A, B]);
        assert_eq!(p.on_failure(ip(A), FailureCause::Transport, t0()), None);
    }

    // --- exhaustion and pool replacement (T029, US3) ---

    #[test]
    fn with_every_candidate_marked_the_soonest_expiring_is_tried() {
        let mut p = mem(A, &[A, B]);
        p.select(t0());
        p.on_failure(ip(A), FailureCause::RetryAfter(secs(900)), t0());
        p.select(t0());
        p.on_failure(ip(B), FailureCause::Transport, t0());
        assert!(!p.has_unmarked(t0()));
        assert_eq!(
            p.select(t0()),
            Selection {
                addr: ip(B),
                reason: SelectReason::AllUnavailable
            }
        );
    }

    #[test]
    fn equal_expiries_go_to_candidate_order() {
        let mut p = mem(A, &[A, B]);
        p.select(t0());
        p.on_failure(ip(A), FailureCause::Transport, t0());
        p.select(t0());
        p.on_failure(ip(B), FailureCause::Transport, t0());
        assert_eq!(p.select(t0()).addr, ip(A));
    }

    #[test]
    fn a_single_address_pool_is_still_selected_when_marked() {
        let mut p = mem(A, &[]);
        p.select(t0());
        p.on_failure(ip(A), FailureCause::Transport, t0());
        assert_eq!(
            p.select(t0()),
            Selection {
                addr: ip(A),
                reason: SelectReason::AllUnavailable
            }
        );
    }

    #[test]
    fn a_new_pool_drops_marks_and_memory_for_departed_addresses() {
        let mut p = mem(A, &[A, B]);
        p.select(t0());
        p.on_failure(ip(A), FailureCause::Transport, t0());
        p.select(t0());
        p.on_registered(ip(B));
        p.on_failure(ip(B), FailureCause::Transport, t0());
        assert!(p.replace_pool(ips(&[C, "10.0.3.10"]), ip(C)));
        assert!(p.marks.is_empty());
        assert_eq!(p.last_registered, None);
        assert_eq!(
            p.select(t0()),
            Selection {
                addr: ip(C),
                reason: SelectReason::PoolReplaced
            }
        );
    }

    #[test]
    fn a_reordered_or_grown_pool_keeps_the_marks_of_addresses_still_there() {
        let mut p = mem(A, &[A, B]);
        p.select(t0());
        p.on_failure(ip(A), FailureCause::Transport, t0());
        assert!(
            !p.replace_pool(ips(&[B, A]), ip(A)),
            "a reorder is not a change"
        );
        assert!(p.replace_pool(ips(&[B, A, C]), ip(A)));
        assert!(p.is_marked(ip(A), t0()));
        assert_eq!(p.candidates(t0()), ips(&[A, B, C]));
    }

    // --- persistence (T033, US4) ---

    #[test]
    fn marks_and_the_last_working_pcscf_survive_a_restart() {
        let path = temp_state();
        {
            let mut p = PcscfPool::new(ip(A), ips(&[A, B]), Some(path.clone()));
            p.select(t0());
            p.on_failure(ip(A), FailureCause::RegisterTimeout, t0());
            p.select(t0());
            p.on_registered(ip(B));
        }
        assert!(!path.with_extension("state.tmp").exists());
        let restarted = PcscfPool::new(ip(A), ips(&[A, B]), Some(path.clone()));
        assert_eq!(restarted.last_registered, Some(ip(B)));
        let m = &restarted.marks[&ip(A)];
        assert_eq!((m.failures, m.cause.as_str()), (1, "register-timeout"));
        assert_eq!(m.until, t0() + secs(330));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_restart_inside_a_mark_goes_to_the_next_candidate() {
        // US4 AS1.
        let path = temp_state();
        {
            let mut p = PcscfPool::new(ip(A), ips(&[A, B]), Some(path.clone()));
            p.select(t0());
            p.on_failure(ip(A), FailureCause::Transport, t0());
        }
        let mut p = PcscfPool::new(ip(A), ips(&[A, B]), Some(path.clone()));
        assert_eq!(p.select(t0() + secs(60)).addr, ip(B));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_restart_after_the_mark_expires_stays_on_the_working_pcscf() {
        // US4 AS2, clarification Q3.
        let path = temp_state();
        {
            let mut p = PcscfPool::new(ip(A), ips(&[A, B]), Some(path.clone()));
            p.select(t0());
            p.on_failure(ip(A), FailureCause::Transport, t0());
            p.select(t0());
            p.on_registered(ip(B));
        }
        let mut p = PcscfPool::new(ip(A), ips(&[A, B]), Some(path.clone()));
        assert_eq!(p.select(t0() + secs(3600)).addr, ip(B));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_restart_with_the_working_pcscf_marked_returns_to_normal_order() {
        // US4 AS3.
        let path = temp_state();
        {
            let mut p = PcscfPool::new(ip(A), ips(&[A, B]), Some(path.clone()));
            p.select(t0());
            p.on_failure(ip(A), FailureCause::Transport, t0());
            p.select(t0());
            p.on_registered(ip(B));
            p.on_failure(ip(B), FailureCause::Transport, t0() + secs(3000));
        }
        let mut p = PcscfPool::new(ip(A), ips(&[A, B]), Some(path.clone()));
        let s = p.select(t0() + secs(3100));
        assert_eq!((s.addr, s.reason), (ip(A), SelectReason::Startup));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn loading_against_a_new_pool_drops_departed_addresses() {
        let path = temp_state();
        {
            let mut p = PcscfPool::new(ip(A), ips(&[A, B]), Some(path.clone()));
            p.select(t0());
            p.on_failure(ip(A), FailureCause::Transport, t0());
            p.select(t0());
            p.on_registered(ip(B));
        }
        let p = PcscfPool::new(ip(C), ips(&[C]), Some(path.clone()));
        assert!(p.marks.is_empty());
        assert_eq!(p.last_registered, None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // --- the marking time the supervisor reads (specs/097) ---

    #[test]
    fn a_mark_records_when_it_was_set_and_a_renewal_moves_it() {
        let path = temp_state();
        let mut p = PcscfPool::new(ip(A), ips(&[A]), Some(path.clone()));
        p.select(t0());
        let m = p
            .on_failure(ip(A), FailureCause::GmKeepalive, t0())
            .unwrap();
        assert_eq!(m.marked_at, t0());
        let saved = crate::ims::pcscf_files::read_pool_state(&path).unwrap();
        assert_eq!(saved.marks[0].marked_at_unix, to_unix(t0()));
        assert_eq!(saved.marks[0].cause, "gm-keepalive");

        let later = t0() + secs(400);
        p.select(later);
        let m = p.on_failure(ip(A), FailureCause::Transport, later).unwrap();
        assert_eq!(m.marked_at, later, "a renewal is a fresh failure");
        let saved = crate::ims::pcscf_files::read_pool_state(&path).unwrap();
        assert_eq!(saved.marks[0].marked_at_unix, to_unix(later));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_restart_keeps_the_marking_time() {
        let path = temp_state();
        {
            let mut p = PcscfPool::new(ip(A), ips(&[A, B]), Some(path.clone()));
            p.select(t0());
            p.on_failure(ip(A), FailureCause::RegisterTimeout, t0());
        }
        let p = PcscfPool::new(ip(A), ips(&[A, B]), Some(path.clone()));
        assert_eq!(p.marks[&ip(A)].marked_at, t0());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_state_file_without_marking_times_still_steers_failover() {
        let path = temp_state();
        let until = to_unix(t0() + secs(600));
        std::fs::write(
            &path,
            format!(
                r#"{{"version":1,"last_registered":null,"marks":[
                {{"addr":"{A}","until_unix":{until},"failures":1,"cause":"transport"}}]}}"#
            ),
        )
        .unwrap();
        let mut p = PcscfPool::new(ip(A), ips(&[A, B]), Some(path.clone()));
        assert_eq!(p.marks[&ip(A)].marked_at, UNIX_EPOCH);
        assert_eq!(p.select(t0()).addr, ip(B), "the old mark still applies");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_expired_mark_keeps_its_count_but_not_its_block() {
        let path = temp_state();
        {
            let mut p = PcscfPool::new(ip(A), ips(&[A, B]), Some(path.clone()));
            p.select(t0());
            p.on_failure(ip(A), FailureCause::Transport, t0());
        }
        let mut p = PcscfPool::new(ip(A), ips(&[A, B]), Some(path.clone()));
        let later = t0() + secs(3600);
        assert_eq!(p.select(later).addr, ip(A));
        let m = p.on_failure(ip(A), FailureCause::Transport, later).unwrap();
        assert_eq!(m.failures, 2);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_absurd_retry_after_is_not_a_panic_and_a_long_one_is_kept() {
        let mut p = mem(A, &[A, B]);
        p.select(t0());
        let m = p
            .on_failure(
                ip(A),
                FailureCause::RetryAfter(Duration::from_secs(u64::MAX)),
                t0(),
            )
            .unwrap();
        assert_eq!(m.until, t0() + FAR_FUTURE);
        let mut p = mem(A, &[A, B]);
        p.select(t0());
        let m = p
            .on_failure(ip(A), FailureCause::RetryAfter(secs(172_800)), t0())
            .unwrap();
        assert_eq!(m.until, t0() + secs(172_800), "two days stays two days");
        // Past chrono's last date but still a valid SystemTime: the log line
        // that reports the mark must not panic.
        let mut p = mem(A, &[A, B]);
        p.select(t0());
        let m = p
            .on_failure(
                ip(A),
                FailureCause::RetryAfter(secs(10_000_000_000_000)),
                t0(),
            )
            .unwrap();
        assert_eq!(m.until, t0() + secs(10_000_000_000_000));
        assert!(rfc3339(m.until).starts_with("unix:"));
    }

    #[test]
    fn an_impossible_stored_expiry_is_dropped_not_a_panic() {
        let path = temp_state();
        std::fs::write(
            &path,
            format!(
                r#"{{"version":1,"last_registered":null,"marks":[{{"addr":"{A}","until_unix":{},"failures":1}}]}}"#,
                u64::MAX
            ),
        )
        .unwrap();
        let p = PcscfPool::new(ip(A), ips(&[A, B]), Some(path.clone()));
        assert!(p.marks.is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_foreign_or_missing_state_starts_clean() {
        for contents in [
            Some("{not json"),
            Some(r#"{"version":2,"last_registered":"10.0.1.10","marks":[]}"#),
            None,
        ] {
            let path = temp_state();
            if let Some(c) = contents {
                std::fs::write(&path, c).unwrap();
            }
            let p = PcscfPool::new(ip(A), ips(&[A, B]), Some(path.clone()));
            assert!(p.marks.is_empty() && p.last_registered.is_none());
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }
    }

    #[test]
    fn an_unwritable_state_path_does_not_stop_failover() {
        let mut p = PcscfPool::new(
            ip(A),
            ips(&[A, B]),
            Some(PathBuf::from("/nonexistent-dir/pcscf-0.state")),
        );
        p.select(t0());
        assert!(p.on_failure(ip(A), FailureCause::Transport, t0()).is_some());
        assert_eq!(p.select(t0()).addr, ip(B));
    }

    // --- what a failed attempt amounts to (T011/T023, FR-007–FR-009, R12) ---

    fn failure(
        error: &str,
        status: Option<u16>,
        retry_after: Option<u64>,
    ) -> crate::ims::session::RegisterFailure {
        crate::ims::session::RegisterFailure {
            error: crate::error::BridgeError::Ims(error.into()),
            status,
            retry_after: retry_after.map(secs),
        }
    }

    #[test]
    fn only_the_pcscfs_own_failures_amount_to_a_cause() {
        let refused = "TCP connect to 10.0.0.10:5060 failed: Connection refused (os error 111)";
        let unreachable =
            "TCP connect to 10.0.0.10:5060 failed: Network is unreachable (os error 101)";
        let cases = [
            (failure(refused, None, None), Some(FailureCause::Transport)),
            (
                failure("timed out waiting for a SIP response", None, None),
                Some(FailureCause::RegisterTimeout),
            ),
            (
                failure("rejected: 503 Service Unavailable", Some(503), Some(600)),
                Some(FailureCause::RetryAfter(secs(600))),
            ),
            (failure(unreachable, None, None), None),
            (
                failure(
                    "connection closed by peer with no data (0 bytes read)",
                    None,
                    None,
                ),
                None,
            ),
            (failure("AT+CSIM failed: 0", None, None), None),
            (failure("rejected: 403 Forbidden", Some(403), None), None),
            (
                failure("rejected: 503 Service Unavailable", Some(503), None),
                None,
            ),
            (
                failure("rejected: 503 Service Unavailable", Some(503), Some(60)),
                None,
            ),
            (
                failure("rejected: 503 Service Unavailable", Some(503), Some(128)),
                None,
            ),
        ];
        for (f, want) in cases {
            assert_eq!(cause_of(&f), want, "{} {:?}", f.error, f.status);
        }
    }

    #[test]
    fn forget_outage_drops_only_the_marks_set_inside_it_and_saves() {
        let dir = std::env::temp_dir().join(format!("pcscf-outage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pcscf-0.state");
        let mut p = PcscfPool::new(ip(A), ips(&[A, B]), Some(path.clone()));
        let before = t0();
        let during = t0() + Duration::from_secs(100);
        p.select(before);
        p.on_failure(ip(A), FailureCause::GmKeepalive, before);
        p.select(during);
        p.on_failure(ip(B), FailureCause::GmKeepalive, during);
        let outage = Outage {
            from: to_unix(t0() + Duration::from_secs(50)),
            to: to_unix(t0() + Duration::from_secs(200)),
        };
        assert_eq!(p.forget_outage(None), 0);
        assert_eq!(p.forget_outage(Some(outage)), 1);
        assert!(p.marks.contains_key(&ip(A)), "set before the outage");
        assert!(!p.marks.contains_key(&ip(B)), "set during it");
        assert_eq!(p.forget_outage(Some(outage)), 0, "already gone");
        let restarted = PcscfPool::new(ip(A), ips(&[A, B]), Some(path));
        assert_eq!(restarted.marks.len(), 1, "the drop was saved");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn forget_outage_keeps_a_retry_after_the_pcscf_sent_inside_the_window() {
        // A 503 Retry-After: 600 just before the route went: the P-CSCF
        // answered, so its pause stands when the path comes back.
        let mut p = mem(A, &[A, B]);
        let at = t0() + Duration::from_secs(100);
        p.select(at);
        p.on_failure(
            ip(A),
            FailureCause::RetryAfter(Duration::from_secs(600)),
            at,
        );
        p.select(at);
        p.on_failure(ip(B), FailureCause::GmKeepalive, at);
        let outage = Outage {
            from: to_unix(t0()),
            to: to_unix(t0() + Duration::from_secs(200)),
        };
        assert_eq!(p.forget_outage(Some(outage)), 1);
        assert_eq!(p.marks[&ip(A)].cause, "retry-after");
        assert!(!p.marks.contains_key(&ip(B)));
    }

    #[test]
    fn next_eligible_and_soonest_expiry_follow_the_marks() {
        let mut p = mem(A, &[A, B]);
        assert_eq!(p.next_eligible(t0()), Some(ip(A)));
        assert_eq!(p.soonest_expiry(t0()), Duration::ZERO);
        p.select(t0());
        p.on_failure(ip(A), FailureCause::RetryAfter(secs(900)), t0());
        assert_eq!(p.next_eligible(t0()), Some(ip(B)));
        p.select(t0());
        p.on_failure(ip(B), FailureCause::RetryAfter(secs(500)), t0());
        assert_eq!(p.next_eligible(t0()), None);
        assert_eq!(p.soonest_expiry(t0()), secs(500));
    }

    // --- status (T039, US5) ---

    #[test]
    fn the_snapshot_lists_only_marks_in_force_in_candidate_order() {
        let mut p = mem(A, &[A, B, C]);
        p.select(t0());
        p.on_failure(ip(A), FailureCause::Transport, t0());
        p.select(t0());
        p.on_failure(ip(B), FailureCause::RetryAfter(secs(10_000)), t0());
        let snap = p.snapshot(t0() + secs(1000));
        assert_eq!(snap.active, Some(ip(B)));
        assert_eq!(snap.pool, ips(&[A, B, C]));
        assert_eq!(snap.unavailable, vec![(ip(B), t0() + secs(10_000))]);
    }
}
