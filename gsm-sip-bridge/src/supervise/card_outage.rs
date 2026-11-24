//! What the supervisor knows about a card-reader line's SIM being out of reach,
//! and how it paces itself while it is (#233 follow-up, specs/163).
//!
//! A card-reader (`pcsc_reader`) line's SIM sits in a PC/SC reader, not in a
//! modem. When it cannot be reached (pulled, reset by another program, its
//! reader gone, pcscd down), nothing the supervisor does brings it back: there
//! is no `AT+CFUN` to cycle. Before this module the agent was restarted every
//! 5 s and the ePDG tunnel re-initiated every tick, each attempt failing on the
//! card, for as long as the card was gone.
//!
//! One [`CardOutage`] per card-reader line, shared by that line's agent thread
//! and steady-state loop behind a mutex, records whether the card is out and
//! paces both: agent restarts back off from 5 s to 120 s, new tunnel
//! initiations go at once, then every 60 s, then every 120 s, and a card check
//! runs at most every 5 s so the line comes back as soon as the card does.
//! Pure, with the clock passed in, like [`super::underlay`] and
//! [`super::sim_recovery`]. Modem and VoLTE lines never construct one.
//!
//! The mutex is never held across a subprocess or a sleep: callers take a
//! decision under it, drop it, act, and lock again to record.

use super::runner::CommandRunner;
use super::sim_recovery::NORMAL_RESTART_DELAY;
use std::io;
use std::process::Output;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How often the card is checked while it is out, whichever of the line's two
/// threads asks. The same as [`super::underlay::HELD_POLL_INTERVAL`], the
/// pace the steady loop already polls at around an uplink hold.
pub const CARD_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// The bound on one card check (`timeout` in its argv). A healthy read takes
/// about 100 ms; charon in the middle of EAP-AKA holds the card for about 1 s.
pub const CARD_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// The first gap between paced tunnel initiations once the first has gone.
/// About today's cadence: a 30 s tick plus a ~50 s initiate.
pub const CARD_TUNNEL_INITIAL_INTERVAL: Duration = Duration::from_secs(60);

/// The cap on both backoffs: the agent's own renewal ladder's cap.
pub const CARD_OUTAGE_MAX_BACKOFF: Duration = crate::ims::agent::RETRY_MAX_BACKOFF;

/// What one card check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardVerdict {
    /// The card answered.
    Present,
    /// pcscd cannot reach the card (`vowifi-plmn` exited
    /// [`crate::vowifi::plmn::EXIT_CARD_UNREACHABLE`]).
    Out,
    /// The check could not tell: it timed out, failed for another reason, or
    /// could not be run. Never starts or ends an outage.
    Unknown,
}

/// A change in whether the line's card is out, returned once per change so
/// the caller logs and reports each exactly once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardTransition {
    Began,
    Ended { held_for: Duration },
}

/// One card-reader line's card outage. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardOutage {
    /// When the outage began; `None` when the card is not known to be out.
    since: Option<Instant>,
    /// When a check last ran, from either thread.
    last_check: Option<Instant>,
    /// The next agent restart delay.
    agent_delay: Duration,
    /// The gap after the next paced tunnel initiation.
    tunnel_interval: Duration,
    /// When the next paced initiation may go; `None` means at once.
    tunnel_next_at: Option<Instant>,
}

impl Default for CardOutage {
    fn default() -> Self {
        Self {
            since: None,
            last_check: None,
            agent_delay: NORMAL_RESTART_DELAY,
            tunnel_interval: CARD_TUNNEL_INITIAL_INTERVAL,
            tunnel_next_at: None,
        }
    }
}

impl CardOutage {
    /// Whether the card is out right now, as far as the line knows.
    pub fn is_out(&self) -> bool {
        self.since.is_some()
    }

    /// Evidence that the card is out: starts an outage, if there is none.
    /// Every delay is at its first value then: the default, or reset by the
    /// end of the last outage.
    pub fn on_out(&mut self, now: Instant) -> Option<CardTransition> {
        if self.since.is_some() {
            return None;
        }
        self.since = Some(now);
        Some(CardTransition::Began)
    }

