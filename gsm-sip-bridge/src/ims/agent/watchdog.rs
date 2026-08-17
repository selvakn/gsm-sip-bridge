//! Progress tracking and stall detection for a line's long-running activities
//! (specs/039-at-stall-watchdog).
//!
//! # Why this exists
//!
//! On 2026-08-16 a line was unreachable for 2h45m because a scheduled
//! re-registration issued an `AT+CSIM` and blocked forever in `read(2)` on the
//! modem's serial port — kernel stack `wait_woken <- n_tty_read <- tty_read`.
//! The dispatch loop *is* the thread that answers calls, so the line went deaf
//! at the same moment, the registration lapsed, and the carrier told callers
//! the phone was switched off.
//!
//! Nothing noticed. Every in-process health signal is produced by a *different*
//! thread than the dispatch loop — the metrics heartbeat, the status listener —
//! so they all kept cheerfully reporting the last known good state. The
//! supervisor only restarts an agent whose process has *exited*, and this one
//! was alive, just permanently parked in a syscall.
//!
//! This module is the thing that notices. An activity publishes which [`Phase`]
//! it is in and when it entered it; a sampling thread compares that against the
//! phase's budget and, when an activity has demonstrably stopped moving,
//! terminates the process so the supervisor restarts the line.
//!
//! # Why exiting is the recovery
//!
//! A thread blocked in `read(2)` cannot be cancelled in safe Rust, and it holds
//! the serial port (and its `flock`) for as long as it lives. Nothing short of
//! process death releases it. Agents are one process per line, so the blast
//! radius of that exit is exactly one line, and `supervise::orchestrate`
//! restarts it within ~5s.
//!
//! # Why budgets are derived rather than configured
//!
//! The legitimate durations here span two orders of magnitude — an idle poll is
//! 1s, a full re-registration is minutes — so a single global threshold would
//! be either useless or dangerous. Each budget is instead computed from the
//! timeouts of the operations its phase actually performs, and
//! [`tests::budgets_exceed_their_derived_worst_case`] recomputes that
//! derivation from the real constants. A future timeout bump therefore fails
//! the build rather than silently turning this module into a false-restart
//! generator.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, Instant};

/// How often the watchdog samples progress. Two consecutive over-budget
/// samples are required to act, so this is also the confirmation delay.
pub(crate) const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// How long recovery may be held off while a call is in progress (FR-029).
///
/// Deferral exists because a stalled *control* loop often leaves a call's audio
/// untouched — media relaying runs on its own threads — so restarting would
/// drop a call that was otherwise fine. But a stalled loop also cannot observe
/// the call ending, so an unbounded deferral would simply never recover. This
/// ceiling is what makes deferring safe rather than a second way to hang.
pub(crate) const DEFER_CEILING: Duration = Duration::from_secs(600);

/// Exit code used when the watchdog terminates a stalled agent. Distinct from
/// a plain failure so a human reading `docker logs`/`ps` can tell the two
/// apart; the supervisor itself classifies on the log marker, exactly as it
/// already does for SIM failures.
pub(crate) const EXIT_WATCHDOG_STALL: i32 = 70;

/// The marker `supervise::sim_recovery::has_at_stall` greps for. Changing this
/// string breaks that classification, so both sides are pinned by tests.
pub(crate) const STALL_MARKER: &str = "watchdog: the dispatch loop has made no progress";

/// Logged when a confirmed stall is held back for a call in progress.
pub(crate) const DEFER_MARKER: &str = "watchdog: recovery deferred while a call is in progress";

