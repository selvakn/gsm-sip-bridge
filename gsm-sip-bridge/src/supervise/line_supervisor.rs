//! One per-line VoWiFi tunnel supervisor, generic over the tunnel engine
//! (specs/021-entrypoint-supervise-rust Phase 3) — collapses the three
//! duplicated bash loops (strongswan establish-time, strongswan steady-state,
//! swu establish+steady-state) into one state machine (FR-005). Every
//! transition and its recovery action is a 1:1 port of the current script's
//! control flow, not a redesign: see the per-branch comments below for the
//! exact bash this replaces.

use super::runner::CommandRunner;
use std::time::Duration;

/// Mirrors the current establish-time loop's `attempt`/`stuck_without_pcscf`
/// locals and the steady-state loop's tracked P-CSCF.
#[derive(Debug, Clone, PartialEq)]
pub enum LineState {
    Establishing {
        attempt: u32,
        stuck_without_pcscf: bool,
    },
    Up {
        pcscf: String,
    },
    /// A steady-state problem was detected this tick; the caller (the
    /// per-line supervisor thread) has already issued the recovery action by
    /// the time `tick` returns this — `Restarting` is a one-tick transient
    /// the next `tick` moves on from, mirroring the bash loop's `continue`.
    Degraded {
        reason: DegradeReason,
    },
    Restarting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradeReason {
    ProcessDied,
    ViciBroken,
    TunVanished,
    ChildSaMissing,
    PcscfChanged,
}

/// Steady-state-only health, beyond "is the process alive" and "is a P-CSCF
/// change pending" (checked separately) — the three strongswan-specific
/// checks (`tun_iface` presence, `swanctl --list-sas`'s exit status, the
/// `ims:` CHILD_SA line's presence) collapse to `Ok` for the swu engine,
/// which the current script's own comment says has "no re-initiate-in-place
/// concept."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteadyStateHealth {
    Ok,
    ViciBroken,
    TunVanished,
    ChildSaMissing,
}

/// Matches the establish-time loop's `sleep 2` between polls, both engines.
pub const ESTABLISH_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Matches the strongswan establish-time loop's `$((attempt % 15))`
/// re-initiate cadence (2s * 15 = 30s between re-initiate attempts).
pub const STRONGSWAN_REINITIATE_EVERY: u32 = 15;
/// Matches the swu establish-time loop's `seq 1 90` bound (90 * 2s = 180s).
pub const SWU_MAX_ESTABLISH_ATTEMPTS: u32 = 90;
/// Matches the steady-state loop's `sleep 30` cadence.
pub const STEADY_STATE_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Engine-specific behavior the shared state machine drives. One
/// implementation per `[vowifi].tunnel_engine` value
/// (`StrongswanEngine`/`SwuEngine`, below) — this is the seam research.md R6
/// calls out as what collapses the two engines' duplicate loops into one
/// transition table without losing engine-specific behavior.
pub trait TunnelEngine {
    /// Is the tunnel established, per this engine's own log marker
    /// (`CHILD_SA.*established` for strongswan, `STATE CONNECTED` for swu)?
    fn is_tunnel_established(&self, runner: &dyn CommandRunner) -> bool;
    /// Latest P-CSCF address from wherever this engine's log records it.
    fn latest_pcscf(&self, runner: &dyn CommandRunner) -> Option<String>;
    /// Is this line's primary process (charon / the swu dialer) still alive?
    fn is_process_alive(&self, runner: &dyn CommandRunner) -> bool;
    /// Terminates the current negotiation without restarting the process
    /// (strongswan: `swanctl --terminate --ike ims`; swu: no-op — no
    /// in-place terminate exists for this engine).
    fn terminate(&self, runner: &dyn CommandRunner);
    /// (Re)initiates a connection attempt without restarting the process
    /// (strongswan: `swanctl --initiate --child ims`; swu: no-op, matching
    /// `terminate`).
    fn reinitiate(&self, runner: &dyn CommandRunner);
    /// Steady-state-only health beyond process-alive/P-CSCF-change.
    fn steady_state_health(&self, runner: &dyn CommandRunner) -> SteadyStateHealth;
    /// Fully restarts this line's primary process from scratch (strongswan:
    /// kill, clear log, remove the stale unqualified pidfile, respawn
    /// charon, `swanctl --load-all`, `swanctl --initiate`; swu: respawn the
    /// dialer). The one place a dead/broken process is actually replaced.
    fn restart_process(&self, runner: &dyn CommandRunner);
    /// The maximum number of establish-time attempts before giving up
    /// (`Some(90)` for swu; `None` for strongswan, which retries forever —
    /// FR-004 of specs/012-strongswan-epdg, "no bounded give-up").
    fn max_establish_attempts(&self) -> Option<u32>;
    /// The attempt-count cadence at which the establish loop re-initiates
    /// mid-wait (`Some(15)` for strongswan; `None` for swu, which has no
    /// periodic action beyond polling — it only ever fully restarts the
    /// dialer, on the steady-state side, never re-initiates in place).
    fn reinitiate_cadence(&self) -> Option<u32>;
}