    /// A check found the card: ends the outage, if there is one.
    pub fn on_present(&mut self, now: Instant) -> Option<CardTransition> {
        self.end(now)
    }

    /// The line's agent registered, which it cannot do without its card:
    /// ends the outage, if there is one.
    pub fn on_registered(&mut self, now: Instant) -> Option<CardTransition> {
        self.end(now)
    }

    fn end(&mut self, now: Instant) -> Option<CardTransition> {
        let since = self.since.take()?;
        // Every way out resets every delay, so a later outage starts from the
        // first values again (learnings: latching state).
        self.reset_delays();
        Some(CardTransition::Ended {
            held_for: now.saturating_duration_since(since),
        })
    }

    fn reset_delays(&mut self) {
        self.agent_delay = NORMAL_RESTART_DELAY;
        self.tunnel_interval = CARD_TUNNEL_INITIAL_INTERVAL;
        self.tunnel_next_at = None;
    }

    /// Records what a check found. `Unknown` changes nothing but the time of
    /// the last check.
    pub fn record(&mut self, verdict: CardVerdict, now: Instant) -> Option<CardTransition> {
        self.last_check = Some(now);
        match verdict {
            CardVerdict::Present => self.on_present(now),
            CardVerdict::Out => self.on_out(now),
            CardVerdict::Unknown => None,
        }
    }

    /// Whether a check may run now: none has run within
    /// [`CARD_CHECK_INTERVAL`], from either thread.
    pub fn check_due(&self, now: Instant) -> bool {
        self.last_check
            .is_none_or(|at| now.saturating_duration_since(at) >= CARD_CHECK_INTERVAL)
    }

    /// [`Self::check_due`], and if so, claims the check, so the line's other
    /// thread does not run one too before this one records its verdict.
    pub fn claim_check(&mut self, now: Instant) -> bool {
        let due = self.check_due(now);
        if due {
            self.last_check = Some(now);
        }
        due
    }

    /// The delay before the next agent restart, which then doubles up to
    /// [`CARD_OUTAGE_MAX_BACKOFF`].
    pub fn next_agent_delay(&mut self) -> Duration {
        let delay = self.agent_delay;
        self.agent_delay = crate::ims::session::next_backoff(delay, CARD_OUTAGE_MAX_BACKOFF);
        delay
    }

    /// Whether a paced tunnel initiation may go now.
    pub fn tunnel_attempt_due(&self, now: Instant) -> bool {
        self.is_out() && self.tunnel_next_at.is_none_or(|at| now >= at)
    }

    /// Records a paced initiation made at `now`, and returns how long until
    /// the next one may go, which then doubles up to the cap.
    pub fn tunnel_attempted(&mut self, now: Instant) -> Duration {
        let gap = self.tunnel_interval;
        self.tunnel_next_at = Some(now + gap);
        self.tunnel_interval = crate::ims::session::next_backoff(gap, CARD_OUTAGE_MAX_BACKOFF);
        gap
    }
}

/// The supervisor's log line for a transition (contracts/operator-surface.md).
/// It names the line, never the card's IMSI.
pub fn transition_line(idx: u32, transition: &CardTransition) -> String {
    match transition {
        CardTransition::Began => format!(
            "[supervise] line {idx}: SIM card out of reach; agent restarts and tunnel attempts are slowed until it is back"
        ),
        CardTransition::Ended { held_for } => format!(
            "[supervise] line {idx}: SIM card reachable again after {}s; resuming",
            held_for.as_secs()
        ),
    }
}

