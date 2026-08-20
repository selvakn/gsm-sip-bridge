//! One per-line VoWiFi tunnel supervisor, generic over the tunnel engine
//! (specs/021-entrypoint-supervise-rust Phase 3) — collapses the three
//! duplicated bash loops (strongswan establish-time, strongswan steady-state,
//! swu establish+steady-state) into one state machine (FR-005). Every
//! transition and its recovery action is a 1:1 port of the original script's
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
    /// The tunnel interface is missing *and* could not be recreated, so this
    /// line has no data path and cannot be given one right now — distinct
    /// from `TunVanished`, which is the same symptom with a recreation that
    /// worked. Nothing downstream of the interface is worth disturbing until
    /// it exists: see the TunVanished branch of [`tick_steady_state`].
    TunUnavailable,
    ChildSaMissing,
    PcscfChanged,
    /// The P-CSCF became unreachable from inside this line's namespace for
    /// several consecutive ticks, while every structural check still passed
    /// (specs/039-at-stall-watchdog).
    ///
    /// This is the 2026-08-17 outage: after the carrier moved the P-CSCF, the
    /// tunnel re-established -- IKE SA up, CHILD_SA installed, interface
    /// present -- but the `default dev tun23-0` route was never reinstalled.
    /// So there was no data path, the inbound SA carried 0 bytes, and the
    /// agent got `ENETUNREACH` and exited every 6 seconds for **8 hours**.
    /// Every structural check passed the whole time, because none of them
    /// looks at whether anything can actually be reached.
    PcscfUnreachable,
    /// The line's netns had no default route through its tunnel, and one has
    /// been reinstated (specs/039-at-stall-watchdog).
    ///
    /// The 2026-08-19 outage: a 2-minute WAN blip (a scheduled router reboot)
    /// tore the CHILD_SA down, charon's `down-client` removed the carrier
    /// address, and the kernel deleted the interface's default route along with
    /// it. The reconnect restored the address but nothing restored the route, so
    /// every SIP connect got `ENETUNREACH` for six hours while
    /// `swanctl --list-sas` reported a healthy tunnel throughout.
    ///
    /// Distinct from [`Self::PcscfUnreachable`] because the remedy is completely
    /// different — one `ip route replace`, no renegotiation, no dropped call —
    /// and because a rebuild provably cannot fix it.
    DefaultRouteMissing,
}

/// Steady-state-only health, beyond "is the process alive" and "is a P-CSCF
/// change pending" (checked separately) — the three strongswan-specific
/// checks (`tun_iface` presence, `swanctl --list-sas`'s exit status, the
/// `ims:` CHILD_SA line's presence) collapse to `Ok` for the swu engine,
/// which the original script's own comment says has "no re-initiate-in-place
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
/// Matches the strongswan steady-state loop's `sleep 30` cadence.
pub const STEADY_STATE_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// The swu steady-state loop's own, much faster cadence.
///
/// Named rather than inline in `orchestrate` because the reachability window
/// below is derived from it: the two engines poll six times apart, so anything
/// expressed in ticks means two different things depending on which loop is
/// running it.
pub const SWU_STEADY_STATE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long the P-CSCF must stay unreachable before the tunnel is rebuilt.
///
/// A *duration*, not a tick count. This was three ticks, which gives ~90s on the
/// strongswan loop but only ~15s on the swu loop — so on swu a routine 15s
/// bearer blip tore down a working tunnel and restarted the agent, while the
/// doc comment and the real-time bound test both described the 90s figure.
///
/// 90s because a genuine loss of the data path should be repaired in about a
/// minute and a half rather than never, and because a single failed TCP connect
/// is a routine thing on a mobile bearer — one dropped packet during a rekey
/// must not be enough.
pub const PCSCF_UNREACHABLE_WINDOW: Duration = Duration::from_secs(90);

/// However fast a loop polls, never act on fewer than this many consecutive
/// failures. A time window alone would let an arbitrarily slow poll act on one
/// sample, and one sample is exactly what is too noisy to trust.
pub const PCSCF_UNREACHABLE_MIN_STRIKES: u32 = 2;