/// What a monitored activity is currently doing.
///
/// Backed by a `u8` so it can live in an atomic: the watchdog has to be able to
/// read this *while the owning thread is blocked in a syscall*, which rules out
/// anything that could require the owner to cooperate — a `Mutex` held across
/// the stall would deadlock the watchdog itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Actively polling for work, and expected to re-enter this phase every
    /// poll interval. Over-budget here means the loop has stopped iterating.
    Idle,
    /// Deliberately asleep until the next scheduled pass, for however long that
    /// is. **Never a stall.**
    ///
    /// This distinction is not academic. The SMS sweep rests between passes for
    /// `MODEM_SWEEP_INTERVAL`, which is longer than `Idle`'s budget -- so while
    /// it rested in `Idle`, the watchdog confirmed a stall and killed the agent
    /// every ~36 seconds. Caught on the live line within a minute of deploying.
    /// A phase an activity sits in *by design* cannot carry a deadline.
    Dormant,
    /// First registration, including the PLMN derive that opens the modem.
    Startup,
    /// Gm signaling liveness probe.
    GmProbe,
    /// Re-registration: modem open, SIM APDUs, REGISTER round trips, SA install.
    Renewal,
    /// Answering and bridging an inbound call.
    InboundCall,
    /// Placing an outbound leg.
    Origination,
    /// Sweeping the modem's own message storage.
    SmsSweep,
}

impl Phase {
    /// Explicit round-trip rather than `as`/`transmute`, so the atomic
    /// representation is total and no cast lint applies.
    const fn as_u8(self) -> u8 {
        match self {
            Phase::Idle => 0,
            Phase::Dormant => 7,
            Phase::Startup => 1,
            Phase::GmProbe => 2,
            Phase::Renewal => 3,
            Phase::InboundCall => 4,
            Phase::Origination => 5,
            Phase::SmsSweep => 6,
        }
    }

    fn from_u8(v: u8) -> Phase {
        match v {
            1 => Phase::Startup,
            2 => Phase::GmProbe,
            3 => Phase::Renewal,
            4 => Phase::InboundCall,
            5 => Phase::Origination,
            6 => Phase::SmsSweep,
            7 => Phase::Dormant,
            // Includes 0. An unknown discriminant can only arise from a bug,
            // and `Idle` is the safe reading: it never trips the watchdog.
            _ => Phase::Idle,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Dormant => "dormant",
            Phase::Startup => "startup",
            Phase::GmProbe => "gm-probe",
            Phase::Renewal => "renewal",
            Phase::InboundCall => "inbound-call",
            Phase::Origination => "origination",
            Phase::SmsSweep => "sms-sweep",
        }
    }

    /// How long this phase may take before it is considered stalled, or `None`
    /// for a phase that is never a stall.
    ///
    /// Every value is derived from the summed worst case of the operations the
    /// phase performs, with margin — see the module docs and the derivation
    /// test. Do not hand-tune these without updating that test.
    ///
    /// `None` is not a shortcut for "a very long budget": it states that the
    /// activity is *supposed* to be sitting here, so elapsed time carries no
    /// information about health. Giving such a phase a number, however large,
    /// reintroduces the false-restart bug the moment a schedule outgrows it.
    pub(crate) fn budget(self) -> Option<Duration> {
        Some(match self {
            // Asleep by design, for as long as its schedule says.
            Phase::Dormant => return None,
            // The loop polls every second when idle and every 100ms during a
            // call; 15s is generous slack over either.
            Phase::Idle => Duration::from_secs(15),
            // Modem open + a full initial registration, which is renewal's work
            // plus PLMN derivation.
            Phase::Startup => Duration::from_secs(420),
            // One OPTIONS round trip plus a reconnect attempt.
            Phase::GmProbe => Duration::from_secs(30),
            // See `derived_renewal_worst_case`.
            Phase::Renewal => Duration::from_secs(360),
            // Control timeout, PBX ring, bridge setup.
            Phase::InboundCall => Duration::from_secs(180),
            // Invite timeout + ring timeout + slack.
            Phase::Origination => Duration::from_secs(120),
            // A sweep re-opens the port per message; this bounds one pass.
            // Only the pass itself -- the wait between passes is `Dormant`.
            Phase::SmsSweep => Duration::from_secs(90),
        })
    }
}

/// A point-in-time read of an activity's progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProgressSnapshot {
    pub(crate) phase: Phase,
    /// When the current phase was entered.
    pub(crate) since: Instant,
    /// Whether a call is in progress, which gates deferral.
    pub(crate) busy: bool,
}