/// Checks whether `imsi`'s card can be reached, by running
/// `timeout 5 <bin> vowifi-plmn --pcsc-imsi <imsi>`.
///
/// A subprocess, not an in-process PC/SC call on a thread: a card command
/// stuck inside the reader blocks its caller until it returns, and a thread
/// stuck that way cannot be stopped, while `timeout` kills the process. It is
/// the read the supervisor already makes at start-up, in one PC/SC
/// transaction that leaves the card as it found it (`LeaveCard`, #232), so a
/// check never resets the SIM charon or the agent may be using. Only the exit
/// status is read, never its text.
pub fn card_check(runner: &dyn CommandRunner, bin: &str, imsi: &str) -> CardVerdict {
    let timeout = CARD_CHECK_TIMEOUT.as_secs().to_string();
    verdict_for(&runner.run(&["timeout", &timeout, bin, "vowifi-plmn", "--pcsc-imsi", imsi]))
}

/// The verdict a card check's result gives (research D2).
fn verdict_for(result: &io::Result<Output>) -> CardVerdict {
    let Ok(out) = result else {
        return CardVerdict::Unknown;
    };
    match out.status.code() {
        Some(0) => CardVerdict::Present,
        Some(code) if code == i32::from(crate::vowifi::plmn::EXIT_CARD_UNREACHABLE) => {
            CardVerdict::Out
        }
        _ => CardVerdict::Unknown,
    }
}

/// Runs one card check if the outage is on and a check is due, and records it.
/// Called by both of the line's threads; the lock is not held while the check
/// runs.
pub fn poll_during_outage(
    outage: &Mutex<CardOutage>,
    clock: &dyn Fn() -> Instant,
    check: &dyn Fn() -> CardVerdict,
) -> Option<(CardVerdict, Option<CardTransition>)> {
    let claimed = {
        let mut o = outage.lock().unwrap();
        o.is_out() && o.claim_check(clock())
    };
    if !claimed {
        return None;
    }
    let verdict = check();
    let transition = outage.lock().unwrap().record(verdict, clock());
    Some((verdict, transition))
}

/// What the steady loop does with one tick's outcome on a card-reader line,
/// for its log and report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CardTickReport {
    /// The outage began or ended on this tick's check.
    pub transition: Option<CardTransition>,
    /// The tick made a paced initiation; the next may go after this long.
    pub next_try_in: Option<Duration>,
}

/// The steady loop's bookkeeping after a tick on a card-reader line:
/// records what the card gate's own check found (`gate_verdict`, `None` when
/// the tick did not check), then a paced initiation the tick made. In that
/// order, so an outage the check just found counts the initiation it allowed
/// as its first.
pub fn after_card_tick(
    outcome: &super::line_supervisor::SteadyOutcome,
    gate_verdict: Option<CardVerdict>,
    outage: &mut CardOutage,
    now: Instant,
) -> CardTickReport {
    let transition = gate_verdict.and_then(|v| outage.record(v, now));
    let next_try_in = match outcome {
        super::line_supervisor::SteadyOutcome::CardHeld {
            initiated: true, ..
        } => Some(outage.tunnel_attempted(now)),
        _ => None,
    };
    CardTickReport {
        transition,
        next_try_in,
    }
}

/// The steady loop's pause before its next tick: no longer than
/// [`CARD_CHECK_INTERVAL`] while the card is out, so its return is noticed
/// within seconds (research D10), and `pause` otherwise.
pub fn steady_pause(pause: Duration, card_out: bool) -> Duration {
    if card_out {
        pause.min(CARD_CHECK_INTERVAL)
    } else {
        pause
    }
}

/// The log line for a paced initiation (contracts/operator-surface.md).
pub fn paced_initiation_line(idx: u32, next_try_in: Duration) -> String {
    format!(
        "[supervise] line {idx}: SIM card still out of reach; trying the tunnel once (next try in {}s)",
        next_try_in.as_secs()
    )
}

/// How a [`wait_for_card_or`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitEnd {
    /// The card is back. `ended` is the transition this wait recorded, if it
    /// was the one that saw it (the other thread logs its own).
    CardBack { ended: Option<CardTransition> },
    /// The supervisor is shutting down.
    Shutdown,
    /// The whole delay passed with the card still out.
    Elapsed,
}