/// Consecutive unreachable ticks that cover [`PCSCF_UNREACHABLE_WINDOW`] at
/// `poll_interval`.
pub fn pcscf_unreachable_strikes(poll_interval: Duration) -> u32 {
    let per_tick = poll_interval.as_secs().max(1);
    let ticks = PCSCF_UNREACHABLE_WINDOW.as_secs().div_ceil(per_tick);
    u32::try_from(ticks)
        .unwrap_or(u32::MAX)
        .max(PCSCF_UNREACHABLE_MIN_STRIKES)
}

/// SIP port probed on the P-CSCF -- the thing a line's data path has to be able
/// to reach for the line to be worth anything.
pub const PCSCF_SIP_PORT: u16 = 5060;

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
    /// (strongswan: `swanctl --terminate --ike <this line's connection>`;
    /// swu: no-op — no in-place terminate exists for this engine). Scoped to
    /// one connection: every line's connection lives in one shared charon, so
    /// a bare `ims` would tear down every line at once.
    fn terminate(&self, runner: &dyn CommandRunner);
    /// (Re)initiates a connection attempt without restarting the process
    /// (strongswan: `swanctl --initiate --child <this line's connection>`;
    /// swu: no-op, matching `terminate`). Scoped for the same reason.
    fn reinitiate(&self, runner: &dyn CommandRunner);
    /// Steady-state-only health beyond process-alive/P-CSCF-change.
    fn steady_state_health(&self, runner: &dyn CommandRunner) -> SteadyStateHealth;
    /// Recreates this line's tunnel interface after `SteadyStateHealth::
    /// TunVanished` — 1:1 port of the original script's own comment: "tun can
    /// vanish from the kernel entirely while swanctl still reports the
    /// CHILD_SA ESTABLISHED/INSTALLED (observed live, specs/012-strongswan-
    /// epdg) — recreate ... rather than trusting the desynced SA." Found
    /// missing live: an earlier version of this port detected TunVanished
    /// correctly but never recreated the interface before re-initiating,
    /// so every subsequent negotiation kept succeeding at the IKE/CHILD_SA
    /// level while silently failing to install a working data path — a
    /// fresh IKE_SA every ~30s (one steady-state tick), forever. No-op for
    /// swu, which has no equivalent pre-created interface concept.
    ///
    /// Returns whether the interface is present afterwards. `false` means no
    /// data path can exist for this line right now whatever else is done to
    /// it, so the caller skips the terminate/reinitiate that would otherwise
    /// follow — see the TunVanished branch of [`tick_steady_state`] for why
    /// that churn is worse than waiting. Always `true` for swu, which has no
    /// interface to be missing.
    fn recreate_interface(&self, runner: &dyn CommandRunner) -> bool;
    /// Reinstate this line's default route if it is missing, returning whether
    /// it had to (`true` = it was missing and has now been repaired).
    ///
    /// Separate from [`TunnelEngine::recreate_interface`] because it is the
    /// cheapest possible repair — one `ip route replace`, no disruption —
    /// whereas a rebuild costs a renegotiation and drops any call. A route that
    /// has gone missing is also *the* observed cause of a line that is
    /// structurally perfect and completely unreachable, so it is worth ruling
    /// out before tearing anything down. Always `false` for swu, which manages
    /// its own device and routing through the dialer.
    fn repair_default_route(&self, runner: &dyn CommandRunner) -> bool;
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

    /// Can this line's P-CSCF actually be reached, from inside this line's
    /// namespace, right now?
    ///
    /// The only check here that tests the *data path* rather than its
    /// structure. Everything else asks whether the pieces exist; this asks
    /// whether they carry traffic, which is the question the 2026-08-17 outage
    /// went 8 hours without anyone asking.
    fn pcscf_reachable(&self, runner: &dyn CommandRunner, pcscf: &str) -> bool;
}

