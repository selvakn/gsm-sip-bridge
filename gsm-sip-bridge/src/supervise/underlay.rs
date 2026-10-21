//! The underlay guard's per-line bookkeeping (specs/100): the verdict on the
//! path to the ePDG, the hold it can put a line in, and the on-demand
//! liveness check that settles a doubt.
//!
//! The steady-state loop owns one [`UnderlayTracker`] per line. Before each
//! tick it hands the tracker what it observed ([`Observation`]) and gets back
//! the [`UnderlayView`] to give the tick, plus any hold that ended; after the
//! tick it hands over the outcome. Everything here is a pure decision over
//! those inputs and an injected clock, so the whole lifecycle is tested without
//! charon, a log file or a sleep.
//!
//! Every rule here is stated in time, never in ticks: the loop ticks every 30s
//! normally and every 5s around a hold, so "the next tick" or "since the
//! previous tick" means a different span depending on when it is asked.

use super::engines::LogMark;
use super::line_supervisor::{
    DegradeReason, IkeEvidence, PendingView, SteadyOutcome, UnderlayVerdict, UnderlayView,
    RECENT_LIFE,
};
use crate::control::protocol::{HoldEndedBy, UplinkHoldCause, UplinkState};
use crate::ims::pcscf_files::Outage;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// A failed `ip route get` must be repeated this much later to count
/// (specs/100 FR-002): two ticks at the 30s cadence, and never two reads of one
/// transient.
pub const ROUTE_DEBOUNCE: Duration = Duration::from_secs(10);

/// The longest a line stays held continuously (FR-012), whatever the cause
/// along the way. A guard that never gave up would turn a wrong verdict into an
/// outage with no end.
pub const MAX_HOLD: Duration = Duration::from_secs(15 * 60);

/// How long a liveness check is given to be answered (FR-004). Until then the
/// recovery it is for stays deferred; after it, an unanswered check holds the
/// line.
pub const LIVENESS_WAIT: Duration = Duration::from_secs(30);

/// How often the loop ticks while a hold is on. The ordinary tick is 30s;
/// ticking faster makes the end of the outage visible sooner. Nothing in the
/// tick may count ticks as time because of this (see the module docs).
pub const HELD_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long the quick polling carries on after a hold ends. The tunnel comes
/// back with a new P-CSCF, which the very next tick would pick up and hand to
/// the agent; at the ordinary 30s that is up to 30s of a restored line not yet
/// registering.
pub const LINGER_AFTER_HOLD: Duration = Duration::from_secs(30);

/// Unanswered retransmits of an outstanding request that establish "no
/// response" on their own (research R4).
pub const NO_RESPONSE_RETRANSMITS: u32 = 2;

/// How far before a hold its outage is taken to reach, at most, when the ePDG
/// was last heard from longer ago than that. An idle line can go minutes
/// without hearing from it; the outage is not that old.
pub const OUTAGE_LOOKBACK_MAX: Duration = Duration::from_secs(5 * 60);

/// How far before a hold its outage is taken to reach when the ePDG has not
/// been heard from at all since supervision began: about what a no-route hold
/// takes to detect (two route checks 10s apart, across a 30s tick).
pub const OUTAGE_LOOKBACK_UNHEARD: Duration = Duration::from_secs(60);

/// One line's hold: one continuous stretch of the path being down, which may
/// change cause along the way without ending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    pub cause: UplinkHoldCause,
    /// When the stretch began, for the [`MAX_HOLD`] bound.
    pub started: Instant,
    pub started_at: SystemTime,
    /// When the current cause began, for the time reported against it.
    pub cause_since: Instant,
    /// When the path was last known to work: the outage began after this.
    pub last_good_at: SystemTime,
    /// The recoveries held back under the current cause, deduplicated, for
    /// the line that ends it.
    pub skipped: Vec<DegradeReason>,
}

/// A liveness check in flight (FR-004).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingCheck {
    /// Taken just before the check was sent; anything the ePDG says after it
    /// answers the check.
    pub mark: LogMark,
    /// The tunnel's received-packet counter at the same point: a packet counted
    /// after it answers the check too.
    pub rx: Option<u64>,
    /// When the tick that sent it began. The answer is waited for
    /// [`LIVENESS_WAIT`] from here.
    pub sent: Instant,
    pub deferred: DegradeReason,
}

/// A hold that began, or a hold whose cause changed to this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldStart {
    pub cause: UplinkHoldCause,
}

/// A hold that ended, or the end of one cause within it
/// ([`HoldEndedBy::CauseChanged`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldEnd {
    pub cause: UplinkHoldCause,
    /// The time under `cause`.
    pub held: Duration,
    pub ended_by: HoldEndedBy,
    pub skipped: Vec<DegradeReason>,
}

impl HoldEnd {
    /// The path came back. Only then is what was learned during the outage
    /// (the agent's P-CSCF marks, charon's backed-off retries) stale: at the
    /// bound, on a change of cause or on shutdown the path is not known to be
    /// back.
    pub fn path_returned(&self) -> bool {
        self.ended_by == HoldEndedBy::Restored
    }
}

/// When the ePDG was heard from, as closely as one tick can tell: something
/// arrived after `after` and by `by`. A tick sees only what accumulated since
/// the previous one, so the two are that tick and the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heard {
    pub after: Instant,
    pub by: Instant,
}

impl Heard {
    fn latest(a: Option<Self>, b: Option<Self>) -> Option<Self> {
        match (a, b) {
            (Some(a), Some(b)) => Some(Self {
                after: a.after.max(b.after),
                by: a.by.max(b.by),
            }),
            (a, b) => a.or(b),
        }
    }
}

/// What each line has heard from each ePDG, so lines sharing an ePDG share
/// the evidence their verdicts rest on (FR-001). One line's IKE_SA failing
/// while another hears from the same ePDG is that line's fault, not the
/// path's, and is repaired rather than held.
///
/// Only evidence crosses lines. Holds, pending checks and strikes stay per
/// line, because recovery does.
#[derive(Debug, Default)]
pub struct EpdgHeard {
    by_epdg: Mutex<HashMap<String, HashMap<u32, Heard>>>,
}