/// The agent thread's wait before restarting an agent that exited because its
/// card is out: `delay` in [`CARD_CHECK_INTERVAL`] steps, each starting with a
/// card check when one is due, returning early when the card is back or the
/// supervisor is shutting down (FR-006).
pub fn wait_for_card_or(
    runner: &dyn CommandRunner,
    outage: &Mutex<CardOutage>,
    delay: Duration,
    clock: &dyn Fn() -> Instant,
    check: &dyn Fn() -> CardVerdict,
    shutting_down: &dyn Fn() -> bool,
) -> WaitEnd {
    let mut waited = Duration::ZERO;
    while waited < delay {
        if shutting_down() {
            return WaitEnd::Shutdown;
        }
        match poll_during_outage(outage, clock, check) {
            Some((CardVerdict::Present, ended)) => return WaitEnd::CardBack { ended },
            Some(_) => {}
            // No check was due, or the outage is already over: the other
            // thread saw the card.
            None if !outage.lock().unwrap().is_out() => {
                return WaitEnd::CardBack { ended: None };
            }
            None => {}
        }
        let step = CARD_CHECK_INTERVAL.min(delay - waited);
        runner.sleep(step);
        waited += step;
    }
    WaitEnd::Elapsed
}

#[cfg(test)]
mod tests {
    use super::super::runner::MockCommandRunner;
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    const IMSI: &str = "001010000000001";
    const CHECK_KEY: &str = "timeout 5 gsm-sip-bridge vowifi-plmn --pcsc-imsi 001010000000001";

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn exited(code: i32) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn an_outage_begins_once_and_ends_once() {
        let t0 = Instant::now();
        let mut o = CardOutage::default();
        assert_eq!(o.on_out(t0), Some(CardTransition::Began));
        assert_eq!(o.on_out(t0 + secs(3)), None);
        assert!(o.is_out());
        assert_eq!(
            o.on_present(t0 + secs(42)),
            Some(CardTransition::Ended { held_for: secs(42) })
        );
        assert_eq!(o.on_present(t0 + secs(50)), None);
        assert!(!o.is_out());
    }

    #[test]
    fn unknown_never_starts_or_ends_an_outage() {
        let t0 = Instant::now();
        let mut o = CardOutage::default();
        assert_eq!(o.record(CardVerdict::Unknown, t0), None);
        assert!(!o.is_out());
        o.record(CardVerdict::Out, t0);
        assert_eq!(o.record(CardVerdict::Unknown, t0 + secs(5)), None);
        assert!(o.is_out());
    }

    #[test]
    fn a_check_is_due_at_most_every_five_seconds() {
        let t0 = Instant::now();
        let mut o = CardOutage::default();
        assert!(o.check_due(t0));
        assert!(o.claim_check(t0));
        assert!(!o.claim_check(t0 + secs(4)), "claimed by the first");
        assert!(!o.check_due(t0 + Duration::from_millis(4999)));
        assert!(o.claim_check(t0 + secs(5)));
        // A recorded verdict counts as a check too.
        o.record(CardVerdict::Unknown, t0 + secs(7));
        assert!(!o.check_due(t0 + secs(11)));
        assert!(o.check_due(t0 + secs(12)));
    }

    #[test]
    fn agent_delays_double_from_five_seconds_to_the_cap() {
        let mut o = CardOutage::default();
        o.on_out(Instant::now());
        let delays: Vec<u64> = (0..7).map(|_| o.next_agent_delay().as_secs()).collect();
        assert_eq!(delays, [5, 10, 20, 40, 80, 120, 120]);
        assert_eq!(CARD_OUTAGE_MAX_BACKOFF, secs(120));
    }