impl ProgressSnapshot {
    /// How far past its budget this phase is, if at all.
    fn overrun(&self, now: Instant) -> Option<Duration> {
        let budget = self.phase.budget()?;
        let elapsed = now.saturating_duration_since(self.since);
        (elapsed > budget).then_some(elapsed)
    }
}

/// Shared progress of one monitored activity.
///
/// One per activity, not one per process: the dispatch loop, the SMS sweep and
/// the VoLTE carrier agent each own one, because each can stall independently
/// and the sweep in particular is detached and would otherwise go unwatched
/// until a later renewal blocked behind it.
#[derive(Debug)]
pub(crate) struct Progress {
    /// Monotonic reference point. `Instant` throughout, never `SystemTime`, so
    /// an NTP step or a container clock jump cannot be mistaken for a stall
    /// (FR-014).
    base: Instant,
    phase: AtomicU8,
    /// Milliseconds since `base` at which the current phase was entered.
    phase_started_ms: AtomicU64,
    busy: AtomicBool,
    label: &'static str,
}

impl Progress {
    pub(crate) fn new(label: &'static str) -> Self {
        Self {
            base: Instant::now(),
            phase: AtomicU8::new(Phase::Idle.as_u8()),
            phase_started_ms: AtomicU64::new(0),
            busy: AtomicBool::new(false),
            label,
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        self.label
    }

    /// Record that this activity has entered `phase` as of now.
    pub(crate) fn enter(&self, phase: Phase) {
        // Store the timestamp before the phase, so a concurrent reader can
        // never see a new phase paired with the previous phase's (older) start
        // time — which would overstate the elapsed time and could trip the
        // watchdog spuriously. The reverse skew merely understates it, which is
        // safe: it can only delay detection by one sample.
        let ms = u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.phase_started_ms.store(ms, Ordering::SeqCst);
        self.phase.store(phase.as_u8(), Ordering::SeqCst);
    }

    /// Return to `Idle`.
    pub(crate) fn leave(&self) {
        self.enter(Phase::Idle);
    }

    pub(crate) fn set_busy(&self, busy: bool) {
        self.busy.store(busy, Ordering::SeqCst);
    }

    pub(crate) fn snapshot(&self) -> ProgressSnapshot {
        let phase = Phase::from_u8(self.phase.load(Ordering::SeqCst));
        let ms = self.phase_started_ms.load(Ordering::SeqCst);
        ProgressSnapshot {
            phase,
            since: self.base + Duration::from_millis(ms),
            busy: self.busy.load(Ordering::SeqCst),
        }
    }

    /// Enter `phase` for as long as the returned guard lives.
    ///
    /// `on_idle_tick` has five early returns; without RAII, any one of them
    /// would leave a phase armed after the work finished and the watchdog would
    /// eventually kill a perfectly healthy line.
    pub(crate) fn phase_guard(&self, phase: Phase) -> PhaseGuard<'_> {
        self.enter(phase);
        PhaseGuard { progress: self }
    }
}

/// Returns its `Progress` to [`Phase::Idle`] on drop — including on an early
/// return or an unwind.
pub(crate) struct PhaseGuard<'a> {
    progress: &'a Progress,
}

impl Drop for PhaseGuard<'_> {
    fn drop(&mut self) {
        self.progress.leave();
    }
}

/// What the watchdog concluded from one sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StallVerdict {
    /// Within budget.
    Healthy,
    /// Over budget for the first time. Not actionable — a single sample can be
    /// an artefact, and the cost of being wrong is a restart.
    Suspected { phase: Phase, elapsed: Duration },
    /// Over budget on two consecutive samples, with no call in progress.
    Confirmed { phase: Phase, elapsed: Duration },
    /// Confirmed, but held back because a call is in progress and the deferral
    /// ceiling has not been reached. Reported, never silent, so a deferral is
    /// distinguishable from an absence of fault.
    Deferred { phase: Phase, elapsed: Duration },
    /// Confirmed and the deferral ceiling is exceeded — act regardless of the
    /// call, because the loop can no longer tell us the call ended.
    Forced { phase: Phase, elapsed: Duration },
}