impl EpdgHeard {
    pub fn record(&self, epdg: &str, line: u32, heard: Heard) {
        let mut map = self.by_epdg.lock().unwrap_or_else(|e| e.into_inner());
        let lines = map.entry(epdg.to_owned()).or_default();
        let merged = Heard::latest(lines.get(&line).copied(), Some(heard));
        if let Some(merged) = merged {
            lines.insert(line, merged);
        }
    }

    /// The latest any *other* line has heard from `epdg`. A line's own
    /// evidence it reads more precisely from the log.
    pub fn others(&self, epdg: &str, line: u32) -> Option<Heard> {
        let map = self.by_epdg.lock().unwrap_or_else(|e| e.into_inner());
        map.get(epdg)?
            .iter()
            .filter(|(l, _)| **l != line)
            .fold(None, |acc, (_, h)| Heard::latest(acc, Some(*h)))
    }
}

/// What the loop saw this tick, before running it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Observation {
    /// `ip route get <ePDG>`.
    pub route_ok: bool,
    /// The log since [`UnderlayTracker::evidence_mark`].
    pub evidence: IkeEvidence,
    /// A parsed message since the previous tick.
    pub parsed_since_tick: bool,
    /// The tunnel interface's received-packet counter, read now.
    pub rx_packets: Option<u64>,
    /// Whether the ePDG said anything since the pending check was sent; `None`
    /// when there is no pending check.
    pub pending_life: Option<bool>,
    /// What other lines on the same ePDG have heard from it
    /// ([`EpdgHeard::others`]).
    pub sibling_heard: Option<Heard>,
    /// The log's current position.
    pub mark: LogMark,
}

/// What [`UnderlayTracker::begin_tick`] decided.
#[derive(Debug, Clone)]
pub struct Begin {
    pub view: UnderlayView,
    pub ended: Option<HoldEnd>,
    pub started: Option<HoldStart>,
    /// What this line heard from the ePDG this tick, for the other lines on
    /// it ([`EpdgHeard::record`]).
    pub heard: Option<Heard>,
}

#[derive(Debug, Clone)]
pub struct UnderlayTracker {
    verdict: UnderlayVerdict,
    route_failures: u32,
    first_route_failure: Option<Instant>,
    last_rx: Option<u64>,
    /// This tick's own counter reading, `None` when it failed: the baseline
    /// for a liveness check the tick sends.
    tick_rx: Option<u64>,
    /// When the previous tick began: the start of what this tick sees.
    prev_tick: Option<Instant>,
    /// The latest this line itself heard from the ePDG.
    heard: Option<Heard>,
    /// Silence is counted from here: the log mark retransmits are read from,
    /// and the time it was taken. Both move whenever the ePDG is heard from by
    /// any means, so an answer of any kind restarts the count.
    evidence_mark: LogMark,
    silence_since: Instant,
    tick_mark: LogMark,
    pub hold: Option<Hold>,
    pub pending_check: Option<PendingCheck>,
    /// Quick polling goes on until this, after a hold has ended.
    linger_until: Option<Instant>,
    /// The last outage the path came back from (FR-011), for the IMS agent.
    last_outage: Option<Outage>,
}

impl UnderlayTracker {
    pub fn new(mark: LogMark, now: Instant) -> Self {
        Self {
            verdict: UnderlayVerdict::Up,
            route_failures: 0,
            first_route_failure: None,
            last_rx: None,
            tick_rx: None,
            prev_tick: None,
            heard: None,
            evidence_mark: mark,
            silence_since: now,
            tick_mark: mark,
            hold: None,
            pending_check: None,
            linger_until: None,
            last_outage: None,
        }
    }

    /// The last outage the path came back from: from when it was last known
    /// to work until the hold ended because it returned.
    pub fn last_outage(&self) -> Option<Outage> {
        self.last_outage
    }

    /// The pause before the next tick: [`HELD_POLL_INTERVAL`] while a hold is
    /// on and for [`LINGER_AFTER_HOLD`] after one ends, the ordinary
    /// steady-state interval otherwise.
    pub fn poll_after(&self, now: Instant) -> Duration {
        if self.hold.is_some() || self.linger_until.is_some_and(|until| now < until) {
            HELD_POLL_INTERVAL
        } else {
            super::line_supervisor::STEADY_STATE_POLL_INTERVAL
        }
    }

    /// Where the evidence for the next verdict starts.
    pub fn evidence_mark(&self) -> LogMark {
        self.evidence_mark
    }

    /// The previous tick's log position, where "recent life" starts.
    pub fn tick_mark(&self) -> LogMark {
        self.tick_mark
    }

    /// The mark a pending check's answer is read from.
    pub fn pending_mark(&self) -> Option<LogMark> {
        self.pending_check.map(|p| p.mark)
    }

    pub fn state(&self) -> UplinkState {
        match self.verdict {
            UnderlayVerdict::Up => UplinkState::Up,
            UnderlayVerdict::NoRoute => UplinkState::NoRoute,
            UnderlayVerdict::NoResponse => UplinkState::NoResponse,
        }
    }