    #[test]
    fn tunnel_attempts_go_at_once_then_every_60_then_120_seconds() {
        let t0 = Instant::now();
        let mut o = CardOutage::default();
        assert!(!o.tunnel_attempt_due(t0), "no outage, no pacing");
        o.on_out(t0);
        assert!(o.tunnel_attempt_due(t0), "the first goes at once");
        assert_eq!(o.tunnel_attempted(t0), secs(60));
        assert!(!o.tunnel_attempt_due(t0 + secs(59)));
        assert!(o.tunnel_attempt_due(t0 + secs(60)));
        assert_eq!(o.tunnel_attempted(t0 + secs(60)), secs(120));
        assert!(!o.tunnel_attempt_due(t0 + secs(179)));
        assert!(o.tunnel_attempt_due(t0 + secs(180)));
        assert_eq!(o.tunnel_attempted(t0 + secs(180)), secs(120));
        assert_eq!(o.tunnel_attempted(t0 + secs(300)), secs(120));
    }

    #[test]
    fn every_way_out_resets_every_delay() {
        type End = fn(&mut CardOutage, Instant) -> Option<CardTransition>;
        let ways: [(&str, End); 2] = [
            ("present", CardOutage::on_present),
            ("registered", CardOutage::on_registered),
        ];
        for (name, end) in ways {
            let t0 = Instant::now();
            let mut o = CardOutage::default();
            o.on_out(t0);
            o.next_agent_delay();
            o.next_agent_delay();
            o.tunnel_attempted(t0);
            o.tunnel_attempted(t0 + secs(60));
            assert!(matches!(
                end(&mut o, t0 + secs(200)),
                Some(CardTransition::Ended { .. })
            ));
            assert!(!o.tunnel_attempt_due(t0 + secs(200)), "{name}: no outage");
            let t1 = t0 + secs(300);
            o.on_out(t1);
            assert_eq!(o.next_agent_delay(), secs(5), "{name}");
            assert!(o.tunnel_attempt_due(t1), "{name}: at once again");
            assert_eq!(o.tunnel_attempted(t1), secs(60), "{name}");
        }
    }

    /// Mock justification: CI has no reader, no pcscd and no `vowifi-plmn`
    /// to run; the real exit mapping is pinned by `vowifi::plmn`'s own test.
    #[test]
    fn card_check_maps_exit_status() {
        let cases = [
            (0, CardVerdict::Present),
            (
                i32::from(crate::vowifi::plmn::EXIT_CARD_UNREACHABLE),
                CardVerdict::Out,
            ),
            (1, CardVerdict::Unknown),
            (124, CardVerdict::Unknown),
        ];
        for (code, want) in cases {
            let mock = MockCommandRunner::new();
            mock.set_run_output(CHECK_KEY, exited(code));
            assert_eq!(card_check(&mock, "gsm-sip-bridge", IMSI), want, "{code}");
        }
        let spawn_error: io::Result<Output> = Err(io::Error::from(io::ErrorKind::NotFound));
        assert_eq!(verdict_for(&spawn_error), CardVerdict::Unknown);
        // Killed by a signal: no exit code at all.
        let killed = Output {
            status: ExitStatus::from_raw(9),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert_eq!(verdict_for(&Ok(killed)), CardVerdict::Unknown);
    }

    #[test]
    fn card_check_is_bounded_and_names_the_imsi() {
        let mock = MockCommandRunner::new();
        card_check(&mock, "gsm-sip-bridge", IMSI);
        assert_eq!(
            *mock.run_calls.lock().unwrap(),
            vec![vec![
                "timeout".to_string(),
                "5".into(),
                "gsm-sip-bridge".into(),
                "vowifi-plmn".into(),
                "--pcsc-imsi".into(),
                IMSI.into(),
            ]]
        );
    }

    /// A clock that advances with the mock's recorded sleeps, from `base`.
    fn sleep_clock(mock: &MockCommandRunner, base: Instant) -> impl Fn() -> Instant + '_ {
        move || base + mock.sleeps.lock().unwrap().iter().sum::<Duration>()
    }