impl StallVerdict {
    pub(crate) fn stalled_for(&self) -> Option<(Phase, Duration)> {
        match *self {
            StallVerdict::Healthy => None,
            StallVerdict::Suspected { phase, elapsed }
            | StallVerdict::Confirmed { phase, elapsed }
            | StallVerdict::Deferred { phase, elapsed }
            | StallVerdict::Forced { phase, elapsed } => Some((phase, elapsed)),
        }
    }
}

/// Decide what one sample means.
///
/// Pure: `now` is a parameter and nothing here touches a clock, a lock or the
/// filesystem, so every branch is unit-testable without waiting on real time.
///
/// `previous` is the snapshot from the last sample, and is what implements
/// two-sample confirmation: a verdict only escalates past `Suspected` when the
/// *same phase instance* (same phase, same start time) was already over budget
/// last time.
pub(crate) fn stall_verdict(
    current: ProgressSnapshot,
    previous: Option<ProgressSnapshot>,
    now: Instant,
    defer_ceiling: Duration,
) -> StallVerdict {
    let Some(elapsed) = current.overrun(now) else {
        return StallVerdict::Healthy;
    };
    let phase = current.phase;

    // Confirmation requires the previous sample to have been the *same* phase
    // instance and also over budget. Comparing `since` as well as `phase`
    // matters: an activity that legitimately re-enters the same phase has a new
    // start time, and must start its confirmation over rather than inheriting
    // the previous instance's suspicion.
    let confirmed = previous.is_some_and(|prev| {
        prev.phase == phase && prev.since == current.since && prev.overrun(now).is_some()
    });
    if !confirmed {
        return StallVerdict::Suspected { phase, elapsed };
    }

    if current.busy {
        // A call is up. Hold off — unless we have held off so long that the
        // loop is clearly never going to tell us the call ended.
        if elapsed >= defer_ceiling {
            return StallVerdict::Forced { phase, elapsed };
        }
        return StallVerdict::Deferred { phase, elapsed };
    }

    StallVerdict::Confirmed { phase, elapsed }
}

/// What the watchdog thread should do about a verdict.
///
/// Split out from the acting so the policy — including the
/// `watchdog_recovery_enabled` escape hatch — is pure and testable without
/// terminating the test runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchdogAction {
    /// Nothing to do.
    Nothing,
    /// Report a deferral (once per episode).
    ReportDeferral,
    /// Report the stall but do not terminate: recovery is disabled so the line
    /// can be preserved for diagnosis (FR-034/FR-035).
    ReportOnly,
    /// Report and terminate so the supervisor restarts this line.
    Terminate,
}

/// Map a verdict to an action, honouring the recovery kill switch.
///
/// FR-035 is the subtle requirement here: disabling recovery must *not* restore
/// the original condition in which a stalled line looks healthy. So a disabled
/// watchdog still reports — it just does not exit.
pub(crate) fn action_for(verdict: StallVerdict, recovery_enabled: bool) -> WatchdogAction {
    match verdict {
        StallVerdict::Healthy | StallVerdict::Suspected { .. } => WatchdogAction::Nothing,
        StallVerdict::Deferred { .. } => WatchdogAction::ReportDeferral,
        StallVerdict::Confirmed { .. } | StallVerdict::Forced { .. } => {
            if recovery_enabled {
                WatchdogAction::Terminate
            } else {
                WatchdogAction::ReportOnly
            }
        }
    }
}

/// Every activity being monitored in this process.
///
/// A registry rather than handles threaded through constructors: activities are
/// created in five different places (the dispatch loop, the SMS sweep spawned
/// from three call sites, the VoLTE carrier agent), and passing a watchdog
/// handle to each would add a parameter to functions that otherwise have no
/// reason to know a watchdog exists. Agents are one process per line, so a
/// process-wide registry is exactly the right scope.
static REGISTRY: std::sync::Mutex<Vec<std::sync::Arc<Progress>>> =
    std::sync::Mutex::new(Vec::new());