/// Outcome of one establish-time tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EstablishOutcome {
    StillEstablishing,
    Established {
        pcscf: String,
    },
    /// The process died before establishing — the line is skipped (matches
    /// the original script's `return 1` from `start_line_strongswan`/
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
    unreachable_streak: &mut u32,
    poll_interval: Duration,
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
            // 1:1 port of the original script's own recovery: recreate the
            // XFRM interface (idempotent — matches `ensure_epdg_interface`)
            // BEFORE terminate+reinitiate. Missing this call was a real bug
            // found live: without it, every subsequent negotiation kept
            // succeeding at the IKE/CHILD_SA level while silently failing to
            // install a working data path onto an interface that was never
            // actually recreated — a fresh IKE_SA every steady-state tick,
            // forever, never actually fixing the underlying problem.
            //
            // The terminate+reinitiate is conditional on that recreation
            // having actually worked. Measured live 2026-07-31: when the
            // container is replaced, the previous run's `ims<N>` namespaces
            // — and the `tun23-<N>` devices inside them, which register their
            // `if_id` in the namespace they were *created* in, the host's —
            // survive about two and a half minutes. Nothing shortens it: the
            // shutdown plan's `ip netns del` runs, the container exits 0, and
            // a stopped deployment still held both ids 2m29s later. Until the
            // kernel reaps them the id is refused and no interface can exist,
            // so terminating each tick only churned the carrier without ever
            // getting closer to a data path: eight and six IKE_SA setups
            // across two startups on the two lines, against two and two once
            // this branch stopped doing it. Waiting is free by comparison: a
            // line with no tunnel interface carries no traffic either way,
            // and the first tick after the id frees recreates it and recovers
            // normally — an 11s startup once the old namespaces are gone,
            // against 163s and 195s when they are not.
            if !engine.recreate_interface(runner) {
                return SteadyOutcome::Recovered {
                    reason: DegradeReason::TunUnavailable,
                };
            }
            engine.terminate(runner);
            engine.reinitiate(runner);
            return SteadyOutcome::Recovered {
                reason: DegradeReason::TunVanished,
            };
        }
        SteadyStateHealth::ChildSaMissing => {
            // Re-initiate only — the original script does NOT restart this
            // line's vowifi-ims-agent for this branch alone, unlike the
            // other three recovery paths.
            engine.reinitiate(runner);
            return SteadyOutcome::Recovered {
                reason: DegradeReason::ChildSaMissing,
            };
        }
        SteadyStateHealth::Ok => {}
    }

    // Reachability is checked last, and deliberately *after* the P-CSCF-changed
    // branch above: when the carrier moves the P-CSCF, the old address becoming
    // unreachable is expected, and refreshing to the new one is the right
    // remedy rather than rebuilding the tunnel.
    if let Some(latest) = engine.latest_pcscf(runner) {
        if latest != current_pcscf {
            return SteadyOutcome::PcscfChanged { new_pcscf: latest };
        }
    }

    if engine.pcscf_reachable(runner, current_pcscf) {
        *unreachable_streak = 0;
    } else {
        // Before escalating: is the route simply gone? That is a millisecond
        // repair with no disruption, against a rebuild that renegotiates the SA
        // and drops any call — and it is the one fault that presents exactly
        // like this, with every structural check passing.
        //
        // It is also what a rebuild cannot fix. The kernel deletes an
        // interface's default route when the last address of that family goes,
        // so the `terminate` below destroys the route `recreate_interface`
        // installs moments earlier. On 2026-08-19 that loop ran 202 times over
        // six hours without ever restoring the data path.
        if engine.repair_default_route(runner) {
            *unreachable_streak = 0;
            return SteadyOutcome::Recovered {
                reason: DegradeReason::DefaultRouteMissing,
            };
        }
        *unreachable_streak = unreachable_streak.saturating_add(1);
        // Derived from this loop's own cadence, so both engines wait the same
        // ~90s of real time rather than the same number of ticks.
        if *unreachable_streak >= pcscf_unreachable_strikes(poll_interval) {
            *unreachable_streak = 0;
            // The same remedy as `TunVanished`, and for the same underlying
            // reason: there is no working data path. `recreate_interface` runs
            // first and the rebuild is conditional on it, exactly as there —
            // a terminate+reinitiate that lands on an interface with no route
            // just churns the carrier and negotiates another SA that carries
            // nothing, which is precisely the state this recovers from.
            if !engine.recreate_interface(runner) {
                return SteadyOutcome::Recovered {
                    reason: DegradeReason::TunUnavailable,
                };
            }
            engine.terminate(runner);
            engine.reinitiate(runner);
            return SteadyOutcome::Recovered {
                reason: DegradeReason::PcscfUnreachable,
            };
        }
    }

    SteadyOutcome::StillUp
}