    /// Computes the verdict for this tick and the view the tick is given.
    pub fn begin_tick(&mut self, obs: &Observation, now: Instant, now_at: SystemTime) -> Begin {
        let window_from = self.prev_tick.replace(now).unwrap_or(now);
        self.tick_mark = obs.mark;
        self.tick_rx = obs.rx_packets;

        // Debounce the route check (FR-002).
        if obs.route_ok {
            self.route_failures = 0;
            self.first_route_failure = None;
        } else {
            if self.route_failures == 0 {
                self.first_route_failure = Some(now);
            }
            self.route_failures = self.route_failures.saturating_add(1);
        }
        let route_down = !obs.route_ok
            && self.route_failures >= 2
            && self
                .first_route_failure
                .is_some_and(|first| now.saturating_duration_since(first) >= ROUTE_DEBOUNCE);

        // What this line heard this tick: a parsed message, or the tunnel
        // decrypting something.
        let rx_grew = matches!(
            (self.last_rx, obs.rx_packets),
            (Some(before), Some(after)) if after > before
        );
        if obs.rx_packets.is_some() {
            self.last_rx = obs.rx_packets;
        }
        let heard_now = (obs.parsed_since_tick || rx_grew).then_some(Heard {
            after: window_from,
            by: now,
        });
        self.heard = Heard::latest(self.heard, heard_now);
        let sibling_heard_since = |t: Instant| obs.sibling_heard.is_some_and(|h| h.after >= t);

        // Alive since silence began. The log orders a parsed message against
        // the retransmits after it; a counter or another line's word cannot be
        // ordered within the window, so either one restarts the count.
        let heard = (obs.evidence.parsed_from_peer && obs.evidence.unanswered_retransmits == 0)
            || rx_grew
            || sibling_heard_since(self.silence_since);

        let previous = self.verdict;
        let mut verdict = if route_down {
            // Local state is checked first and is the cheaper explanation.
            UnderlayVerdict::NoRoute
        } else if previous == UnderlayVerdict::NoResponse {
            if heard {
                UnderlayVerdict::Up
            } else {
                UnderlayVerdict::NoResponse
            }
        } else if previous == UnderlayVerdict::NoRoute {
            // The route is back (FR-006). Whatever charon retransmitted while it
            // was gone says nothing about the ePDG, so judge afresh.
            UnderlayVerdict::Up
        } else if obs.evidence.unanswered_retransmits >= NO_RESPONSE_RETRANSMITS && !heard {
            UnderlayVerdict::NoResponse
        } else {
            UnderlayVerdict::Up
        };

        // Silence is counted from a mark that moves on whenever the line is up
        // and nothing is outstanding, or the ePDG has been heard from, so the
        // log slice stays short and old retransmits are never counted against
        // a path that has since answered. It stays put while retransmits are
        // being counted.
        if verdict == UnderlayVerdict::Up
            && (previous != UnderlayVerdict::Up
                || heard
                || obs.evidence.unanswered_retransmits == 0)
        {
            self.restart_silence(obs.mark, now);
        }

        // The bound (FR-012): one ordinary pass, whatever the verdict says.
        let mut ended = None;
        let mut started = None;
        let mut unheld_pass = false;
        if let Some(hold) = &self.hold {
            if now.saturating_duration_since(hold.started) >= MAX_HOLD {
                ended = self.end_hold(HoldEndedBy::MaxHold, now);
                verdict = UnderlayVerdict::Up;
                self.restart_silence(obs.mark, now);
                unheld_pass = true;
            }
        }

        if !unheld_pass {
            match (self.hold.as_ref().map(|h| h.cause), verdict.hold_cause()) {
                (Some(current), Some(cause)) if current != cause => {
                    // Still down, for a different reason: the same hold, so
                    // the bound still runs from when it began.
                    ended = self.change_cause(cause, now);
                    started = Some(HoldStart { cause });
                }
                (Some(_), None) => {
                    let last_good_at = self.hold.as_ref().map(|h| h.last_good_at);
                    ended = self.end_hold(HoldEndedBy::Restored, now);
                    self.last_outage = last_good_at.map(|from| Outage {
                        from: unix_secs(from),
                        to: unix_secs(now_at),
                    });
                }
                (None, Some(cause)) => {
                    self.start_hold(cause, now, now_at);
                    started = Some(HoldStart { cause });
                }
                _ => {}
            }
        }
        self.verdict = verdict;
        if started.is_some() {
            // The suspicion the check was for is now a verdict.
            self.pending_check = None;
        }

        // A no-route hold that ended because the route came back: whatever
        // charon was retrying across the dead path should be retried now.
        let restored = ended
            .as_ref()
            .is_some_and(|e| e.path_returned() && e.cause == UplinkHoldCause::NoRoute);
        let pending = self.pending_check.map(|p| PendingView {
            reason: p.deferred,
            // The log, the tunnel counter growing since the check went out,
            // or another line on the ePDG hearing from it since. Measured from
            // the check, not from the previous tick: traffic from before the
            // check is no answer to it.
            life_since_mark: obs.pending_life.unwrap_or(false)
                || matches!(
                    (p.rx, obs.rx_packets),
                    (Some(before), Some(after)) if after > before
                )
                || sibling_heard_since(p.sent),
            expired: now.saturating_duration_since(p.sent) >= LIVENESS_WAIT,
        });
        let heard_recently = Heard::latest(self.heard, obs.sibling_heard)
            .is_some_and(|h| now.saturating_duration_since(h.by) < RECENT_LIFE);
        Begin {
            view: UnderlayView {
                enabled: true,
                verdict,
                route_ok: obs.route_ok || unheld_pass,
                pending: pending.filter(|_| !unheld_pass),
                heard_recently: heard_recently || unheld_pass,
                restored,
            },
            ended,
            started,
            heard: heard_now,
        }
    }

    /// Takes the tick's outcome: records what a hold skipped, starts a hold
    /// the tick itself decided on (data-model rows 6 and 7), and keeps the
    /// pending check in step. `mark_before_tick` is the log position taken
    /// before the tick, i.e. before any liveness check the tick sent.
    pub fn end_tick(
        &mut self,
        outcome: &SteadyOutcome,
        mark_before_tick: LogMark,
        now: Instant,
        now_at: SystemTime,
    ) -> Option<HoldStart> {
        match outcome {
            SteadyOutcome::Deferred { reason } => {
                // A check already out keeps its mark and its clock: waiting on
                // it is not a new suspicion (FR-004, one exchange per
                // suspicion).
                self.pending_check.get_or_insert(PendingCheck {
                    mark: mark_before_tick,
                    // Read by this tick's `begin_tick`, before the check went out.
                    rx: self.tick_rx,
                    sent: now,
                    deferred: *reason,
                });
                None
            }
            SteadyOutcome::Held { cause, skipped, .. } => {
                let started = if self.hold.is_none() {
                    // Decided inside the tick: one failed route check with a
                    // recovery due (row 7), or a liveness check nobody
                    // answered (row 6).
                    if *cause == UplinkHoldCause::NoResponse {
                        if let Some(pending) = self.pending_check {
                            self.evidence_mark = pending.mark;
                            self.silence_since = pending.sent;
                        }
                    }
                    self.verdict = match cause {
                        UplinkHoldCause::NoRoute => UnderlayVerdict::NoRoute,
                        UplinkHoldCause::NoResponse => UnderlayVerdict::NoResponse,
                    };
                    self.start_hold(*cause, now, now_at);
                    Some(HoldStart { cause: *cause })
                } else {
                    None
                };
                if let Some(hold) = &mut self.hold {
                    if !hold.skipped.contains(skipped) {
                        hold.skipped.push(*skipped);
                    }
                }
                self.pending_check = None;
                started
            }
            // Whatever made the check necessary is gone or has been acted on.
            SteadyOutcome::StillUp
            | SteadyOutcome::PcscfChanged { .. }
            | SteadyOutcome::WaitingForNegotiation
            | SteadyOutcome::Recovered { .. } => {
                self.pending_check = None;
                None
            }
        }
    }