/// Register an activity to be monitored, returning it for the caller's own use.
///
/// Safe to call before or after [`spawn`] — the sampling loop re-reads the
/// registry each pass, so an activity that starts later is picked up.
pub(crate) fn register(progress: std::sync::Arc<Progress>) -> std::sync::Arc<Progress> {
    REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(std::sync::Arc::clone(&progress));
    progress
}

fn registered() -> Vec<std::sync::Arc<Progress>> {
    REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(std::sync::Arc::clone)
        .collect()
}

/// Per-activity bookkeeping the sampling loop carries between samples.
#[derive(Default)]
struct Tracked {
    previous: Option<ProgressSnapshot>,
    /// Whether the current deferral episode has already been logged, so a long
    /// deferral produces one line rather than one every five seconds.
    deferral_reported: bool,
    /// Same, for a report-only stall when recovery is disabled.
    stall_reported: bool,
}

/// Start the watchdog thread watching every registered activity.
///
/// Spawn failure is fatal rather than a warning: a silently absent watchdog
/// would reinstate exactly the blind spot this feature exists to remove, and it
/// would do so invisibly. Better to refuse to start the line.
pub(crate) fn spawn(recovery_enabled: bool) -> crate::error::BridgeResult<()> {
    // One watchdog per process, watching every registered activity. Guarded so
    // that a second caller (the two bearers reach `serve_inbound` by different
    // routes) is a harmless no-op rather than a duplicate sampling thread.
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    if !recovery_enabled {
        tracing::warn!(
            "watchdog recovery is disabled by configuration: stalls will be detected and \
             reported, but a stalled line will not be restarted"
        );
    }
    std::thread::Builder::new()
        .name("ims-watchdog".to_string())
        .spawn(move || run_loop(recovery_enabled))
        .map(|_| ())
        .map_err(|e| {
            crate::error::BridgeError::Ims(format!(
                "failed to start the stall watchdog, refusing to run unmonitored: {e}"
            ))
        })
}

fn run_loop(recovery_enabled: bool) {
    // Indexed in lockstep with the registry, which only ever grows.
    let mut tracked: Vec<Tracked> = Vec::new();

    loop {
        std::thread::sleep(SAMPLE_INTERVAL);
        let now = Instant::now();
        let activities = registered();
        if tracked.len() < activities.len() {
            tracked.resize_with(activities.len(), Tracked::default);
        }
        for (progress, t) in activities.iter().zip(tracked.iter_mut()) {
            let current = progress.snapshot();
            let verdict = stall_verdict(current, t.previous, now, DEFER_CEILING);
            t.previous = Some(current);

            // Reset the once-per-episode latches as soon as the activity is
            // moving again, so a later episode is reported afresh.
            if matches!(verdict, StallVerdict::Healthy) {
                t.deferral_reported = false;
                t.stall_reported = false;
                continue;
            }

            let Some((phase, elapsed)) = verdict.stalled_for() else {
                continue;
            };
            match action_for(verdict, recovery_enabled) {
                WatchdogAction::Nothing => {}
                WatchdogAction::ReportDeferral => {
                    if !t.deferral_reported {
                        t.deferral_reported = true;
                        tracing::warn!(
                            activity = progress.label(),
                            phase = phase.label(),
                            stalled_secs = elapsed.as_secs(),
                            budget_secs = phase.budget().map(|b| b.as_secs()),
                            "{}",
                            DEFER_MARKER
                        );
                    }
                }
                WatchdogAction::ReportOnly => {
                    if !t.stall_reported {
                        t.stall_reported = true;
                        report_stall(progress.label(), phase, elapsed);
                        tracing::error!(
                            "watchdog recovery is disabled; leaving this line stalled for diagnosis"
                        );
                    }
                }
                WatchdogAction::Terminate => {
                    report_stall(progress.label(), phase, elapsed);
                    // A thread blocked in `read(2)` cannot be cancelled and
                    // holds the serial port, so only process death frees it.
                    // Logging is synchronous to stderr, which the supervisor
                    // redirects to the file it reads back to classify this
                    // exit, so the marker above is durable before we go.
                    std::process::exit(EXIT_WATCHDOG_STALL);
                }
            }
        }
    }
}