/// Whether a [`SteadyOutcome::Recovered`] reason also means "restart this
/// line's vowifi-ims-agent" — matches the original script exactly: every
/// recovery path does, EXCEPT a bare `ChildSaMissing` re-initiate.
///
/// `TunUnavailable` is the one addition to that rule. The agent's whole job is
/// to route over a tunnel interface which, in that state, does not exist and
/// cannot be made to exist yet, so killing it only guarantees it fails the same
/// way on restart.
///
/// Worth knowing what this does *not* buy: measured live 2026-07-31, the agent
/// restart count over a container replacement did not move (48 across two
/// startups, before and after). Nearly all of those are the agent's own
/// crash-loop — it starts, cannot reach its P-CSCF without an interface, exits,
/// and its supervisor restarts it 5s later — which this does not touch. The
/// tick-driven kills removed here are a handful on top of that. The change is
/// kept because killing a process over a condition it cannot affect is wrong on
/// its own terms, not because it measurably improved anything.
pub fn recovery_restarts_agent(reason: DegradeReason) -> bool {
    !matches!(
        reason,
        DegradeReason::ChildSaMissing | DegradeReason::TunUnavailable
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::runner::MockCommandRunner;
    use std::cell::RefCell;

    /// `tick_steady_state` at the strongswan cadence, which is what almost every
    /// test here means. Shadows the real one so the cases that do not care about
    /// the poll interval stay readable; the ones that *do* care call
    /// `super::tick_steady_state` explicitly with the interval under test.
    fn tick_steady_state(
        engine: &dyn TunnelEngine,
        runner: &dyn CommandRunner,
        current_pcscf: &str,
        unreachable_streak: &mut u32,
    ) -> SteadyOutcome {
        super::tick_steady_state(
            engine,
            runner,
            current_pcscf,
            unreachable_streak,
            STEADY_STATE_POLL_INTERVAL,
        )
    }

    // MOCK JUSTIFICATION (constitution Principle I): TunnelEngine's real
    // implementations (StrongswanEngine/SwuEngine) drive real charon/pcscd/
    // swanctl/a live modem — none available in CI. This fake engine lets the
    // *state machine* (tick_establishing/tick_steady_state) be tested against
    // every transition in data-model.md's table directly, independent of
    // engine wiring.
    struct FakeEngine {
        /// Whether the data path works. Separate from every structural field
        /// below, because the whole point is that structure can be perfect
        /// while nothing is reachable.
        pcscf_reachable: RefCell<bool>,
        /// Whether this line's default route has gone missing. Separate from
        /// `pcscf_reachable` because the whole point is that a missing route is
        /// one specific *cause* of unreachability with its own cheap remedy.
        route_missing: RefCell<bool>,
        route_repairs: RefCell<u32>,
        established: RefCell<bool>,
        pcscf: RefCell<Option<String>>,
        alive: RefCell<bool>,
        health: RefCell<SteadyStateHealth>,
        max_attempts: Option<u32>,
        reinitiate_cadence: Option<u32>,
        terminate_calls: RefCell<u32>,
        reinitiate_calls: RefCell<u32>,
        restart_calls: RefCell<u32>,
        recreate_interface_calls: RefCell<u32>,
        /// Whether `recreate_interface` reports the interface as present
        /// afterwards — `false` models a line whose `if_id` is still held by
        /// a not-yet-reaped previous run.
        recreate_ok: bool,
        /// Recovery calls in the order they were issued. Counts alone cannot
        /// express the ordering the TunVanished branch depends on.
        call_order: RefCell<Vec<&'static str>>,
    }

    impl Default for FakeEngine {
        fn default() -> Self {
            Self {
                established: RefCell::new(false),
                pcscf: RefCell::new(None),
                // Default reachable: every pre-existing test asserts on the
                // structural checks and must keep passing unchanged.
                pcscf_reachable: RefCell::new(true),
                // Default present: an unreachable P-CSCF in a pre-existing test
                // means "unreachable for some other reason", so the route repair
                // must not silently absorb those cases.
                route_missing: RefCell::new(false),
                route_repairs: RefCell::new(0),
                alive: RefCell::new(true),
                health: RefCell::new(SteadyStateHealth::Ok),
                max_attempts: None,
                reinitiate_cadence: Some(STRONGSWAN_REINITIATE_EVERY),
                terminate_calls: RefCell::new(0),
                reinitiate_calls: RefCell::new(0),
                restart_calls: RefCell::new(0),
                recreate_interface_calls: RefCell::new(0),
                recreate_ok: true,
                call_order: RefCell::new(Vec::new()),
            }
        }
    }

    impl TunnelEngine for FakeEngine {
        fn pcscf_reachable(&self, _runner: &dyn CommandRunner, _pcscf: &str) -> bool {
            *self.pcscf_reachable.borrow()
        }

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
            self.call_order.borrow_mut().push("terminate");
        }
        fn reinitiate(&self, _runner: &dyn CommandRunner) {
            *self.reinitiate_calls.borrow_mut() += 1;
            self.call_order.borrow_mut().push("reinitiate");
        }
        fn repair_default_route(&self, _runner: &dyn CommandRunner) -> bool {
            if !*self.route_missing.borrow() {
                return false;
            }
            // Repaired: the route is back, and with it reachability. Modelling
            // that here is the point -- a missing route is a *cause* of
            // unreachability, so fixing it must clear the symptom too.
            *self.route_missing.borrow_mut() = false;
            *self.pcscf_reachable.borrow_mut() = true;
            *self.route_repairs.borrow_mut() += 1;
            self.call_order.borrow_mut().push("repair_route");
            true
        }
        fn steady_state_health(&self, _runner: &dyn CommandRunner) -> SteadyStateHealth {
            *self.health.borrow()
        }
        fn restart_process(&self, _runner: &dyn CommandRunner) {
            *self.restart_calls.borrow_mut() += 1;
        }
        fn recreate_interface(&self, _runner: &dyn CommandRunner) -> bool {
            *self.recreate_interface_calls.borrow_mut() += 1;
            self.call_order.borrow_mut().push("recreate_interface");
            self.recreate_ok
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
        let outcome = tick_steady_state(&engine, &runner, "10.0.0.1", &mut 0);
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
        let outcome = tick_steady_state(&engine, &runner, "10.0.0.1", &mut 0);
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
        let outcome = tick_steady_state(&engine, &runner, "10.0.0.1", &mut 0);
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
        assert_eq!(
            *engine.recreate_interface_calls.borrow(),
            1,
            "regression test: an earlier version of this port detected \
             TunVanished but never recreated the interface, so every \
             subsequent negotiation kept succeeding at the IKE/CHILD_SA \
             level while silently failing to install a working data path"
        );
    }

    #[test]
    fn steady_state_tun_vanished_recreates_before_it_terminates_and_reinitiates() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::TunVanished),
            ..Default::default()
        };
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut 0);
        assert_eq!(
            *engine.call_order.borrow(),
            vec!["recreate_interface", "terminate", "reinitiate"],
            "the original script's ordering: the interface must exist before \
             the renegotiation that installs a data path onto it"
        );
    }

    #[test]
    fn steady_state_tun_vanished_leaves_the_sa_alone_when_the_interface_cannot_be_recreated() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::TunVanished),
            recreate_ok: false,
            ..Default::default()
        };
        let outcome = tick_steady_state(&engine, &runner, "10.0.0.1", &mut 0);
        assert_eq!(
            outcome,
            SteadyOutcome::Recovered {
                reason: DegradeReason::TunUnavailable
            },
            "a distinct reason, so the caller can tell 'recreated it' from \
             'could not', and skip the agent restart for the latter"
        );
        assert!(!recovery_restarts_agent(DegradeReason::TunUnavailable));
        assert_eq!(*engine.recreate_interface_calls.borrow(), 1);
        assert_eq!(
            (
                *engine.terminate_calls.borrow(),
                *engine.reinitiate_calls.borrow()
            ),
            (0, 0),
            "measured live 2026-07-31: when a container is replaced, the \
             previous run's namespaces hold this line's if_id for ~2.5min and \
             nothing shortens that, so no interface can exist yet. Tearing the \
             SA down every 30s through that window only churns the carrier \
             (eight and six IKE_SA setups across two startups, against two and \
             two after this gate) and cannot produce a data path — the tick \
             after the id frees recovers normally"
        );
    }

    #[test]
    fn an_unreachable_pcscf_rebuilds_the_tunnel_after_three_strikes() {
        // The 2026-08-17 outage, as a test. Every structural check passes --
        // process alive, vici fine, interface present, CHILD_SA installed,
        // P-CSCF unchanged -- and nothing can be reached, because the route
        // through the tunnel was never reinstalled. That state lasted 8 hours
        // and the agent exited 4804 times, because no check asked whether the
        // data path carried traffic.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf_reachable: RefCell::new(false),
            ..Default::default()
        };
        let mut streak = 0;

        // A single failure is routine on a mobile bearer -- one dropped packet
        // during a rekey must not tear down a working tunnel.
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::StillUp
        );
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::StillUp
        );
        assert_eq!(*engine.reinitiate_calls.borrow(), 0, "no churn yet");

        // Sustained, though, means the data path is genuinely gone.
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::Recovered {
                reason: DegradeReason::PcscfUnreachable
            }
        );
        // Same remedy as TunVanished, and in the same order: recreate the
        // interface first, then rebuild the negotiation onto it.
        assert_eq!(*engine.recreate_interface_calls.borrow(), 1);
        assert_eq!(*engine.terminate_calls.borrow(), 1);
        assert_eq!(*engine.reinitiate_calls.borrow(), 1);
        assert_eq!(streak, 0, "the streak resets after acting");
    }

    #[test]
    fn a_recovered_reachability_resets_the_streak_before_it_ever_acts() {
        // A flapping bearer must never accumulate its way to a teardown.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine::default();
        let mut streak = 0;
        for _ in 0..10 {
            *engine.pcscf_reachable.borrow_mut() = false;
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak);
            *engine.pcscf_reachable.borrow_mut() = true;
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak);
        }
        assert_eq!(streak, 0);
        assert_eq!(
            *engine.reinitiate_calls.borrow(),
            0,
            "alternating reachability is not a sustained loss and must not rebuild"
        );
    }

    #[test]
    fn a_reachable_pcscf_is_still_up_and_touches_nothing() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine::default();
        let mut streak = 0;
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::StillUp
        );
        assert_eq!(*engine.terminate_calls.borrow(), 0);
        assert_eq!(*engine.reinitiate_calls.borrow(), 0);
        assert_eq!(*engine.restart_calls.borrow(), 0);
    }

    #[test]
    fn a_changed_pcscf_is_refreshed_rather_than_rebuilt_even_when_unreachable() {
        // Ordering matters: when the carrier moves the P-CSCF, the *old*
        // address going unreachable is expected. Refreshing to the new one is
        // the cheap correct remedy; rebuilding the tunnel would be churn. This
        // is the exact sequence that preceded the outage.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf: RefCell::new(Some("10.0.0.2".to_string())),
            pcscf_reachable: RefCell::new(false),
            ..Default::default()
        };
        let mut streak = 0;
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::PcscfChanged {
                new_pcscf: "10.0.0.2".to_string()
            }
        );
        assert_eq!(streak, 0, "a refresh must not count as an unreachable tick");
        assert_eq!(*engine.reinitiate_calls.borrow(), 0);
    }

    #[test]
    fn a_missing_default_route_is_repaired_without_touching_the_tunnel() {
        // The 2026-08-19 outage. A 2-minute WAN blip (a scheduled router reboot)
        // tore the CHILD_SA down; charon's `down-client` removed the carrier
        // address, and the kernel deleted the interface's default route with it.
        // The reconnect restored the address and nothing restored the route, so
        // every SIP connect got ENETUNREACH for six hours while every structural
        // check -- process alive, VICI fine, interface present, CHILD_SA
        // installed -- passed.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf_reachable: RefCell::new(false),
            route_missing: RefCell::new(true),
            ..Default::default()
        };
        let mut streak = 0;

        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::Recovered {
                reason: DegradeReason::DefaultRouteMissing
            },
            "a missing route must be named as such, not inferred from unreachability"
        );
        assert_eq!(*engine.route_repairs.borrow(), 1);
        // The whole point: no renegotiation. A rebuild costs a dropped call and
        // -- as the outage proved -- cannot fix this anyway.
        assert_eq!(*engine.terminate_calls.borrow(), 0);
        assert_eq!(*engine.reinitiate_calls.borrow(), 0);
        assert_eq!(*engine.recreate_interface_calls.borrow(), 0);
        assert_eq!(streak, 0, "a repaired route must not count as a strike");

        // And the line is well again on the next tick.
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::StillUp
        );
    }

    #[test]
    fn unreachable_with_the_route_present_still_escalates_to_a_rebuild() {
        // The route repair must not swallow the fault it does not explain --
        // otherwise it becomes a way to report success while the line stays dead.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf_reachable: RefCell::new(false),
            route_missing: RefCell::new(false),
            ..Default::default()
        };
        let mut streak = 0;
        let strikes = pcscf_unreachable_strikes(STEADY_STATE_POLL_INTERVAL);
        let mut rebuilt = false;
        for _ in 0..strikes {
            if let SteadyOutcome::Recovered {
                reason: DegradeReason::PcscfUnreachable,
            } = tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak)
            {
                rebuilt = true;
            }
        }
        assert!(rebuilt, "a genuinely dead data path must still be rebuilt");
        assert_eq!(*engine.route_repairs.borrow(), 0);
    }

    #[test]
    fn a_reachable_line_is_never_probed_for_its_route() {
        // Cheap as the repair is, it is still two `ip` invocations per line per
        // tick. A healthy line must not pay for them.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine::default();
        let mut streak = 0;
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::StillUp
        );
        assert!(!engine.call_order.borrow().contains(&"repair_route"));
    }

    #[test]
    fn the_repair_window_is_bounded_in_real_time_on_every_engine() {
        // Pins the outcome an operator cares about: how long a lost data path
        // can persist. This used to assert against `STEADY_STATE_POLL_INTERVAL`
        // only, which is the strongswan loop's cadence — so it passed while the
        // swu loop, polling six times faster, was acting after ~15s and tearing
        // down working tunnels on routine bearer blips.
        for interval in [
            STEADY_STATE_POLL_INTERVAL,
            SWU_STEADY_STATE_POLL_INTERVAL,
            Duration::from_secs(1),
        ] {
            let strikes = pcscf_unreachable_strikes(interval);
            let worst = interval * strikes;
            assert!(
                worst <= Duration::from_secs(120),
                "at a {interval:?} cadence a lost data path must be repaired in ~2 minutes, \
                 not {worst:?}"
            );
            assert!(
                worst >= PCSCF_UNREACHABLE_WINDOW,
                "at a {interval:?} cadence the tunnel must survive a blip shorter than \
                 {PCSCF_UNREACHABLE_WINDOW:?}, but it is torn down after {worst:?}"
            );
        }
    }

    #[test]
    fn a_slow_poll_still_needs_more_than_one_failure() {
        // A window alone would let an arbitrarily slow loop act on a single
        // sample, and one failed TCP connect on a mobile bearer is routine.
        let strikes = pcscf_unreachable_strikes(Duration::from_secs(600));
        assert_eq!(strikes, PCSCF_UNREACHABLE_MIN_STRIKES);
        assert!(strikes > 1);
        // A zero interval must not divide by zero.
        assert!(pcscf_unreachable_strikes(Duration::ZERO) >= PCSCF_UNREACHABLE_MIN_STRIKES);
    }

    #[test]
    fn the_swu_cadence_does_not_rebuild_on_a_short_blip() {
        // The concrete regression: 15s of unreachability used to be enough on
        // swu. It must not be.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf_reachable: RefCell::new(false),
            ..Default::default()
        };
        let mut streak = 0;
        let blip_ticks =
            (Duration::from_secs(15).as_secs() / SWU_STEADY_STATE_POLL_INTERVAL.as_secs()) as u32;
        for tick in 0..blip_ticks {
            assert_eq!(
                super::tick_steady_state(
                    &engine,
                    &runner,
                    "10.0.0.1",
                    &mut streak,
                    SWU_STEADY_STATE_POLL_INTERVAL,
                ),
                SteadyOutcome::StillUp,
                "tick {tick} of a 15s blip must not rebuild the tunnel"
            );
        }
        assert_eq!(*engine.reinitiate_calls.borrow(), 0);

        // ...but a genuinely lost data path is still repaired, within the window.
        let remaining = pcscf_unreachable_strikes(SWU_STEADY_STATE_POLL_INTERVAL) - blip_ticks;
        let mut rebuilt = false;
        for _ in 0..remaining {
            if let SteadyOutcome::Recovered {
                reason: DegradeReason::PcscfUnreachable,
            } = super::tick_steady_state(
                &engine,
                &runner,
                "10.0.0.1",
                &mut streak,
                SWU_STEADY_STATE_POLL_INTERVAL,
            ) {
                rebuilt = true;
            }
        }
        assert!(rebuilt, "a sustained loss must still be repaired");
    }

    #[test]
    fn steady_state_child_sa_missing_only_reinitiates_no_terminate_no_restart() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::ChildSaMissing),
            ..Default::default()
        };
        let outcome = tick_steady_state(&engine, &runner, "10.0.0.1", &mut 0);
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
        let outcome = tick_steady_state(&engine, &runner, "10.0.0.1", &mut 0);
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
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut 0),
            SteadyOutcome::StillUp
        );
    }

    #[test]
    fn only_child_sa_missing_and_tun_unavailable_skip_the_agent_restart() {
        for reason in [DegradeReason::ChildSaMissing, DegradeReason::TunUnavailable] {
            assert!(
                !recovery_restarts_agent(reason),
                "{reason:?} should leave the agent alone"
            );
        }
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
        // Both arms above list their variants literally, so a new one added to
        // DegradeReason would be silently untested. This match has no wildcard
        // and so fails to compile until whoever adds it decides which arm it
        // belongs in.
        for reason in [
            DegradeReason::ProcessDied,
            DegradeReason::ViciBroken,
            DegradeReason::TunVanished,
            DegradeReason::TunUnavailable,
            DegradeReason::ChildSaMissing,
            DegradeReason::PcscfChanged,
            DegradeReason::PcscfUnreachable,
            DegradeReason::DefaultRouteMissing,
        ] {
            match reason {
                DegradeReason::ProcessDied
                | DegradeReason::ViciBroken
                | DegradeReason::TunVanished
                | DegradeReason::PcscfChanged
                // A rebuilt tunnel means a new data path, which the agent has
                // to re-register over -- so it must be restarted.
                | DegradeReason::PcscfUnreachable
                // Restarted despite the repair itself being non-disruptive:
                // with no route there was no data path, so the agent's
                // registration and any call over it were already dead. Nothing
                // is lost by restarting, and its cached socket is useless.
                | DegradeReason::DefaultRouteMissing => assert!(recovery_restarts_agent(reason)),
                DegradeReason::TunUnavailable | DegradeReason::ChildSaMissing => {
                    assert!(!recovery_restarts_agent(reason))
                }
            }
        }
    }
}