    /// A check answering `verdicts` in turn, then `Out`.
    fn scripted_check(verdicts: &[CardVerdict]) -> impl Fn() -> CardVerdict + '_ {
        let next = std::cell::Cell::new(0);
        move || {
            let i = next.get();
            next.set(i + 1);
            verdicts.get(i).copied().unwrap_or(CardVerdict::Out)
        }
    }

    fn out_since(t0: Instant) -> Mutex<CardOutage> {
        let mut o = CardOutage::default();
        o.on_out(t0);
        Mutex::new(o)
    }

    #[test]
    fn the_wait_returns_early_when_the_card_is_back() {
        let t0 = Instant::now();
        let mock = MockCommandRunner::new();
        let outage = out_since(t0);
        let verdicts = [CardVerdict::Out, CardVerdict::Out, CardVerdict::Present];
        let end = wait_for_card_or(
            &mock,
            &outage,
            secs(120),
            &sleep_clock(&mock, t0),
            &scripted_check(&verdicts),
            &|| false,
        );
        assert_eq!(
            end,
            WaitEnd::CardBack {
                ended: Some(CardTransition::Ended { held_for: secs(10) })
            }
        );
        assert_eq!(*mock.sleeps.lock().unwrap(), vec![secs(5), secs(5)]);
        assert!(!outage.lock().unwrap().is_out());
    }

    #[test]
    fn the_wait_lasts_the_whole_delay_while_the_card_stays_out() {
        let t0 = Instant::now();
        let mock = MockCommandRunner::new();
        let outage = out_since(t0);
        let end = wait_for_card_or(
            &mock,
            &outage,
            secs(20),
            &sleep_clock(&mock, t0),
            &|| CardVerdict::Out,
            &|| false,
        );
        assert_eq!(end, WaitEnd::Elapsed);
        assert_eq!(*mock.sleeps.lock().unwrap(), vec![secs(5); 4]);
        assert!(outage.lock().unwrap().is_out());
    }

    #[test]
    fn the_wait_stops_at_once_on_shutdown() {
        let t0 = Instant::now();
        let mock = MockCommandRunner::new();
        let outage = out_since(t0);
        let checked = std::cell::Cell::new(false);
        let end = wait_for_card_or(
            &mock,
            &outage,
            secs(120),
            &sleep_clock(&mock, t0),
            &|| {
                checked.set(true);
                CardVerdict::Out
            },
            &|| true,
        );
        assert_eq!(end, WaitEnd::Shutdown);
        assert!(mock.sleeps.lock().unwrap().is_empty());
        assert!(!checked.get());
    }

    #[test]
    fn the_wait_runs_at_most_one_check_per_five_seconds_when_shared() {
        let t0 = Instant::now();
        let mock = MockCommandRunner::new();
        let outage = out_since(t0);
        // The steady loop checked 2 s ago and found it still out.
        outage
            .lock()
            .unwrap()
            .record(CardVerdict::Out, t0 - secs(2));
        let checks = std::cell::Cell::new(0);
        wait_for_card_or(
            &mock,
            &outage,
            secs(10),
            &sleep_clock(&mock, t0),
            &|| {
                checks.set(checks.get() + 1);
                CardVerdict::Out
            },
            &|| false,
        );
        // Not at t0 (2 s after the other check), then at t0+5.
        assert_eq!(checks.get(), 1);
        assert_eq!(*mock.sleeps.lock().unwrap(), vec![secs(5), secs(5)]);
    }

    #[test]
    fn the_wait_ends_when_the_other_thread_saw_the_card() {
        let t0 = Instant::now();
        let mock = MockCommandRunner::new();
        let outage = Mutex::new(CardOutage::default());
        // The steady loop's check found it 1 s ago and ended the outage.
        outage
            .lock()
            .unwrap()
            .record(CardVerdict::Present, t0 - secs(1));
        let end = wait_for_card_or(
            &mock,
            &outage,
            secs(40),
            &sleep_clock(&mock, t0),
            &|| panic!("no check is due"),
            &|| false,
        );
        assert_eq!(end, WaitEnd::CardBack { ended: None });
        assert!(mock.sleeps.lock().unwrap().is_empty());
    }

    use crate::supervise::line_supervisor::{DegradeReason, SteadyOutcome};

    fn card_held(initiated: bool) -> SteadyOutcome {
        SteadyOutcome::CardHeld {
            skipped: DegradeReason::IkeSaLost,
            initiated,
        }
    }

    #[test]
    fn a_paced_initiation_is_recorded_after_the_tick() {
        let t0 = Instant::now();
        let mut o = out_since(t0).into_inner().unwrap();
        let report = after_card_tick(&card_held(true), None, &mut o, t0);
        assert_eq!(
            report,
            CardTickReport {
                transition: None,
                next_try_in: Some(secs(60)),
            }
        );
        assert!(!o.tunnel_attempt_due(t0 + secs(59)));
        // A held tick with nothing initiated changes nothing.
        let before = o.clone();
        let report = after_card_tick(&card_held(false), None, &mut o, t0 + secs(5));
        assert_eq!(report, CardTickReport::default());
        assert_eq!(o, before);
    }

    #[test]
    fn the_gates_own_out_begins_the_outage_and_counts_its_first_attempt() {
        let t0 = Instant::now();
        let mut o = CardOutage::default();
        let report = after_card_tick(&card_held(true), Some(CardVerdict::Out), &mut o, t0);
        assert_eq!(report.transition, Some(CardTransition::Began));
        assert_eq!(report.next_try_in, Some(secs(60)));
        assert!(o.is_out());
        assert!(!o.check_due(t0), "the gate's check counts as one");
        // A present or unknown check on a tick that ran the recovery.
        let recovered = SteadyOutcome::Recovered {
            reason: DegradeReason::IkeSaLost,
            ike_sa_loss_cause: None,
        };
        let mut fine = CardOutage::default();
        let report = after_card_tick(&recovered, Some(CardVerdict::Unknown), &mut fine, t0);
        assert_eq!(report, CardTickReport::default());
        assert!(!fine.is_out());
        let report = after_card_tick(&recovered, Some(CardVerdict::Present), &mut o, t0 + secs(9));
        assert_eq!(
            report.transition,
            Some(CardTransition::Ended { held_for: secs(9) })
        );
    }

    #[test]
    fn the_held_initiation_restart_is_left_to_the_tick() {
        // `HeldInitiationEstablished` restarts the agent through the loop's
        // existing handling; the card's bookkeeping neither blocks nor
        // repeats it, and the outage stays until a check or a registration.
        let t0 = Instant::now();
        let mut o = out_since(t0).into_inner().unwrap();
        let established = SteadyOutcome::HeldInitiationEstablished {
            new_pcscf: "10.0.0.11".into(),
        };
        assert!(crate::supervise::line_supervisor::outcome_restarts_agent(
            &established
        ));
        let report = after_card_tick(&established, None, &mut o, t0 + secs(30));
        assert_eq!(report, CardTickReport::default());
        assert!(o.is_out());
    }

    #[test]
    fn the_loop_polls_every_five_seconds_while_the_card_is_out() {
        let steady = crate::supervise::line_supervisor::STEADY_STATE_POLL_INTERVAL;
        assert_eq!(steady_pause(steady, true), CARD_CHECK_INTERVAL);
        assert_eq!(
            CARD_CHECK_INTERVAL,
            crate::supervise::underlay::HELD_POLL_INTERVAL
        );
        assert_eq!(steady_pause(steady, false), steady);
        // An uplink hold's quicker pause is kept either way.
        let held = crate::supervise::underlay::HELD_POLL_INTERVAL;
        assert_eq!(steady_pause(held, true), held);
        assert_eq!(steady_pause(held, false), held);
    }

    #[test]
    fn the_paced_initiation_line_carries_its_delay() {
        assert_eq!(
            paced_initiation_line(1, secs(120)),
            "[supervise] line 1: SIM card still out of reach; trying the tunnel once (next try in 120s)"
        );
    }

    /// Whether `line` carries a run of 10 or more digits (an IMSI, ICCID or
    /// MSISDN would).
    fn has_a_subscriber_id(line: &str) -> bool {
        line.split(|c: char| !c.is_ascii_digit())
            .any(|run| run.len() >= 10)
    }

    #[test]
    fn a_ten_minute_outage_logs_one_began_and_one_ended_line() {
        // The agent thread over a card outage of 10 minutes (SC-001, SC-005):
        // every run exits on the card, is paced and waited out with checks,
        // with one steady tick on a missing IKE_SA between restarts.
        let base = Instant::now();
        let mock = MockCommandRunner::new();
        let clock = sleep_clock(&mock, base);
        let back_at = base + secs(600);
        let check = || {
            if clock() >= back_at {
                CardVerdict::Present
            } else {
                CardVerdict::Out
            }
        };
        let outage = Mutex::new(CardOutage::default());
        let mut lines = Vec::new();
        let mut restarts = 0;
        loop {
            let now = clock();
            assert!(now < base + secs(900), "the outage never ended");
            let (began, delay) = {
                let mut o = outage.lock().unwrap();
                (o.on_out(now), o.next_agent_delay())
            };
            lines.extend(began.map(|t| transition_line(0, &t)));
            lines.push(format!(
                "[supervise] line 0: vowifi-ims-agent exited (SIM card out of reach); restarting in {}s",
                delay.as_secs()
            ));
            let tick = {
                let mut o = outage.lock().unwrap();
                let due = o.tunnel_attempt_due(now);
                let outcome = SteadyOutcome::CardHeld {
                    skipped: DegradeReason::IkeSaLost,
                    initiated: due,
                };
                after_card_tick(&outcome, None, &mut o, now)
            };
            lines.extend(tick.transition.map(|t| transition_line(0, &t)));
            lines.extend(tick.next_try_in.map(|n| paced_initiation_line(0, n)));
            match wait_for_card_or(&mock, &outage, delay, &clock, &check, &|| false) {
                WaitEnd::CardBack { ended } => {
                    lines.extend(ended.map(|t| transition_line(0, &t)));
                    break;
                }
                WaitEnd::Elapsed => restarts += 1,
                WaitEnd::Shutdown => unreachable!(),
            }
        }
        let count = |needle: &str| lines.iter().filter(|l| l.contains(needle)).count();
        assert_eq!(
            count("SIM card out of reach; agent restarts"),
            1,
            "{lines:#?}"
        );
        assert_eq!(count("SIM card reachable again after"), 1, "{lines:#?}");
        assert!(
            lines.last().unwrap().contains("reachable again after 600s"),
            "{lines:#?}"
        );
        assert!(restarts <= 15, "SC-001: {restarts} restarts");
        // Each paced line carries its delay, and none names the card.
        assert!(count("restarting in ") > 0);
        assert!(lines.iter().all(|l| !has_a_subscriber_id(l)), "{lines:#?}");
        // Back within one check of the card's return (FR-006).
        assert!(clock() <= back_at + CARD_CHECK_INTERVAL);
    }

    #[test]
    fn a_ten_minute_outage_makes_at_most_six_tunnel_attempts() {
        // The steady loop ticking every 5 s over 10 minutes with no IKE_SA
        // (SC-002).
        let t0 = Instant::now();
        let mut o = CardOutage::default();
        o.on_out(t0);
        let mut attempts = Vec::new();
        for step in 0..120 {
            let now = t0 + secs(5 * step);
            let due = o.tunnel_attempt_due(now);
            let outcome = SteadyOutcome::CardHeld {
                skipped: DegradeReason::IkeSaLost,
                initiated: due,
            };
            if after_card_tick(&outcome, None, &mut o, now)
                .next_try_in
                .is_some()
            {
                attempts.push(5 * step);
            }
        }
        assert_eq!(attempts, [0, 60, 180, 300, 420, 540]);
    }

    #[test]
    fn transition_lines_name_the_line_and_never_the_card() {
        assert_eq!(
            transition_line(2, &CardTransition::Began),
            "[supervise] line 2: SIM card out of reach; agent restarts and tunnel attempts are slowed until it is back"
        );
        assert_eq!(
            transition_line(
                0,
                &CardTransition::Ended {
                    held_for: secs(312)
                }
            ),
            "[supervise] line 0: SIM card reachable again after 312s; resuming"
        );
    }
}