    /// Ends the hold because the supervisor is shutting down, so its time is
    /// counted. The verdict goes back to up with it: the shutdown reports
    /// `up`, and the status file written from [`Self::state`] must not go on
    /// showing a hold nobody is supervising.
    pub fn end_for_shutdown(&mut self, now: Instant) -> Option<HoldEnd> {
        self.verdict = UnderlayVerdict::Up;
        self.end_hold(HoldEndedBy::Shutdown, now)
    }

    fn restart_silence(&mut self, mark: LogMark, now: Instant) {
        self.evidence_mark = mark;
        self.silence_since = now;
    }

    fn start_hold(&mut self, cause: UplinkHoldCause, now: Instant, now_at: SystemTime) {
        // The last time the ePDG was heard from, within the bounds: before
        // that it was answering, so the outage cannot reach further back.
        // The start of the window it was heard in, not the end: the tick
        // only says it arrived somewhere in between, and the outage may have
        // begun right after (found live: the end put a dropped-inbound
        // outage's start 18s after the drop began).
        let ago = self
            .heard
            .map_or(OUTAGE_LOOKBACK_UNHEARD, |h| {
                now.saturating_duration_since(h.after)
            })
            .min(OUTAGE_LOOKBACK_MAX);
        self.hold = Some(Hold {
            cause,
            started: now,
            started_at: now_at,
            cause_since: now,
            last_good_at: now_at.checked_sub(ago).unwrap_or(now_at),
            skipped: Vec::new(),
        });
    }

    /// Reports the time under the old cause and carries on under the new one.
    fn change_cause(&mut self, cause: UplinkHoldCause, now: Instant) -> Option<HoldEnd> {
        let hold = self.hold.as_mut()?;
        let end = HoldEnd {
            cause: hold.cause,
            held: now.saturating_duration_since(hold.cause_since),
            ended_by: HoldEndedBy::CauseChanged,
            skipped: std::mem::take(&mut hold.skipped),
        };
        hold.cause = cause;
        hold.cause_since = now;
        Some(end)
    }

    fn end_hold(&mut self, ended_by: HoldEndedBy, now: Instant) -> Option<HoldEnd> {
        let hold = self.hold.take()?;
        self.linger_until = Some(now + LINGER_AFTER_HOLD);
        Some(HoldEnd {
            cause: hold.cause,
            held: now.saturating_duration_since(hold.cause_since),
            ended_by,
            skipped: hold.skipped,
        })
    }
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = Duration::from_secs(30);

    fn mark() -> LogMark {
        LogMark::default()
    }

    fn tracker() -> UnderlayTracker {
        UnderlayTracker::new(mark(), Instant::now())
    }

    fn quiet() -> Observation {
        Observation {
            route_ok: true,
            evidence: IkeEvidence::default(),
            parsed_since_tick: true,
            rx_packets: Some(100),
            pending_life: None,
            sibling_heard: None,
            mark: mark(),
        }
    }

    fn no_route() -> Observation {
        Observation {
            route_ok: false,
            ..quiet()
        }
    }

    fn silent(retransmits: u32) -> Observation {
        Observation {
            evidence: IkeEvidence {
                parsed_from_peer: false,
                unanswered_retransmits: retransmits,
            },
            ..quiet()
        }
    }

    struct Clock {
        t0: Instant,
        n: u32,
    }

    impl Clock {
        fn new() -> Self {
            Self {
                t0: Instant::now(),
                n: 0,
            }
        }

        fn next(&mut self) -> (Instant, SystemTime) {
            let at = self.t0 + TICK * self.n;
            self.n += 1;
            (at, SystemTime::UNIX_EPOCH + TICK * self.n)
        }

        fn begin(&mut self, tracker: &mut UnderlayTracker, obs: &Observation) -> Begin {
            let (now, at) = self.next();
            tracker.begin_tick(obs, now, at)
        }
    }