/// Outcome of one establish-time tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EstablishOutcome {
    StillEstablishing,
    Established {
        pcscf: String,
    },
    /// The process died before establishing — the line is skipped (matches
    /// the current script's `return 1` from `start_line_strongswan`/
    /// `start_line_swu`).
    FatalProcessDied,
    /// swu only: `SWU_MAX_ESTABLISH_ATTEMPTS` reached without connecting.
    FatalTimedOut,
}

/// Advances the establish-time state machine by one tick. 1:1 port of both
/// engines' establish-time `while`/`for` loop bodies (see module docs).
pub fn tick_establishing(
    engine: &dyn TunnelEngine,
    runner: &dyn CommandRunner,
    attempt: &mut u32,
    stuck_without_pcscf: &mut bool,
) -> EstablishOutcome {
    if engine.is_tunnel_established(runner) {
        if let Some(pcscf) = engine.latest_pcscf(runner) {
            return EstablishOutcome::Established { pcscf };
        }
        *stuck_without_pcscf = true;
    } else {
        *stuck_without_pcscf = false;
    }

    if !engine.is_process_alive(runner) {
        return EstablishOutcome::FatalProcessDied;
    }

    *attempt += 1;

    if let Some(max) = engine.max_establish_attempts() {
        if *attempt >= max {
            return EstablishOutcome::FatalTimedOut;
        }
    }

    if let Some(cadence) = engine.reinitiate_cadence() {
        if (*attempt).is_multiple_of(cadence) {
            // Stuck-with-an-established-SA-but-no-P-CSCF gets a full
            // terminate before re-initiating fresh; a plain "still waiting"
            // just re-initiates on top of whatever's there.
            if *stuck_without_pcscf {
                engine.terminate(runner);
            }
            engine.reinitiate(runner);
        }
    }

    EstablishOutcome::StillEstablishing
}

/// Outcome of one steady-state tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteadyOutcome {
    StillUp,
    /// The active connection changed which P-CSCF it reports; the caller
    /// should refresh the P-CSCF source file and restart this line's
    /// vowifi-ims-agent only.
    PcscfChanged {
        new_pcscf: String,
    },
    /// The process/tunnel was unhealthy and a recovery action was already
    /// issued by the time this returns.
    Recovered {
        reason: DegradeReason,
    },
}