fn report_stall(activity: &'static str, phase: Phase, elapsed: Duration) {
    // `last_at_command` is the single highest-value diagnostic: the original
    // incident needed a kernel stack and an fd table to establish what the
    // agent had been waiting on.
    let last_at = crate::modules::at_commander::last_at_command();
    let (last_cmd, last_cmd_age_secs) = match last_at {
        Some((cmd, at)) => (cmd, Some(at.elapsed().as_secs())),
        None => ("<none>".to_string(), None),
    };
    tracing::error!(
        activity,
        phase = phase.label(),
        stalled_secs = elapsed.as_secs(),
        budget_secs = phase.budget().map(|b| b.as_secs()),
        last_at_command = %last_cmd,
        last_at_command_age_secs = ?last_cmd_age_secs,
        "{}",
        STALL_MARKER
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The renewal worst case, recomputed from the constants the renewal path
    /// actually uses. Kept in the test rather than in `budget()` so that
    /// `budget()` stays a plain table a human can read, while the derivation
    /// remains machine-checked.
    fn derived_renewal_worst_case() -> Duration {
        // Opening the modem, when another holder has it.
        let modem_open = crate::ims::MODEM_OPEN_MAX_WAIT;
        // The EF_DIR walk selects the MF and EF_DIR, then reads up to 16
        // records, and `select_usim` adds two more selects. Each APDU is one
        // AT command bounded by the AT layer's default timeout.
        let apdus = 16 + 2 + 2;
        let apdu_time = crate::modules::at_commander::DEFAULT_TIMEOUT * apdus;
        // Two REGISTER round trips (the challenge and the authenticated
        // retry), each bounded by SIP Timer B.
        let register = Duration::from_secs(32) * 2;
        // Installing the Gm SAs shells out to `ip xfrm` ~20 times.
        let sa_install = Duration::from_secs(20);
        modem_open + apdu_time + register + sa_install
    }

    #[test]
    fn budgets_exceed_their_derived_worst_case() {
        // The guard against a future timeout bump silently arming false
        // restarts: if someone raises `DEFAULT_TIMEOUT` or the APDU count, this
        // fails the build instead of the watchdog killing healthy lines.
        let worst = derived_renewal_worst_case();
        let budget = Phase::Renewal
            .budget()
            .expect("renewal is a working phase and must carry a budget");
        assert!(
            budget > worst,
            "renewal budget {budget:?} must exceed its derived worst case {worst:?}"
        );
        let margin = budget.as_secs_f64() / worst.as_secs_f64() - 1.0;
        assert!(
            margin >= 0.20,
            "renewal budget {budget:?} leaves only {:.1}% margin over {worst:?}; want >=20%",
            margin * 100.0
        );
    }

    #[test]
    fn idle_budget_dwarfs_the_poll_interval() {
        assert!(Phase::Idle
            .budget()
            .is_some_and(|b| b >= super::super::IDLE_POLL_INTERVAL * 10));
    }

    #[test]
    fn phase_discriminants_round_trip() {
        for phase in [
            Phase::Idle,
            Phase::Startup,
            Phase::GmProbe,
            Phase::Renewal,
            Phase::InboundCall,
            Phase::Origination,
            Phase::SmsSweep,
        ] {
            assert_eq!(Phase::from_u8(phase.as_u8()), phase);
        }
    }

    /// A working phase's budget. Panics for a no-deadline phase, which is
    /// exactly what a test asserting on elapsed time should do.
    fn budget_of(phase: Phase) -> Duration {
        phase
            .budget()
            .unwrap_or_else(|| panic!("{phase:?} carries no budget"))
    }

    fn snap(phase: Phase, since: Instant, busy: bool) -> ProgressSnapshot {
        ProgressSnapshot { phase, since, busy }
    }

    #[test]
    fn within_budget_is_healthy() {
        let now = Instant::now();
        let s = snap(Phase::Renewal, now, false);
        assert_eq!(
            stall_verdict(s, Some(s), now, DEFER_CEILING),
            StallVerdict::Healthy
        );
    }

    #[test]
    fn a_single_overrun_is_only_suspected() {
        let now = Instant::now();
        let since = now - budget_of(Phase::GmProbe) - Duration::from_secs(1);
        let s = snap(Phase::GmProbe, since, false);
        // No previous sample: this is the first observation.
        let v = stall_verdict(s, None, now, DEFER_CEILING);
        assert!(matches!(v, StallVerdict::Suspected { .. }), "{v:?}");
        assert_eq!(action_for(v, true), WatchdogAction::Nothing);
    }

    #[test]
    fn two_consecutive_overruns_confirm() {
        let now = Instant::now();
        let since = now - budget_of(Phase::GmProbe) - Duration::from_secs(1);
        let s = snap(Phase::GmProbe, since, false);
        let v = stall_verdict(s, Some(s), now, DEFER_CEILING);
        assert!(matches!(v, StallVerdict::Confirmed { .. }), "{v:?}");
        assert_eq!(action_for(v, true), WatchdogAction::Terminate);
    }

    #[test]
    fn re_entering_the_same_phase_restarts_confirmation() {
        // A line that legitimately retries an operation enters the same phase
        // again with a fresh start time. That must not inherit the previous
        // attempt's suspicion, or a sequence of slow-but-successful retries
        // would be read as one long stall.
        let now = Instant::now();
        let old = snap(
            Phase::Renewal,
            now - budget_of(Phase::Renewal) - Duration::from_secs(30),
            false,
        );
        let fresh = snap(
            Phase::Renewal,
            now - budget_of(Phase::Renewal) - Duration::from_secs(1),
            false,
        );
        let v = stall_verdict(fresh, Some(old), now, DEFER_CEILING);
        assert!(matches!(v, StallVerdict::Suspected { .. }), "{v:?}");
    }

    #[test]
    fn a_call_in_progress_defers_recovery() {
        let now = Instant::now();
        let since = now - budget_of(Phase::Renewal) - Duration::from_secs(1);
        let s = snap(Phase::Renewal, since, true);
        let v = stall_verdict(s, Some(s), now, DEFER_CEILING);
        assert!(matches!(v, StallVerdict::Deferred { .. }), "{v:?}");
        assert_eq!(
            action_for(v, true),
            WatchdogAction::ReportDeferral,
            "a deferred verdict must not terminate the process"
        );
        assert!(
            v.stalled_for().is_some(),
            "a deferral must still be reported, not look like an absence of fault"
        );
    }

    #[test]
    fn deferral_is_forced_once_the_ceiling_is_exceeded() {
        // The case that stops deferral becoming a second way to hang: a wedged
        // loop can never observe the call ending, so `busy` stays true forever.
        let now = Instant::now();
        let since = now - DEFER_CEILING - Duration::from_secs(1);
        let s = snap(Phase::Renewal, since, true);
        let v = stall_verdict(s, Some(s), now, DEFER_CEILING);
        assert!(matches!(v, StallVerdict::Forced { .. }), "{v:?}");
        assert_eq!(action_for(v, true), WatchdogAction::Terminate);
    }

    #[test]
    fn a_dormant_activity_never_trips_however_long_it_rests() {
        // Regression test for a live false-restart. The SMS sweep sleeps
        // `MODEM_SWEEP_INTERVAL` between passes; it rested in `Idle`, whose
        // budget is *shorter* than that sleep, so the watchdog confirmed a
        // stall and killed the agent roughly every 36 seconds. The line could
        // not stay registered, which is strictly worse than the bug this
        // feature exists to fix.
        assert_eq!(
            Phase::Dormant.budget(),
            None,
            "a phase an activity rests in by design must carry no deadline"
        );
        let now = Instant::now();
        let rested = crate::volte::sms::MODEM_SWEEP_INTERVAL + Duration::from_secs(3600);
        let s = snap(Phase::Dormant, now - rested, false);
        assert_eq!(
            stall_verdict(s, Some(s), now, DEFER_CEILING),
            StallVerdict::Healthy
        );

        // ...and pin why `Idle` cannot be reused for that, so nobody "tidies"
        // `Dormant` away or gives it a budget for symmetry.
        let as_idle = snap(Phase::Idle, now - rested, false);
        assert!(
            matches!(
                stall_verdict(as_idle, Some(as_idle), now, DEFER_CEILING),
                StallVerdict::Confirmed { .. }
            ),
            "resting in Idle for a sweep interval is exactly the bug that shipped"
        );
    }

    #[test]
    fn every_scheduled_wait_outlives_or_avoids_its_phase_budget() {
        // The general form of the bug above: any interval a thread deliberately
        // sleeps for must either be `Dormant` or be shorter than the budget of
        // the phase it sleeps in. Checked against the real constants, so a
        // future change to either side fails here rather than on the phone line.
        let sweep_wait = crate::volte::sms::MODEM_SWEEP_INTERVAL;
        assert!(
            budget_of(Phase::Idle) < sweep_wait,
            "if Idle's budget ever exceeds the sweep interval this test stops \
             protecting anything -- re-derive it rather than deleting this"
        );
        assert_eq!(Phase::Dormant.budget(), None);
    }

    #[test]
    fn idle_within_its_budget_never_trips() {
        let now = Instant::now();
        let s = snap(Phase::Idle, now - Duration::from_secs(2), false);
        assert_eq!(
            stall_verdict(s, Some(s), now, DEFER_CEILING),
            StallVerdict::Healthy
        );
    }

    #[test]
    fn disabling_recovery_still_reports_the_stall() {
        // FR-035. The escape hatch exists to preserve a wedged line for
        // diagnosis; it must not quietly restore the original condition in
        // which a stalled line looked perfectly healthy.
        let now = Instant::now();
        let since = now - budget_of(Phase::Renewal) - Duration::from_secs(1);
        let s = snap(Phase::Renewal, since, false);
        let v = stall_verdict(s, Some(s), now, DEFER_CEILING);
        assert_eq!(action_for(v, false), WatchdogAction::ReportOnly);
        assert!(
            v.stalled_for().is_some(),
            "the stall must still be reportable when recovery is disabled"
        );
    }

    #[test]
    fn a_healthy_line_never_terminates_regardless_of_the_switch() {
        let now = Instant::now();
        let s = snap(Phase::Renewal, now, false);
        let v = stall_verdict(s, Some(s), now, DEFER_CEILING);
        assert_eq!(action_for(v, true), WatchdogAction::Nothing);
        assert_eq!(action_for(v, false), WatchdogAction::Nothing);
    }

    #[test]
    fn phase_guard_restores_idle_on_drop() {
        let p = Progress::new("test");
        {
            let _g = p.phase_guard(Phase::Renewal);
            assert_eq!(p.snapshot().phase, Phase::Renewal);
        }
        assert_eq!(p.snapshot().phase, Phase::Idle);
    }

    #[test]
    fn phase_guard_restores_idle_on_unwind() {
        // The early-return and panic paths are the whole reason for RAII here.
        let p = Progress::new("test");
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = p.phase_guard(Phase::Renewal);
            panic!("boom");
        }));
        assert!(caught.is_err());
        assert_eq!(p.snapshot().phase, Phase::Idle);
    }

    #[test]
    fn snapshot_reports_entered_phase_and_busy() {
        let p = Progress::new("test");
        p.enter(Phase::SmsSweep);
        p.set_busy(true);
        let s = p.snapshot();
        assert_eq!(s.phase, Phase::SmsSweep);
        assert!(s.busy);
        assert!(s.since.elapsed() < Duration::from_secs(5));
    }
}