    #[test]
    fn a_held_line_is_polled_faster_and_stays_quick_for_a_while_after_it_ends() {
        let mut t = tracker();
        let t0 = Instant::now();
        let at = SystemTime::UNIX_EPOCH;
        assert_eq!(t.poll_after(t0), Duration::from_secs(30));
        t.begin_tick(&no_route(), t0, at);
        t.begin_tick(&no_route(), t0 + TICK, at);
        assert_eq!(t.poll_after(t0 + TICK), Duration::from_secs(5));
        // The route returns: the hold ends, and the quick polling lingers.
        let end = t0 + TICK * 2;
        let b = t.begin_tick(&quiet(), end, at);
        assert!(b.ended.is_some());
        assert_eq!(t.poll_after(end), Duration::from_secs(5));
        assert_eq!(
            t.poll_after(end + LINGER_AFTER_HOLD - Duration::from_secs(1)),
            Duration::from_secs(5)
        );
        assert_eq!(
            t.poll_after(end + LINGER_AFTER_HOLD),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn a_quiet_healthy_line_is_up_with_no_hold() {
        let mut clock = Clock::new();
        let mut t = tracker();
        for _ in 0..5 {
            let b = clock.begin(&mut t, &quiet());
            assert_eq!(b.view.verdict, UnderlayVerdict::Up);
            assert!(b.view.enabled && b.view.route_ok);
            assert!(b.started.is_none() && b.ended.is_none());
        }
        assert_eq!(t.state(), UplinkState::Up);
    }

    #[test]
    fn one_failed_route_check_is_not_a_verdict() {
        let mut clock = Clock::new();
        let mut t = tracker();
        let b = clock.begin(&mut t, &no_route());
        assert_eq!(b.view.verdict, UnderlayVerdict::Up);
        assert!(!b.view.route_ok, "but the tick is told, for row 7");
        assert!(b.started.is_none());
    }

    #[test]
    fn two_failed_route_checks_ten_seconds_apart_hold_the_line() {
        let mut clock = Clock::new();
        let mut t = tracker();
        clock.begin(&mut t, &no_route());
        let b = clock.begin(&mut t, &no_route());
        assert_eq!(b.view.verdict, UnderlayVerdict::NoRoute);
        assert_eq!(
            b.started,
            Some(HoldStart {
                cause: UplinkHoldCause::NoRoute
            })
        );
        assert_eq!(t.state(), UplinkState::NoRoute);
        assert_eq!(t.hold.as_ref().unwrap().cause, UplinkHoldCause::NoRoute);
        // Still held, but not started again.
        let b = clock.begin(&mut t, &no_route());
        assert_eq!(b.view.verdict, UnderlayVerdict::NoRoute);
        assert!(b.started.is_none());
    }

    #[test]
    fn two_failures_closer_than_ten_seconds_do_not_count() {
        let mut t = tracker();
        let t0 = Instant::now();
        let at = SystemTime::UNIX_EPOCH;
        t.begin_tick(&no_route(), t0, at);
        let b = t.begin_tick(&no_route(), t0 + Duration::from_secs(9), at);
        assert_eq!(b.view.verdict, UnderlayVerdict::Up);
        let b = t.begin_tick(&no_route(), t0 + Duration::from_secs(10), at);
        assert_eq!(b.view.verdict, UnderlayVerdict::NoRoute);
    }

    #[test]
    fn a_good_route_check_between_failures_restarts_the_debounce() {
        let mut clock = Clock::new();
        let mut t = tracker();
        clock.begin(&mut t, &no_route());
        clock.begin(&mut t, &quiet());
        let b = clock.begin(&mut t, &no_route());
        assert_eq!(b.view.verdict, UnderlayVerdict::Up);
    }

    #[test]
    fn the_hold_ends_as_soon_as_the_route_is_back() {
        let mut clock = Clock::new();
        let mut t = tracker();
        clock.begin(&mut t, &no_route());
        clock.begin(&mut t, &no_route());
        let b = clock.begin(&mut t, &quiet());
        assert_eq!(b.view.verdict, UnderlayVerdict::Up);
        let ended = b.ended.expect("hold ended");
        assert_eq!(ended.cause, UplinkHoldCause::NoRoute);
        assert_eq!(ended.ended_by, HoldEndedBy::Restored);
        assert_eq!(ended.held, TICK);
        assert!(t.hold.is_none());
    }

    #[test]
    fn only_a_restored_no_route_hold_flags_the_tick_that_follows_it() {
        // Route back: flagged, once.
        let mut clock = Clock::new();
        let mut t = tracker();
        clock.begin(&mut t, &no_route());
        clock.begin(&mut t, &no_route());
        let b = clock.begin(&mut t, &quiet());
        assert!(b.view.restored);
        assert!(!clock.begin(&mut t, &quiet()).view.restored);
        // A no-response hold ending means the ePDG was heard from: not flagged.
        let mut clock = Clock::new();
        let mut t = tracker();
        clock.begin(&mut t, &silent(2));
        let heard = Observation {
            evidence: IkeEvidence {
                parsed_from_peer: true,
                unanswered_retransmits: 0,
            },
            ..quiet()
        };
        let b = clock.begin(&mut t, &heard);
        assert!(b.ended.is_some() && !b.view.restored);
        // The 15-minute cut is not a restoration either.
        let mut t = tracker();
        let t0 = Instant::now();
        let at = SystemTime::UNIX_EPOCH;
        t.begin_tick(&no_route(), t0, at);
        t.begin_tick(&no_route(), t0 + TICK, at);
        let b = t.begin_tick(&no_route(), t0 + TICK + MAX_HOLD, at);
        assert!(b.ended.is_some() && !b.view.restored);
    }

    #[test]
    fn two_unanswered_retransmits_with_nothing_heard_is_no_response() {
        let mut clock = Clock::new();
        let mut t = tracker();
        let b = clock.begin(&mut t, &silent(1));
        assert_eq!(b.view.verdict, UnderlayVerdict::Up, "one is not enough");
        let b = clock.begin(&mut t, &silent(2));
        assert_eq!(b.view.verdict, UnderlayVerdict::NoResponse);
        assert_eq!(
            b.started,
            Some(HoldStart {
                cause: UplinkHoldCause::NoResponse
            })
        );
    }

    #[test]
    fn a_grown_tunnel_counter_is_life_even_with_retransmits_outstanding() {
        let mut clock = Clock::new();
        let mut t = tracker();
        clock.begin(&mut t, &quiet());
        let mut obs = silent(3);
        obs.rx_packets = Some(101);
        let b = clock.begin(&mut t, &obs);
        assert_eq!(b.view.verdict, UnderlayVerdict::Up);
    }

    #[test]
    fn no_response_stays_until_the_epdg_is_heard_from() {
        let mut clock = Clock::new();
        let mut t = tracker();
        clock.begin(&mut t, &silent(2));
        // charon gave up retransmitting, and the log goes quiet: still held.
        let b = clock.begin(&mut t, &quiet_but_unanswered());
        assert_eq!(b.view.verdict, UnderlayVerdict::NoResponse);
        // A parsed message with nothing outstanding after it ends it.
        let heard = Observation {
            evidence: IkeEvidence {
                parsed_from_peer: true,
                unanswered_retransmits: 0,
            },
            ..quiet()
        };
        let b = clock.begin(&mut t, &heard);
        assert_eq!(b.view.verdict, UnderlayVerdict::Up);
        assert_eq!(b.ended.unwrap().ended_by, HoldEndedBy::Restored);
    }

    fn quiet_but_unanswered() -> Observation {
        silent(2)
    }

    #[test]
    fn no_route_takes_precedence_over_no_response() {
        let mut clock = Clock::new();
        let mut t = tracker();
        clock.begin(&mut t, &silent(2));
        assert_eq!(t.state(), UplinkState::NoResponse);
        let both = Observation {
            route_ok: false,
            ..silent(4)
        };
        clock.begin(&mut t, &both);
        let b = clock.begin(&mut t, &both);
        assert_eq!(b.view.verdict, UnderlayVerdict::NoRoute);
        // The cause changed; the path never came back, so this is the end of
        // one cause within the same hold, not a restore.
        let ended = b.ended.unwrap();
        assert_eq!(ended.cause, UplinkHoldCause::NoResponse);
        assert_eq!(ended.ended_by, HoldEndedBy::CauseChanged);
        assert!(!ended.path_returned());
        assert!(!b.view.restored);
        assert_eq!(b.started.unwrap().cause, UplinkHoldCause::NoRoute);
    }

    #[test]
    fn a_change_of_cause_keeps_the_hold_and_its_fifteen_minute_bound() {
        let mut t = tracker();
        let t0 = Instant::now();
        let at = SystemTime::UNIX_EPOCH;
        t.begin_tick(&silent(2), t0, at);
        let began = t.hold.as_ref().unwrap().started;
        // The route goes 14 minutes in.
        let fourteen = t0 + Duration::from_secs(14 * 60);
        let both = Observation {
            route_ok: false,
            ..silent(4)
        };
        t.begin_tick(&both, fourteen, at);
        let b = t.begin_tick(&both, fourteen + TICK, at);
        let ended = b.ended.unwrap();
        assert_eq!(ended.ended_by, HoldEndedBy::CauseChanged);
        assert_eq!(ended.held, Duration::from_secs(14 * 60) + TICK);
        let hold = t.hold.as_ref().unwrap();
        assert_eq!(hold.cause, UplinkHoldCause::NoRoute);
        assert_eq!(hold.started, began, "the same hold");
        // So the bound falls at 15 minutes from the start, not 29.
        let b = t.begin_tick(&both, t0 + MAX_HOLD, at);
        let ended = b.ended.expect("cut at the bound");
        assert_eq!(ended.ended_by, HoldEndedBy::MaxHold);
        assert_eq!(ended.cause, UplinkHoldCause::NoRoute);
        assert_eq!(ended.held, MAX_HOLD - Duration::from_secs(14 * 60) - TICK);
    }

    #[test]
    fn a_grown_counter_restarts_the_silence_count() {
        // Retransmits outstanding while the tunnel still decrypts traffic:
        // alive. Silence is then counted afresh from this tick, so those same
        // retransmits cannot hold the line on a later quiet tick.
        let mut clock = Clock::new();
        let mut t = tracker();
        clock.begin(&mut t, &quiet());
        let grew = Observation {
            rx_packets: Some(101),
            mark: LogMark::at(500),
            ..silent(2)
        };
        let b = clock.begin(&mut t, &grew);
        assert_eq!(b.view.verdict, UnderlayVerdict::Up);
        assert_eq!(t.evidence_mark(), LogMark::at(500));
        // Without growth the mark stays put while retransmits are counted.
        let mut t = tracker();
        let stuck = Observation {
            mark: LogMark::at(500),
            ..silent(1)
        };
        clock.begin(&mut t, &stuck);
        assert_eq!(t.evidence_mark(), mark());
    }

    #[test]
    fn a_sibling_hearing_from_the_epdg_keeps_this_line_up_and_answers_its_check() {
        let mut clock = Clock::new();
        let mut t = tracker();
        clock.begin(&mut t, &quiet());
        // Another line on the same ePDG heard from it after this line's
        // silence began: this line's retransmits are its own trouble.
        let (now, _) = (clock.t0 + TICK, ());
        let sibling = Some(Heard {
            after: now,
            by: now + TICK,
        });
        let b = clock.begin(
            &mut t,
            &Observation {
                sibling_heard: sibling,
                ..silent(3)
            },
        );
        assert_eq!(b.view.verdict, UnderlayVerdict::Up);
        assert!(b.view.heard_recently);
        // A check sent now is answered by sibling traffic after it, and
        // not by sibling traffic from before it.
        let (sent, at) = clock.next();
        t.end_tick(
            &SteadyOutcome::Deferred {
                reason: DegradeReason::PcscfPoolExhausted,
            },
            mark(),
            sent,
            at,
        );
        let before = Observation {
            pending_life: Some(false),
            sibling_heard: Some(Heard {
                after: sent - Duration::from_secs(1),
                by: sent + TICK,
            }),
            ..quiet()
        };
        let b = clock.begin(&mut t, &before);
        assert!(!b.view.pending.unwrap().life_since_mark);
        let after = Observation {
            sibling_heard: Some(Heard {
                after: sent,
                by: sent + TICK * 2,
            }),
            ..before
        };
        let b = clock.begin(&mut t, &after);
        assert!(b.view.pending.unwrap().life_since_mark);
    }

    #[test]
    fn heard_recently_means_thirty_seconds_at_any_cadence() {
        let mut t = tracker();
        let t0 = Instant::now();
        let at = SystemTime::UNIX_EPOCH;
        let quick = Duration::from_secs(5);
        t.begin_tick(&quiet(), t0, at);
        let silent_tick = Observation {
            parsed_since_tick: false,
            ..quiet()
        };
        // Heard at t0; 25s of quick ticks later it is still recent.
        let mut now = t0;
        for _ in 0..5 {
            now += quick;
            assert!(t.begin_tick(&silent_tick, now, at).view.heard_recently);
        }
        now += quick;
        assert!(!t.begin_tick(&silent_tick, now, at).view.heard_recently);
    }

    #[test]
    fn a_restored_hold_records_its_outage_from_when_the_epdg_was_last_heard() {
        let mut t = tracker();
        let t0 = Instant::now();
        let at0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let tick = |n: u32| (t0 + TICK * n, at0 + TICK * n);
        // Heard on the first tick, then the route goes.
        let (now, at) = tick(0);
        t.begin_tick(&quiet(), now, at);
        let silent_no_route = Observation {
            parsed_since_tick: false,
            ..no_route()
        };
        for n in 1..=2 {
            let (now, at) = tick(n);
            t.begin_tick(&silent_no_route, now, at);
        }
        assert!(t.hold.is_some());
        assert_eq!(t.last_outage(), None, "not while held");
        let (now, at) = tick(3);
        let b = t.begin_tick(&quiet(), now, at);
        assert!(b.ended.unwrap().path_returned());
        assert_eq!(
            t.last_outage(),
            Some(Outage {
                from: 1_000_000,
                to: 1_000_000 + 90,
            })
        );
    }

    #[test]
    fn an_outage_starts_where_the_last_life_could_first_have_arrived() {
        // Heard on a tick whose window opened 30s earlier: the packet may be
        // that old, and the outage may have begun right after it.
        let mut t = tracker();
        let t0 = Instant::now();
        let at0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let silent = Observation {
            parsed_since_tick: false,
            ..quiet()
        };
        t.begin_tick(&silent, t0, at0);
        t.begin_tick(&quiet(), t0 + TICK, at0 + TICK);
        let silent_no_route = Observation {
            parsed_since_tick: false,
            ..no_route()
        };
        t.begin_tick(&silent_no_route, t0 + TICK * 2, at0 + TICK * 2);
        t.begin_tick(&silent_no_route, t0 + TICK * 3, at0 + TICK * 3);
        assert_eq!(t.hold.as_ref().unwrap().last_good_at, at0);
    }

    #[test]
    fn a_hold_cut_at_its_bound_records_no_outage() {
        let mut t = tracker();
        let t0 = Instant::now();
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        t.begin_tick(&no_route(), t0, at);
        t.begin_tick(&no_route(), t0 + TICK, at);
        let b = t.begin_tick(&no_route(), t0 + TICK + MAX_HOLD, at);
        assert_eq!(b.ended.unwrap().ended_by, HoldEndedBy::MaxHold);
        assert_eq!(t.last_outage(), None);
    }

    #[test]
    fn an_outage_reaches_back_at_most_five_minutes() {
        let mut t = tracker();
        let t0 = Instant::now();
        let at0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        t.begin_tick(&quiet(), t0, at0);
        let silent_no_route = Observation {
            parsed_since_tick: false,
            ..no_route()
        };
        // Last heard an hour before the route went.
        let later = Duration::from_secs(3600);
        t.begin_tick(&silent_no_route, t0 + later, at0 + later);
        t.begin_tick(&silent_no_route, t0 + later + TICK, at0 + later + TICK);
        let hold = t.hold.as_ref().unwrap();
        assert_eq!(hold.last_good_at, at0 + later + TICK - OUTAGE_LOOKBACK_MAX);
    }

    #[test]
    fn epdg_heard_shares_between_lines_on_one_epdg_only() {
        let shared = EpdgHeard::default();
        let t0 = Instant::now();
        let h = Heard {
            after: t0,
            by: t0 + TICK,
        };
        shared.record("192.0.2.1", 0, h);
        assert_eq!(shared.others("192.0.2.1", 1), Some(h));
        assert_eq!(shared.others("192.0.2.1", 0), None, "not its own");
        assert_eq!(shared.others("192.0.2.2", 1), None, "another ePDG");
        let later = Heard {
            after: t0 + TICK,
            by: t0 + TICK * 2,
        };
        shared.record("192.0.2.1", 2, later);
        assert_eq!(shared.others("192.0.2.1", 1), Some(later));
    }

    #[test]
    fn retransmits_from_the_outage_do_not_hold_a_line_whose_route_just_returned() {
        let mut clock = Clock::new();
        let mut t = tracker();
        clock.begin(&mut t, &no_route());
        clock.begin(&mut t, &no_route());
        // Route back, but the log slice still carries the outage's retransmits.
        let back = Observation {
            evidence: IkeEvidence {
                parsed_from_peer: false,
                unanswered_retransmits: 5,
            },
            ..quiet()
        };
        let b = clock.begin(&mut t, &back);
        assert_eq!(b.view.verdict, UnderlayVerdict::Up);
        assert!(t.hold.is_none());
    }

    #[test]
    fn a_hold_is_cut_at_fifteen_minutes_and_that_tick_runs_unheld() {
        let mut t = tracker();
        let t0 = Instant::now();
        let at = SystemTime::UNIX_EPOCH;
        t.begin_tick(&no_route(), t0, at);
        t.begin_tick(&no_route(), t0 + TICK, at);
        assert!(t.hold.is_some());
        let started = t.hold.as_ref().unwrap().started;
        // Just under the bound: still held.
        let b = t.begin_tick(&no_route(), started + MAX_HOLD - Duration::from_secs(1), at);
        assert_eq!(b.view.verdict, UnderlayVerdict::NoRoute);
        // At the bound: ended, and the tick is told everything is fine.
        let b = t.begin_tick(&no_route(), started + MAX_HOLD, at);
        let ended = b.ended.expect("ended at the bound");
        assert_eq!(ended.ended_by, HoldEndedBy::MaxHold);
        assert_eq!(ended.held, MAX_HOLD);
        assert_eq!(b.view.verdict, UnderlayVerdict::Up);
        assert!(b.view.route_ok && b.view.enabled);
        assert!(b.started.is_none());
        // The next tick, the route still failing, holds again on fresh evidence.
        let b = t.begin_tick(&no_route(), started + MAX_HOLD + TICK, at);
        assert_eq!(b.view.verdict, UnderlayVerdict::NoRoute);
        assert!(b.started.is_some());
    }

    #[test]
    fn shutdown_while_held_counts_the_time() {
        let mut clock = Clock::new();
        let mut t = tracker();
        clock.begin(&mut t, &no_route());
        clock.begin(&mut t, &no_route());
        assert_eq!(t.state(), UplinkState::NoRoute);
        let end = t.end_for_shutdown(clock.t0 + TICK * 5).unwrap();
        assert_eq!(end.ended_by, HoldEndedBy::Shutdown);
        assert_eq!(end.held, TICK * 4);
        // What the status file is written from on the way out.
        assert_eq!(t.state(), UplinkState::Up);
        assert!(t.end_for_shutdown(clock.t0 + TICK * 6).is_none());
    }

    #[test]
    fn held_recoveries_are_recorded_once_each() {
        let mut clock = Clock::new();
        let mut t = tracker();
        clock.begin(&mut t, &no_route());
        clock.begin(&mut t, &no_route());
        let (now, at) = clock.next();
        for skipped in [
            DegradeReason::PcscfPoolExhausted,
            DegradeReason::ChildSaStuck,
            DegradeReason::PcscfPoolExhausted,
        ] {
            let held = SteadyOutcome::Held {
                cause: UplinkHoldCause::NoRoute,
                skipped,
                initiated: false,
                ike_sa_loss_cause: None,
            };
            assert!(t.end_tick(&held, mark(), now, at).is_none());
        }
        let b = clock.begin(&mut t, &quiet());
        assert_eq!(
            b.ended.unwrap().skipped,
            vec![
                DegradeReason::PcscfPoolExhausted,
                DegradeReason::ChildSaStuck
            ]
        );
    }

    #[test]
    fn a_hold_decided_inside_the_tick_starts_one() {
        // Row 7: one failed route check and a recovery due.
        let mut clock = Clock::new();
        let mut t = tracker();
        clock.begin(&mut t, &no_route());
        let (now, at) = clock.next();
        let held = SteadyOutcome::Held {
            cause: UplinkHoldCause::NoRoute,
            skipped: DegradeReason::PcscfPoolExhausted,
            initiated: false,
            ike_sa_loss_cause: None,
        };
        let started = t.end_tick(&held, mark(), now, at);
        assert_eq!(
            started,
            Some(HoldStart {
                cause: UplinkHoldCause::NoRoute
            })
        );
        assert_eq!(t.state(), UplinkState::NoRoute);
        assert_eq!(
            t.hold.as_ref().unwrap().skipped,
            vec![DegradeReason::PcscfPoolExhausted]
        );
    }

    #[test]
    fn a_deferral_creates_a_pending_check_and_a_second_one_reuses_it() {
        let mut clock = Clock::new();
        let mut t = tracker();
        let (now, at) = clock.next();
        let first = SteadyOutcome::Deferred {
            reason: DegradeReason::PcscfPoolExhausted,
        };
        assert!(t.end_tick(&first, mark(), now, at).is_none());
        let kept = t.pending_check.unwrap();
        assert_eq!(kept.deferred, DegradeReason::PcscfPoolExhausted);
        let second = SteadyOutcome::Deferred {
            reason: DegradeReason::ChildSaStuck,
        };
        t.end_tick(&second, mark(), now, at);
        assert_eq!(t.pending_check.unwrap(), kept);
    }

    #[test]
    fn the_pending_check_is_offered_to_the_next_tick_with_its_answer() {
        let mut clock = Clock::new();
        let mut t = tracker();
        let (now, at) = clock.next();
        t.end_tick(
            &SteadyOutcome::Deferred {
                reason: DegradeReason::PcscfPoolExhausted,
            },
            mark(),
            now,
            at,
        );
        let unanswered = Observation {
            pending_life: Some(false),
            ..quiet()
        };
        let b = clock.begin(&mut t, &unanswered);
        assert_eq!(
            b.view.pending,
            Some(PendingView {
                reason: DegradeReason::PcscfPoolExhausted,
                life_since_mark: false,
                expired: true,
            })
        );
        let answered = Observation {
            pending_life: Some(true),
            ..quiet()
        };
        let b = clock.begin(&mut t, &answered);
        assert!(b.view.pending.unwrap().life_since_mark);
    }

    #[test]
    fn only_packets_counted_after_the_check_answer_it() {
        let mut clock = Clock::new();
        let mut t = tracker();
        let quiet_rx = |rx| Observation {
            rx_packets: Some(rx),
            pending_life: Some(false),
            ..quiet()
        };
        clock.begin(&mut t, &quiet_rx(100));
        // Traffic arrived since the previous tick, and then this tick sent the
        // check.
        clock.begin(&mut t, &quiet_rx(150));
        let (now, at) = clock.next();
        t.end_tick(
            &SteadyOutcome::Deferred {
                reason: DegradeReason::PcscfPoolExhausted,
            },
            mark(),
            now,
            at,
        );
        let b = clock.begin(&mut t, &quiet_rx(150));
        assert!(
            !b.view.pending.unwrap().life_since_mark,
            "the 50 packets predate the check"
        );
        let b = clock.begin(&mut t, &quiet_rx(151));
        assert!(b.view.pending.unwrap().life_since_mark);
    }

    #[test]
    fn the_pending_check_clears_when_the_recovery_runs_or_the_suspicion_goes() {
        for outcome in [
            SteadyOutcome::StillUp,
            SteadyOutcome::WaitingForNegotiation,
            SteadyOutcome::Recovered {
                reason: DegradeReason::PcscfPoolExhausted,
                ike_sa_loss_cause: None,
            },
        ] {
            let mut clock = Clock::new();
            let mut t = tracker();
            let (now, at) = clock.next();
            t.end_tick(
                &SteadyOutcome::Deferred {
                    reason: DegradeReason::PcscfPoolExhausted,
                },
                mark(),
                now,
                at,
            );
            t.end_tick(&outcome, mark(), now, at);
            assert!(t.pending_check.is_none(), "{outcome:?}");
        }
    }

    #[test]
    fn an_unanswered_check_that_holds_the_line_judges_life_from_the_check_onward() {
        let mut clock = Clock::new();
        let mut t = tracker();
        let (now, at) = clock.next();
        t.end_tick(
            &SteadyOutcome::Deferred {
                reason: DegradeReason::PcscfPoolExhausted,
            },
            mark(),
            now,
            at,
        );
        let (now, at) = clock.next();
        let held = SteadyOutcome::Held {
            cause: UplinkHoldCause::NoResponse,
            skipped: DegradeReason::PcscfPoolExhausted,
            initiated: false,
            ike_sa_loss_cause: None,
        };
        assert!(t.end_tick(&held, mark(), now, at).is_some());
        assert!(t.pending_check.is_none());
        assert_eq!(t.state(), UplinkState::NoResponse);
        // No retransmits were ever logged, yet the hold stands until the ePDG
        // is heard.
        let b = clock.begin(&mut t, &quiet_not_heard());
        assert_eq!(b.view.verdict, UnderlayVerdict::NoResponse);
    }

    fn quiet_not_heard() -> Observation {
        Observation {
            evidence: IkeEvidence::default(),
            ..silent(0)
        }
    }
}
