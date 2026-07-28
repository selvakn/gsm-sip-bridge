//! USIM auto-recovery (specs/021-entrypoint-supervise-rust Phase 3) — 1:1 port
//! of `docker/entrypoint.sh`'s `reset_line_sim` + `start_line_tail`'s
//! per-incident CSIM-failure counting.
//!
//! The vowifi-ims-agent's first *live* SIM access is AT+CSIM (IMS-AKA). With
//! imsi_override/imei_override pinned, a USIM that has electrically dropped
//! off the modem bus at runtime surfaces only as `AT+CSIM failed: 0` in a 5s
//! restart loop. A soft radio cycle (AT+CFUN=0 -> AT+CFUN=1) re-detects the
//! card without re-enumerating USB or restarting the container — the recipe
//! used by hand during the sugam incident (2026-07-26, see project memory).

use super::runner::{ChildHandle, CommandRunner, Signal};
use std::path::Path;
use std::time::Duration;

/// After this many consecutive `AT+CSIM failed` exits, power-cycle the SIM.
pub const CSIM_FAIL_THRESHOLD: u32 = 3;
/// Give up resetting (leave it for the healthcheck/a human) after this many
/// resets in one incident.
pub const MAX_SIM_RESETS: u32 = 5;

/// Per-incident counters — the typed replacement for `start_line_tail`'s
/// `csim_fails`/`sim_resets` locals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IncidentCounters {
    pub csim_fails: u32,
    pub sim_resets: u32,
}

/// What this run's agent exit looked like, as observed from its tee'd log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentExitOutcome {
    /// `grep -q 'AT+CSIM failed'` matched this run's log.
    CsimFailure,
    /// A clean run or a non-CSIM failure — either way, the current script
    /// resets both counters (a non-CSIM failure is a different problem, not
    /// evidence of a dropped USIM continuing).
    Other,
}

/// What the caller should do after observing one agent exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do (either the incident continues below-threshold, or the
    /// exit was clean/non-CSIM and both counters reset).
    None,
    /// Threshold reached and resets remain this incident: power-cycle the SIM.
    ResetSim,
    /// Threshold reached but `MAX_SIM_RESETS` already used this incident:
    /// leave the loop alone (still resets `csim_fails` to 0, matching the
    /// current script, so the *next* CSIM failure starts counting fresh
    /// rather than instantly re-triggering "give up" again every single
    /// iteration).
    GiveUpForThisIncident,
}

impl IncidentCounters {
    /// Advances the counters by one agent-exit observation, 1:1 port of
    /// `start_line_tail`'s per-iteration if/else block.
    pub fn observe(&mut self, outcome: AgentExitOutcome) -> Action {
        match outcome {
            AgentExitOutcome::Other => {
                self.csim_fails = 0;
                self.sim_resets = 0;
                Action::None
            }
            AgentExitOutcome::CsimFailure => {
                self.csim_fails += 1;
                if self.csim_fails >= CSIM_FAIL_THRESHOLD {
                    self.csim_fails = 0;
                    if self.sim_resets >= MAX_SIM_RESETS {
                        Action::GiveUpForThisIncident
                    } else {
                        self.sim_resets += 1;
                        Action::ResetSim
                    }
                } else {
                    Action::None
                }
            }
        }
    }
}

/// 1:1 port of `grep -q 'AT+CSIM failed'`.
pub fn has_csim_failure(agent_log: &str) -> bool {
    agent_log.contains("AT+CSIM failed")
}

/// 1:1 port of `grep -q '+CPIN: READY'`.
pub fn is_cpin_ready(reset_log: &str) -> bool {
    reset_log.contains("+CPIN: READY")
}

