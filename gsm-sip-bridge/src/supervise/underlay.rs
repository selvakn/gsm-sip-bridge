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

use super::engines::LogMark;
use super::line_supervisor::{
    DegradeReason, IkeEvidence, PendingView, SteadyOutcome, UnderlayVerdict, UnderlayView,
};
use crate::control::protocol::{HoldEndedBy, UplinkHoldCause, UplinkState};
use std::time::{Duration, Instant, SystemTime};

/// A failed `ip route get` must be repeated this much later to count
/// (specs/100 FR-002): two ticks at the 30s cadence, and never two reads of one
/// transient.
pub const ROUTE_DEBOUNCE: Duration = Duration::from_secs(10);

/// The longest a line stays held (FR-012). A guard that never gave up would
/// turn a wrong verdict into an outage with no end.
pub const MAX_HOLD: Duration = Duration::from_secs(15 * 60);

/// How often the loop ticks while a hold is on. The ordinary tick is 30s;
/// during a hold nothing is counted in ticks (a held recovery resets its
/// strikes), so ticking faster only makes the end of the outage visible sooner.
pub const HELD_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// The pause before the next tick: [`HELD_POLL_INTERVAL`] while a hold is on,
/// the ordinary steady-state interval otherwise.
pub fn poll_interval(held: bool) -> Duration {
    if held {
        HELD_POLL_INTERVAL
    } else {
        super::line_supervisor::STEADY_STATE_POLL_INTERVAL
    }
}

/// Unanswered retransmits of an outstanding request that establish "no
/// response" on their own (research R4).
pub const NO_RESPONSE_RETRANSMITS: u32 = 2;

/// One line's hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    pub cause: UplinkHoldCause,
    pub started: Instant,
    pub started_at: SystemTime,
    /// The recoveries held back, deduplicated, for the end-of-hold line.
    pub skipped: Vec<DegradeReason>,
}

/// A liveness check in flight (FR-004).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingCheck {
    /// Taken just before the check was sent; anything the ePDG says after it
    /// answers the check.
    pub mark: LogMark,
    pub deferred: DegradeReason,
}

/// A hold that began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldStart {
    pub cause: UplinkHoldCause,
}

/// A hold that ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldEnd {
    pub cause: UplinkHoldCause,
    pub held: Duration,
    pub ended_by: HoldEndedBy,
    pub skipped: Vec<DegradeReason>,
}

/// What the loop saw this tick, before running it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Observation {
    /// `ip route get <ePDG>`.
    pub route_ok: bool,
    /// The log since [`UnderlayTracker::evidence_mark`].
    pub evidence: IkeEvidence,
    /// A parsed message since the previous tick.
    pub ike_life_recent: bool,
    /// The tunnel interface's received-packet counter.
    pub rx_packets: Option<u64>,
    /// Whether the ePDG said anything since the pending check was sent; `None`
    /// when there is no pending check.
    pub pending_life: Option<bool>,
    /// The log's current position.
    pub mark: LogMark,
}

/// What [`UnderlayTracker::begin_tick`] decided.
#[derive(Debug, Clone)]
pub struct Begin {
    pub view: UnderlayView,
    pub ended: Option<HoldEnd>,
    pub started: Option<HoldStart>,
}

#[derive(Debug, Clone)]
pub struct UnderlayTracker {
    verdict: UnderlayVerdict,
    route_failures: u32,
    first_route_failure: Option<Instant>,
    last_rx: Option<u64>,
    evidence_mark: LogMark,
    tick_mark: LogMark,
    pub hold: Option<Hold>,
    pub pending_check: Option<PendingCheck>,
}