/// Advances the steady-state state machine by one tick. 1:1 port of both
/// engines' steady-state supervisor loop bodies (see module docs) — the
/// order of checks matches the bash exactly: process-alive first, then
/// engine-specific health, then P-CSCF-change last (a rekey can change the
/// P-CSCF without any of the earlier checks ever firing).
pub fn tick_steady_state(
    engine: &dyn TunnelEngine,
    runner: &dyn CommandRunner,
    current_pcscf: &str,
) -> SteadyOutcome {
    if !engine.is_process_alive(runner) {
        engine.restart_process(runner);
        return SteadyOutcome::Recovered {
            reason: DegradeReason::ProcessDied,
        };
    }

    match engine.steady_state_health(runner) {
        SteadyStateHealth::ViciBroken => {
            engine.restart_process(runner);
            return SteadyOutcome::Recovered {
                reason: DegradeReason::ViciBroken,
            };
        }
        SteadyStateHealth::TunVanished => {
            // The current script also recreates the XFRM interface here
            // (`ensure_epdg_interface`) before terminate+reinitiate — that
            // idempotent netns/interface setup is orchestrated by the
            // caller (supervise::mod, Phase 4), which already owns
            // `ensure_epdg_interface`'s equivalent for initial setup; this
            // tick only issues the terminate+reinitiate strongSwan expects
            // once the interface is back.
            engine.terminate(runner);
            engine.reinitiate(runner);
            return SteadyOutcome::Recovered {
                reason: DegradeReason::TunVanished,
            };
        }
        SteadyStateHealth::ChildSaMissing => {
            // Re-initiate only — the current script does NOT restart this
            // line's vowifi-ims-agent for this branch alone, unlike the
            // other three recovery paths.
            engine.reinitiate(runner);
            return SteadyOutcome::Recovered {
                reason: DegradeReason::ChildSaMissing,
            };
        }
        SteadyStateHealth::Ok => {}
    }

    if let Some(latest) = engine.latest_pcscf(runner) {
        if latest != current_pcscf {
            return SteadyOutcome::PcscfChanged { new_pcscf: latest };
        }
    }

    SteadyOutcome::StillUp
}