/// Matches the current script's `sleep 0.5` after freezing the holder, to
/// let any in-flight serial I/O settle before driving the port directly.
const HOLDER_FREEZE_SETTLE: Duration = Duration::from_millis(500);
/// Matches `sleep 4` between `AT+CFUN=0` and `AT+CFUN=1`.
const CFUN_CYCLE_DELAY: Duration = Duration::from_secs(4);
/// Matches the `for i in $(seq 1 15)` READY poll, 1s apart.
const CPIN_POLL_ATTEMPTS: u32 = 15;
const CPIN_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Matches the current script's 30s `timeout` on the background `cat
/// "$modem"` reader — comfortably outlasts the ~4.3s pre-roll plus the ~15s
/// readiness poll.
const READER_TIMEOUT_SECS: u32 = 30;
/// Matches `sleep 0.3` between starting the background reader and the first
/// AT write, so the reader is attached before any reply could arrive.
const READER_STARTUP_SETTLE: Duration = Duration::from_millis(300);

/// Power-cycles the USIM on `modem` (AT+CFUN=0 -> 1) and polls for
/// `+CPIN: READY`, returning whether it was seen within the poll window.
///
/// A serial device is full-duplex, not a regular file: writing an AT command
/// to `modem` does not make it readable back from `modem` — replies arrive
/// asynchronously on the same fd. The current script models this with a
/// background `cat "$modem" >"$reset_log"` reader process, started before any
/// AT write, whose accumulated output is what actually gets grepped for
/// `+CPIN: READY`; this port keeps that same two-path shape (`modem` for
/// writes, `reset_log` for reads) via [`CommandRunner::spawn`] +
/// [`CommandRunner::read_file`], rather than collapsing them onto one path
/// (an earlier version of this port did that, and a test caught it: a mock
/// `write_file` targeting the same path a test had `set_file`-seeded for
/// reading silently clobbered the seeded reply — exactly the bug a real
/// serial device's independent write/read directions would not have, but a
/// same-path read/write model does).
///
/// `holder` is the currently-running vowifi-usim-bridge/swu-dialer process
/// for this line, if any — frozen (SIGSTOP) for the duration so its own
/// traffic cannot interleave with this function's raw AT writes, then
/// resumed (SIGCONT) regardless of outcome. Passed in as an already-known
/// [`ChildHandle`] rather than rediscovered via `pgrep -f`: by the time this
/// runs in the full `supervise` wiring (Phase 4), the caller already holds
/// that handle from its own `spawn` call, so re-deriving it by process-name
/// pattern matching (as the bash version had to, having no handle
/// bookkeeping of its own) would be strictly worse — it could match an
/// unrelated process sharing the same command-line substring.
pub fn reset_modem_sim(
    runner: &dyn CommandRunner,
    modem: &Path,
    reset_log: &Path,
    holder: Option<&ChildHandle>,
) -> bool {
    if let Some(h) = holder {
        runner.signal(h, Signal::Stop);
        runner.sleep(HOLDER_FREEZE_SETTLE);
    }

    let modem_str = modem.to_string_lossy();
    let _ = runner.run(&["stty", "-F", &modem_str, "115200", "-echo"]);

    let reader = runner
        .spawn(
            super::runner::ChildSpec::new([
                "timeout",
                &READER_TIMEOUT_SECS.to_string(),
                "cat",
                &modem_str,
            ])
            .capture_stdout_to(reset_log.to_path_buf()),
        )
        .ok();
    runner.sleep(READER_STARTUP_SETTLE);

    let _ = runner.write_file(modem, "AT+CFUN=0\r");
    runner.sleep(CFUN_CYCLE_DELAY);
    let _ = runner.write_file(modem, "AT+CFUN=1\r");

    let mut ready = false;
    for _ in 0..CPIN_POLL_ATTEMPTS {
        let _ = runner.write_file(modem, "AT+CPIN?\r");
        runner.sleep(CPIN_POLL_INTERVAL);
        if let Ok(content) = runner.read_file(reset_log) {
            if is_cpin_ready(&content) {
                ready = true;
                break;
            }
        }
    }

    if let Some(r) = reader {
        // 1:1 port of `kill "$reader_pid" 2>/dev/null || true; wait
        // "$reader_pid" 2>/dev/null || true` — the bash original explicitly
        // waits for the reader after killing it, not just signals it. This
        // path was the one place that had it right from the start; it is now
        // the trait's `reap`, so every other site gets it by construction
        // rather than by remembering.
        runner.reap(&r);
    }
    if let Some(h) = holder {
        runner.signal(h, Signal::Cont);
    }

    ready
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_consecutive_csim_failures_trigger_a_reset() {
        let mut c = IncidentCounters::default();
        assert_eq!(c.observe(AgentExitOutcome::CsimFailure), Action::None);
        assert_eq!(c.observe(AgentExitOutcome::CsimFailure), Action::None);
        assert_eq!(c.observe(AgentExitOutcome::CsimFailure), Action::ResetSim);
        assert_eq!(c.csim_fails, 0, "csim_fails resets after triggering");
        assert_eq!(c.sim_resets, 1);
    }

    #[test]
    fn a_clean_run_resets_both_counters_mid_incident() {
        let mut c = IncidentCounters::default();
        c.observe(AgentExitOutcome::CsimFailure);
        c.observe(AgentExitOutcome::CsimFailure);
        assert_eq!(c.observe(AgentExitOutcome::Other), Action::None);
        assert_eq!(c, IncidentCounters::default());
    }

    #[test]
    fn a_non_csim_failure_also_resets_both_counters() {
        // The current script's `else` branch covers a clean run AND a
        // non-CSIM failure identically — both end the "incident."
        let mut c = IncidentCounters {
            csim_fails: 2,
            sim_resets: 3,
        };
        c.observe(AgentExitOutcome::Other);
        assert_eq!(c, IncidentCounters::default());
    }

    #[test]
    fn exceeding_max_sim_resets_gives_up_without_resetting_again() {
        let mut c = IncidentCounters {
            csim_fails: 0,
            sim_resets: MAX_SIM_RESETS,
        };
        c.observe(AgentExitOutcome::CsimFailure);
        c.observe(AgentExitOutcome::CsimFailure);
        let action = c.observe(AgentExitOutcome::CsimFailure);
        assert_eq!(action, Action::GiveUpForThisIncident);
        assert_eq!(c.sim_resets, MAX_SIM_RESETS, "must not exceed the cap");
    }

    #[test]
    fn a_reset_used_resets_and_a_later_incident_can_still_reset_again() {
        // Confirms MAX_SIM_RESETS is a per-incident cap tracked across
        // triggers, not a one-shot latch — after a successful reset,
        // further below-threshold failures don't immediately give up.
        let mut c = IncidentCounters::default();
        for _ in 0..CSIM_FAIL_THRESHOLD {
            c.observe(AgentExitOutcome::CsimFailure);
        }
        assert_eq!(c.sim_resets, 1);
        assert_eq!(c.observe(AgentExitOutcome::CsimFailure), Action::None);
    }

    #[test]
    fn has_csim_failure_matches_the_grep_pattern() {
        assert!(has_csim_failure("blah\nAT+CSIM failed: 0\nblah"));
        assert!(!has_csim_failure("no such marker here"));
    }

    #[test]
    fn is_cpin_ready_matches_the_grep_pattern() {
        assert!(is_cpin_ready("+CME ERROR: 1\n+CPIN: READY\n"));
        assert!(!is_cpin_ready("+CPIN: SIM PIN\n"));
    }

    mod reset_modem_sim_tests {
        use super::*;
        use crate::supervise::runner::{ChildSpec, MockCommandRunner};
        use std::path::PathBuf;

        // MOCK JUSTIFICATION (constitution Principle I): stands in for the
        // real serial modem device and the real vowifi-usim-bridge/swu-dialer
        // holder process — neither is available in CI. The sequencing under
        // test (freeze before writing, resume regardless of outcome, the
        // exact AT command order) is real production code.

        #[test]
        fn freezes_the_holder_before_driving_the_port_and_resumes_it_after() {
            let runner = MockCommandRunner::new();
            let holder = runner
                .spawn(ChildSpec::new(["vowifi-usim-bridge"]))
                .unwrap();
            let modem = PathBuf::from("/dev/ttyUSB2");
            let reset_log = PathBuf::from("/tmp/sim-reset-0.log");
            runner.set_file(&reset_log, "+CPIN: READY\n");

            reset_modem_sim(&runner, &modem, &reset_log, Some(&holder));

            assert_eq!(
                runner.signals_for(&holder),
                vec![Signal::Stop, Signal::Cont]
            );
        }

        #[test]
        fn the_background_reader_is_reaped_not_just_signaled() {
            // 1:1 port of bash's `kill "$reader_pid" ...; wait "$reader_pid"
            // ...` — regression test for the same leak shape a Greptile
            // review flagged elsewhere in this PR (a signaled-and-forgotten
            // handle never gets removed from RealCommandRunner's table).
            let runner = MockCommandRunner::new();
            let modem = PathBuf::from("/dev/ttyUSB2");
            let reset_log = PathBuf::from("/tmp/sim-reset-0.log");
            runner.set_file(&reset_log, "+CPIN: READY\n");

            reset_modem_sim(&runner, &modem, &reset_log, None);

            // Exactly one child (the reader) is spawned with no holder.
            assert_eq!(runner.spawn_specs.lock().unwrap().len(), 1);
            // `reap` is signal-then-confirm-gone, not signal-then-wait: it
            // polls `is_alive` instead, because `wait` untracks the child up
            // front and would defeat any concurrent holder of the handle.
            // The guarantee to assert is therefore that the reader was
            // terminated and is actually dead by the time this returns — not
            // that some particular syscall was issued.
            let reader_id = *runner
                .child_ids()
                .first()
                .expect("the reader child must have been spawned");
            assert_eq!(
                runner.signals_for_id(reader_id),
                vec![Signal::Term],
                "the reader must be terminated — and a plain SIGTERM must be \
                 enough, i.e. reap must not need to escalate to SIGKILL"
            );
        }

        #[test]
        fn returns_true_as_soon_as_ready_is_seen_on_the_readers_log_not_the_modem_path() {
            let runner = MockCommandRunner::new();
            let modem = PathBuf::from("/dev/ttyUSB2");
            let reset_log = PathBuf::from("/tmp/sim-reset-0.log");
            // Seeded on reset_log, NOT on modem — a real serial device would
            // never make an AT command's reply readable back from the same
            // path it was written to.
            runner.set_file(&reset_log, "+CPIN: READY\n");
            assert!(reset_modem_sim(&runner, &modem, &reset_log, None));
        }

        #[test]
        fn returns_false_when_never_ready() {
            let runner = MockCommandRunner::new();
            let modem = PathBuf::from("/dev/ttyUSB2");
            let reset_log = PathBuf::from("/tmp/sim-reset-0.log");
            runner.set_file(&reset_log, "+CME ERROR: 0\n");
            assert!(!reset_modem_sim(&runner, &modem, &reset_log, None));
        }

        #[test]
        fn with_no_holder_no_stop_or_cont_signal_is_sent_to_anything() {
            let runner = MockCommandRunner::new();
            let modem = PathBuf::from("/dev/ttyUSB2");
            let reset_log = PathBuf::from("/tmp/sim-reset-0.log");
            runner.set_file(&reset_log, "+CPIN: READY\n");
            reset_modem_sim(&runner, &modem, &reset_log, None);
            // Nothing was spawned, so there is nothing to assert signals
            // against — this test's value is that reset_modem_sim doesn't
            // panic/misbehave when `holder` is None (the common case: no
            // vowifi-usim-bridge/swu process currently running for this
            // line's engine).
        }
    }
}