impl UnderlayTracker {
    pub fn new(mark: LogMark) -> Self {
        Self {
            verdict: UnderlayVerdict::Up,
            route_failures: 0,
            first_route_failure: None,
            last_rx: None,
            evidence_mark: mark,
            tick_mark: mark,
            hold: None,
            pending_check: None,
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
        self.tick_mark = obs.mark;

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

        // The ePDG said something, or the tunnel decrypted something: alive.
        let rx_grew = matches!(
            (self.last_rx, obs.rx_packets),
            (Some(before), Some(after)) if after > before
        );
        if obs.rx_packets.is_some() {
            self.last_rx = obs.rx_packets;
        }
        let heard =
            (obs.evidence.parsed_from_peer && obs.evidence.unanswered_retransmits == 0) || rx_grew;

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
        } else if obs.evidence.unanswered_retransmits >= NO_RESPONSE_RETRANSMITS && !rx_grew {
            UnderlayVerdict::NoResponse
        } else {
            UnderlayVerdict::Up
        };

        // Evidence is read from a mark that moves on whenever nothing is
        // outstanding, so the log slice stays short, and stays put while
        // retransmits are being counted.
        let advance = match (previous, verdict) {
            // Heard from, or the route came back and the slate is wiped.
            (UnderlayVerdict::NoResponse | UnderlayVerdict::NoRoute, UnderlayVerdict::Up) => true,
            (_, UnderlayVerdict::Up) => obs.evidence.unanswered_retransmits == 0,
            _ => false,
        };
        if advance {
            self.evidence_mark = obs.mark;
        }

        // The bound (FR-012): one ordinary pass, whatever the verdict says.
        let mut ended = None;
        let mut started = None;
        let mut unheld_pass = false;
        if let Some(hold) = &self.hold {
            if now.saturating_duration_since(hold.started) >= MAX_HOLD {
                ended = self.end_hold(HoldEndedBy::MaxHold, now);
                verdict = UnderlayVerdict::Up;
                self.evidence_mark = obs.mark;
                unheld_pass = true;
            }
        }

        if !unheld_pass {
            let cause = verdict.hold_cause();
            let current = self.hold.as_ref().map(|h| h.cause);
            if current != cause {
                if current.is_some() {
                    ended = self.end_hold(HoldEndedBy::Restored, now);
                }
                if let Some(cause) = cause {
                    self.start_hold(cause, now, now_at);
                    started = Some(HoldStart { cause });
                }
            }
        }
        self.verdict = verdict;
        if started.is_some() {
            // The suspicion the check was for is now a verdict.
            self.pending_check = None;
        }

        // A no-route hold that ended because the route came back: whatever
        // charon was retrying across the dead path should be retried now.
        let restored = ended.as_ref().is_some_and(|e| {
            e.ended_by == HoldEndedBy::Restored && e.cause == UplinkHoldCause::NoRoute
        });
        let pending = self.pending_check.map(|p| PendingView {
            reason: p.deferred,
            // The tunnel counter growing since the previous tick covers the
            // packets that arrived after the check went out, as well as the
            // log lines.
            life_since_mark: obs.pending_life.unwrap_or(false) || rx_grew,
        });
        Begin {
            view: UnderlayView {
                enabled: true,
                verdict,
                route_ok: obs.route_ok || unheld_pass,
                pending: pending.filter(|_| !unheld_pass),
                ike_life_recent: obs.ike_life_recent || unheld_pass,
                restored,
            },
            ended,
            started,
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
                self.pending_check.get_or_insert(PendingCheck {
                    mark: mark_before_tick,
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
    /// counted.
    pub fn end_for_shutdown(&mut self, now: Instant) -> Option<HoldEnd> {
        self.end_hold(HoldEndedBy::Shutdown, now)
    }

    fn start_hold(&mut self, cause: UplinkHoldCause, now: Instant, now_at: SystemTime) {
        self.hold = Some(Hold {
            cause,
            started: now,
            started_at: now_at,
            skipped: Vec::new(),
        });
    }

    fn end_hold(&mut self, ended_by: HoldEndedBy, now: Instant) -> Option<HoldEnd> {
        let hold = self.hold.take()?;
        Some(HoldEnd {
            cause: hold.cause,
            held: now.saturating_duration_since(hold.started),
            ended_by,
            skipped: hold.skipped,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = Duration::from_secs(30);

    fn mark() -> LogMark {
        LogMark::default()
    }

    fn quiet() -> Observation {
        Observation {
            route_ok: true,
            evidence: IkeEvidence::default(),
            ike_life_recent: true,
            rx_packets: Some(100),
            pending_life: None,
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
    fn a_held_line_is_polled_faster_than_an_ordinary_one() {
        assert_eq!(poll_interval(false), Duration::from_secs(30));
        assert_eq!(poll_interval(true), Duration::from_secs(5));
    }

    #[test]
    fn a_quiet_healthy_line_is_up_with_no_hold() {
        let mut clock = Clock::new();
        let mut t = UnderlayTracker::new(mark());
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
        let mut t = UnderlayTracker::new(mark());
        let b = clock.begin(&mut t, &no_route());
        assert_eq!(b.view.verdict, UnderlayVerdict::Up);
        assert!(!b.view.route_ok, "but the tick is told, for row 7");
        assert!(b.started.is_none());
    }

    #[test]
    fn two_failed_route_checks_ten_seconds_apart_hold_the_line() {
        let mut clock = Clock::new();
        let mut t = UnderlayTracker::new(mark());
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
        let mut t = UnderlayTracker::new(mark());
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
        let mut t = UnderlayTracker::new(mark());
        clock.begin(&mut t, &no_route());
        clock.begin(&mut t, &quiet());
        let b = clock.begin(&mut t, &no_route());
        assert_eq!(b.view.verdict, UnderlayVerdict::Up);
    }

    #[test]
    fn the_hold_ends_as_soon_as_the_route_is_back() {
        let mut clock = Clock::new();
        let mut t = UnderlayTracker::new(mark());
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
        let mut t = UnderlayTracker::new(mark());
        clock.begin(&mut t, &no_route());
        clock.begin(&mut t, &no_route());
        let b = clock.begin(&mut t, &quiet());
        assert!(b.view.restored);
        assert!(!clock.begin(&mut t, &quiet()).view.restored);
        // A no-response hold ending means the ePDG was heard from: not flagged.
        let mut clock = Clock::new();
        let mut t = UnderlayTracker::new(mark());
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
        let mut t = UnderlayTracker::new(mark());
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
        let mut t = UnderlayTracker::new(mark());
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
        let mut t = UnderlayTracker::new(mark());
        clock.begin(&mut t, &quiet());
        let mut obs = silent(3);
        obs.rx_packets = Some(101);
        let b = clock.begin(&mut t, &obs);
        assert_eq!(b.view.verdict, UnderlayVerdict::Up);
    }

    #[test]
    fn no_response_stays_until_the_epdg_is_heard_from() {
        let mut clock = Clock::new();
        let mut t = UnderlayTracker::new(mark());
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
        let mut t = UnderlayTracker::new(mark());
        clock.begin(&mut t, &silent(2));
        assert_eq!(t.state(), UplinkState::NoResponse);
        let both = Observation {
            route_ok: false,
            ..silent(4)
        };
        clock.begin(&mut t, &both);
        let b = clock.begin(&mut t, &both);
        assert_eq!(b.view.verdict, UnderlayVerdict::NoRoute);
        // The cause flipped: the hold ended and a new one started.
        assert_eq!(b.ended.unwrap().cause, UplinkHoldCause::NoResponse);
        assert_eq!(b.started.unwrap().cause, UplinkHoldCause::NoRoute);
    }

    #[test]
    fn retransmits_from_the_outage_do_not_hold_a_line_whose_route_just_returned() {
        let mut clock = Clock::new();
        let mut t = UnderlayTracker::new(mark());
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
        let mut t = UnderlayTracker::new(mark());
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
        let mut t = UnderlayTracker::new(mark());
        clock.begin(&mut t, &no_route());
        clock.begin(&mut t, &no_route());
        let end = t.end_for_shutdown(clock.t0 + TICK * 5).unwrap();
        assert_eq!(end.ended_by, HoldEndedBy::Shutdown);
        assert_eq!(end.held, TICK * 4);
        assert!(t.end_for_shutdown(clock.t0 + TICK * 6).is_none());
    }

    #[test]
    fn held_recoveries_are_recorded_once_each() {
        let mut clock = Clock::new();
        let mut t = UnderlayTracker::new(mark());
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
        let mut t = UnderlayTracker::new(mark());
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
        let mut t = UnderlayTracker::new(mark());
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
        let mut t = UnderlayTracker::new(mark());
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
                life_since_mark: false
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
            let mut t = UnderlayTracker::new(mark());
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
        let mut t = UnderlayTracker::new(mark());
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