/// Whether a [`SteadyOutcome::Recovered`] reason also means "restart this
/// line's vowifi-ims-agent" — matches the current script exactly: every
/// recovery path does, EXCEPT a bare `ChildSaMissing` re-initiate.
pub fn recovery_restarts_agent(reason: DegradeReason) -> bool {
    !matches!(reason, DegradeReason::ChildSaMissing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::runner::MockCommandRunner;
    use std::cell::RefCell;

    // MOCK JUSTIFICATION (constitution Principle I): TunnelEngine's real
    // implementations (StrongswanEngine/SwuEngine) drive real charon/pcscd/
    // swanctl/a live modem — none available in CI. This fake engine lets the
    // *state machine* (tick_establishing/tick_steady_state) be tested against
    // every transition in data-model.md's table directly, independent of
    // engine wiring.
    struct FakeEngine {
        established: RefCell<bool>,
        pcscf: RefCell<Option<String>>,
        alive: RefCell<bool>,
        health: RefCell<SteadyStateHealth>,
        max_attempts: Option<u32>,
        reinitiate_cadence: Option<u32>,
        terminate_calls: RefCell<u32>,
        reinitiate_calls: RefCell<u32>,
        restart_calls: RefCell<u32>,
    }

    impl Default for FakeEngine {
        fn default() -> Self {
            Self {
                established: RefCell::new(false),
                pcscf: RefCell::new(None),
                alive: RefCell::new(true),
                health: RefCell::new(SteadyStateHealth::Ok),
                max_attempts: None,
                reinitiate_cadence: Some(STRONGSWAN_REINITIATE_EVERY),
                terminate_calls: RefCell::new(0),
                reinitiate_calls: RefCell::new(0),
                restart_calls: RefCell::new(0),
            }
        }
    }

    impl TunnelEngine for FakeEngine {
        fn is_tunnel_established(&self, _runner: &dyn CommandRunner) -> bool {
            *self.established.borrow()
        }
        fn latest_pcscf(&self, _runner: &dyn CommandRunner) -> Option<String> {
            self.pcscf.borrow().clone()
        }
        fn is_process_alive(&self, _runner: &dyn CommandRunner) -> bool {
            *self.alive.borrow()
        }
        fn terminate(&self, _runner: &dyn CommandRunner) {
            *self.terminate_calls.borrow_mut() += 1;
        }
        fn reinitiate(&self, _runner: &dyn CommandRunner) {
            *self.reinitiate_calls.borrow_mut() += 1;
        }
        fn steady_state_health(&self, _runner: &dyn CommandRunner) -> SteadyStateHealth {
            *self.health.borrow()
        }
        fn restart_process(&self, _runner: &dyn CommandRunner) {
            *self.restart_calls.borrow_mut() += 1;
        }
        fn max_establish_attempts(&self) -> Option<u32> {
            self.max_attempts
        }
        fn reinitiate_cadence(&self) -> Option<u32> {
            self.reinitiate_cadence
        }
    }

    #[test]
    fn establishing_with_a_child_sa_but_no_pcscf_yet_keeps_waiting_and_marks_stuck() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            established: RefCell::new(true),
            pcscf: RefCell::new(None),
            ..Default::default()
        };
        let mut attempt = 0;
        let mut stuck = false;
        let outcome = tick_establishing(&engine, &runner, &mut attempt, &mut stuck);
        assert_eq!(outcome, EstablishOutcome::StillEstablishing);
        assert!(
            stuck,
            "CHILD_SA established but no P-CSCF must set stuck_without_pcscf"
        );
    }

    #[test]
    fn establishing_with_child_sa_and_pcscf_transitions_to_established() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            established: RefCell::new(true),
            pcscf: RefCell::new(Some("10.0.0.1".to_string())),
            ..Default::default()
        };
        let mut attempt = 0;
        let mut stuck = false;
        let outcome = tick_establishing(&engine, &runner, &mut attempt, &mut stuck);
        assert_eq!(
            outcome,
            EstablishOutcome::Established {
                pcscf: "10.0.0.1".to_string()
            }
        );
    }

    #[test]
    fn establishing_process_death_is_fatal_for_this_line() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            alive: RefCell::new(false),
            ..Default::default()
        };
        let mut attempt = 0;
        let mut stuck = false;
        assert_eq!(
            tick_establishing(&engine, &runner, &mut attempt, &mut stuck),
            EstablishOutcome::FatalProcessDied
        );
    }

    #[test]
    fn strongswan_reinitiates_every_15_attempts_while_still_waiting() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine::default();
        let mut attempt = 0;
        let mut stuck = false;
        for _ in 0..14 {
            tick_establishing(&engine, &runner, &mut attempt, &mut stuck);
        }
        assert_eq!(
            *engine.reinitiate_calls.borrow(),
            0,
            "no reinitiate before the 15th attempt"
        );
        tick_establishing(&engine, &runner, &mut attempt, &mut stuck);
        assert_eq!(attempt, 15);
        assert_eq!(*engine.reinitiate_calls.borrow(), 1);
        assert_eq!(
            *engine.terminate_calls.borrow(),
            0,
            "no terminate when not stuck-without-pcscf"
        );
    }

    #[test]
    fn strongswan_stuck_without_pcscf_terminates_before_reinitiating_at_the_cadence() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            established: RefCell::new(true), // CHILD_SA up, but pcscf stays None
            ..Default::default()
        };
        let mut attempt = 0;
        let mut stuck = false;
        for _ in 0..STRONGSWAN_REINITIATE_EVERY {
            tick_establishing(&engine, &runner, &mut attempt, &mut stuck);
        }
        assert_eq!(*engine.terminate_calls.borrow(), 1);
        assert_eq!(*engine.reinitiate_calls.borrow(), 1);
    }

    #[test]
    fn swu_never_reinitiates_in_place_only_counts_toward_its_own_timeout() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            max_attempts: Some(SWU_MAX_ESTABLISH_ATTEMPTS),
            reinitiate_cadence: None,
            ..Default::default()
        };
        let mut attempt = 0;
        let mut stuck = false;
        for _ in 0..SWU_MAX_ESTABLISH_ATTEMPTS - 1 {
            let outcome = tick_establishing(&engine, &runner, &mut attempt, &mut stuck);
            assert_eq!(outcome, EstablishOutcome::StillEstablishing);
        }
        assert_eq!(*engine.reinitiate_calls.borrow(), 0);
        let final_outcome = tick_establishing(&engine, &runner, &mut attempt, &mut stuck);
        assert_eq!(final_outcome, EstablishOutcome::FatalTimedOut);
    }

    #[test]
    fn steady_state_dead_process_is_restarted() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            alive: RefCell::new(false),
            ..Default::default()
        };
        let outcome = tick_steady_state(&engine, &runner, "10.0.0.1");
        assert_eq!(
            outcome,
            SteadyOutcome::Recovered {
                reason: DegradeReason::ProcessDied
            }
        );
        assert_eq!(*engine.restart_calls.borrow(), 1);
    }

    #[test]
    fn steady_state_vici_broken_restarts_the_process() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::ViciBroken),
            ..Default::default()
        };
        let outcome = tick_steady_state(&engine, &runner, "10.0.0.1");
        assert_eq!(
            outcome,
            SteadyOutcome::Recovered {
                reason: DegradeReason::ViciBroken
            }
        );
        assert_eq!(*engine.restart_calls.borrow(), 1);
    }

    #[test]
    fn steady_state_tun_vanished_terminates_then_reinitiates_not_a_full_restart() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::TunVanished),
            ..Default::default()
        };
        let outcome = tick_steady_state(&engine, &runner, "10.0.0.1");
        assert_eq!(
            outcome,
            SteadyOutcome::Recovered {
                reason: DegradeReason::TunVanished
            }
        );
        assert_eq!(*engine.terminate_calls.borrow(), 1);
        assert_eq!(*engine.reinitiate_calls.borrow(), 1);
        assert_eq!(
            *engine.restart_calls.borrow(),
            0,
            "must not be a full process restart"
        );
    }

    #[test]
    fn steady_state_child_sa_missing_only_reinitiates_no_terminate_no_restart() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::ChildSaMissing),
            ..Default::default()
        };
        let outcome = tick_steady_state(&engine, &runner, "10.0.0.1");
        assert_eq!(
            outcome,
            SteadyOutcome::Recovered {
                reason: DegradeReason::ChildSaMissing
            }
        );
        assert_eq!(*engine.reinitiate_calls.borrow(), 1);
        assert_eq!(*engine.terminate_calls.borrow(), 0);
        assert_eq!(*engine.restart_calls.borrow(), 0);
    }

    #[test]
    fn steady_state_pcscf_change_is_reported_without_any_recovery_action() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf: RefCell::new(Some("10.0.0.9".to_string())),
            ..Default::default()
        };
        let outcome = tick_steady_state(&engine, &runner, "10.0.0.1");
        assert_eq!(
            outcome,
            SteadyOutcome::PcscfChanged {
                new_pcscf: "10.0.0.9".to_string()
            }
        );
        assert_eq!(*engine.restart_calls.borrow(), 0);
        assert_eq!(*engine.reinitiate_calls.borrow(), 0);
    }

    #[test]
    fn steady_state_unchanged_pcscf_and_healthy_process_is_a_no_op_tick() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf: RefCell::new(Some("10.0.0.1".to_string())),
            ..Default::default()
        };
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1"),
            SteadyOutcome::StillUp
        );
    }

    #[test]
    fn only_child_sa_missing_skips_the_agent_restart() {
        assert!(!recovery_restarts_agent(DegradeReason::ChildSaMissing));
        for reason in [
            DegradeReason::ProcessDied,
            DegradeReason::ViciBroken,
            DegradeReason::TunVanished,
            DegradeReason::PcscfChanged,
        ] {
            assert!(
                recovery_restarts_agent(reason),
                "{reason:?} should restart the agent"
            );
        }
    }
}
