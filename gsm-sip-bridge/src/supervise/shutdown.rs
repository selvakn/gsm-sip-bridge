//! Container teardown as an owned, ordered plan (specs/021-entrypoint-supervise-rust
//! Phase 2) — replacing `docker/entrypoint.sh`'s `cleanup()` trap, which
//! reconciled ~15 hand-tracked global PID arrays. [`build_shutdown_plan`] is
//! pure: given a record of what actually started this run, it returns the
//! exact step sequence, in order; [`ShutdownPlan::execute`] is the only place
//! any of it is actually run.
//!
//! Ordering mirrors the current trap 1:1, extended by specs/041-shutdown-
//! resource-cleanup to actually give back what a run took rather than only
//! signal its processes: every VoWiFi line's IKE_SA is terminated before its
//! shared charon is killed, every VoWiFi/VoLTE child is waited for (with a
//! kill escalation) before anything it might be using is deleted, this
//! deployment's own XFRM state is flushed, then each line's tunnel interface
//! and virtual cable pair are deleted explicitly — because destroying the
//! device is the only thing that actually releases a strongSwan XFRM
//! interface's `if_id` (see `research.md` R1); nothing else does, including
//! `ip netns del`. See the ordering-invariant tests below and `data-model.md`.

use super::runner::{ChildHandle, CommandRunner, Signal};
use std::collections::BTreeSet;
use std::sync::Arc;

/// How long a single `swanctl --terminate` may run before it is abandoned —
/// bounded so a wedged charon cannot hang the whole teardown (FR-009).
const TERMINATE_TIMEOUT_SECS: u32 = 5;
/// How long a single `ip link del` may run before it is abandoned. Deleting
/// an XFRM interface or a veth end is normally instant; this bound exists for
/// the case research.md R2/R3 exists to rule out — something still
/// referencing the device — so a stuck delete is diagnosed, not hung on.
const DELETE_LINK_TIMEOUT_SECS: u32 = 5;
/// How long a failed `DeleteLink` pauses before its single retry, giving the
/// kernel a moment to finish reaping a just-SIGKILLed child whose lingering
/// socket reference is what blocked the first attempt. Deliberately short:
/// a normally-scheduled process is reaped in well under this, and one stuck
/// in uninterruptible sleep will not be reaped by any wait at all.
const DELETE_RETRY_SETTLE: std::time::Duration = std::time::Duration::from_millis(500);

/// One step of container teardown. `Run`/`RunInNetns` cover the current
/// trap's non-signal actions (`volte-cleanup`, `volte-pdn --action down`,
/// `ip netns del`); `KillChild`/`WaitForExit` cover its `kill`/poll-for-exit
/// pairs.
/// Borrows its handles from the [`StartedState`] it was built from, rather
/// than copying them: a `KillChild` and the `WaitForExit` that confirms it
/// both name the *same* child, which an owned, non-`Copy` [`ChildHandle`]
/// could not express. Borrowing is also the honest description — a teardown
/// step observes a child, it does not take ownership of one.
#[derive(Debug, Clone, PartialEq)]
pub enum TeardownStep<'a> {
    KillChild {
        handle: &'a ChildHandle,
        signal: Signal,
    },
    /// Poll `is_alive` up to `max_polls` times (matching the current trap's
    /// bounded `for _ in $(seq 1 20); do pgrep ... || break; sleep 0.25;
    /// done`), so a zombie cannot hang shutdown.
    WaitForExit {
        handle: &'a ChildHandle,
        max_polls: u32,
    },
    RunInNetns {
        netns: String,
        argv: Vec<String>,
    },
    Run {
        argv: Vec<String>,
    },
    /// Ask the shared charon to tear down one line's IKE_SA (and every
    /// CHILD_SA under it) — `swanctl --terminate --ike <conn_name>`, scoped
    /// to exactly this line's connection name so it can never take another
    /// line's tunnel down with it (see `engines::StrongswanEngine::terminate`,
    /// which this reuses the argv shape of). A no-op for a line whose engine
    /// has no in-place terminate concept (the swu fallback) — no step of
    /// this kind is emitted for one.
    TerminateIke {
        conn_name: String,
        timeout_secs: u32,
    },
    /// Delete one network device — `netns: Some(_)` runs `ip link del`
    /// inside that namespace, `None` runs it in the caller's own (the
    /// container's default) namespace, which is where a line's host-side
    /// veth end lives. This is the load-bearing step of the whole feature:
    /// destroying the device is the only thing that releases a strongSwan
    /// XFRM interface's `if_id` (research.md R1) — no flush and no
    /// `DeleteNetns` does.
    DeleteLink {
        netns: Option<String>,
        iface: String,
        timeout_secs: u32,
    },
    /// Flush this deployment's own XFRM state and policy, if and only if
    /// everything present is identifiably ours (`ours`, the set of `if_id`s
    /// this run's strongswan-engine lines claim) — reuses
    /// `epdg_iface::classify_and_maybe_flush`'s all-ours-or-nothing rule
    /// unchanged (FR-011), the same guard startup's `reclaim_stale_xfrm`
    /// applies, now also run at stop where it can actually help (research.md
    /// R5).
    FlushXfrm {
        ours: BTreeSet<u32>,
    },
    DeleteNetns {
        netns: String,
    },
}

/// One strongswan-engine VoWiFi line's IKE/XFRM teardown facts — absent for a
/// swu-engine line, which has no in-place terminate concept
/// (`SwuEngine::terminate` is a deliberate no-op) and whose tunnel is a plain
/// TUN device torn down with its dialer process rather than an XFRM
/// interface with a claimable `if_id`.
#[derive(Debug, Clone)]
pub struct StrongswanTeardownInfo {
    pub conn_name: String,
    pub tun_iface: String,
    pub if_id: u32,
}

/// One VoWiFi line's teardown-relevant resources, recorded so the plan can
/// name a device to delete rather than merely a namespace to remove. Absent
/// fields (`strongswan: None`) simply emit no step for the concept that does
/// not apply — the same "both bearers, one vocabulary" discipline
/// `StartedVolteLine` follows below (FR-018).
#[derive(Debug, Clone)]
pub struct StartedVowifiLine {
    pub index: u32,
    pub strongswan: Option<StrongswanTeardownInfo>,
    pub netns: String,
    /// This line's host-side (container-default-namespace) veth end.
    /// Deleting it removes the whole pair — the peer inside `netns` goes
    /// with it.
    pub veth_host: String,
}

/// One VoLTE line whose namespace/processes were actually started this run —
/// the typed replacement for `VOLTE_STARTED_LINE_NETNS`/`VOLTE_STARTED_LINE_INDEX`.
/// Handles are `Arc<ChildHandle>` because they are genuinely shared: the
/// supervision loop that spawned the child polls it for liveness on its own
/// thread, while this record exists so the shutdown plan can signal that same
/// child from another. Sharing the claim is the honest description; what is
/// *not* allowed is `wait()`ing on a shared claim, which the owned-only
/// signature of [`super::runner::CommandRunner::wait`] now prevents.
#[derive(Debug, Clone)]
pub struct StartedVolteLine {
    pub index: u32,
    pub netns: String,
    pub carrier_agent_handles: Vec<Arc<ChildHandle>>,
    /// This line's host-side veth end (`ensure_volte_line_veth`'s
    /// `veth_telephony`), if the carrier veth pair was created for it —
    /// `None` for the diagnostic single-`--modem` path, which has no
    /// namespace or veth at all. Deleting it removes the whole pair,
    /// mirroring `StartedVowifiLine::veth_host` (FR-018).
    pub veth_host: Option<String>,
}

/// Everything that actually started this run, appended-to only on success
/// (mirroring `STARTED_NETNS`'s existing append-on-success discipline) — the
/// typed replacement for the current trap's ~15 global PID arrays.
#[derive(Debug, Clone, Default)]
pub struct StartedState {
    pub daemon_supervisor: Option<Arc<ChildHandle>>,
    pub sip_agent_supervisor: Option<Arc<ChildHandle>>,
    pub pcscd: Option<Arc<ChildHandle>>,
    /// Every VoWiFi/strongswan-or-swu per-line child (charon, usim-bridge,
    /// ims-agent, swu dialer, keepalive/log-tail loops, ...) that should be
    /// killed before any namespace teardown, regardless of engine.
    pub vowifi_child_handles: Vec<Arc<ChildHandle>>,
    /// Every network namespace this run created (VoWiFi lines' `ims`/`imsN`
    /// namespaces AND VoLTE lines' `volteN` namespaces) — deleted last, after
    /// every process using it is gone.
    pub started_netns: Vec<String>,
    /// Set when the legacy single-line `volte-register` path ran (not the
    /// auto-discovered multi-line path) — its PDN teardown runs directly,
    /// unscoped to any namespace, restoring `restore_cid` if one was recorded.
    pub legacy_volte_registration: Option<LegacyVolteRegistration>,
    /// Auto-discovered multi-line VoLTE lines that actually started.
    pub volte_lines: Vec<StartedVolteLine>,
    pub volte_bridge_supervisor: Option<Arc<ChildHandle>>,
    /// Every VoWiFi line that reached the point where its namespace/tunnel
    /// setup returns — the same position `started_netns.push` occupies, so a
    /// line that fails later is still fully described here (data-model.md).
    pub vowifi_lines: Vec<StartedVowifiLine>,
}

#[derive(Debug, Clone)]
pub struct LegacyVolteRegistration {
    pub supervisor_handle: Arc<ChildHandle>,
    pub bridge_inbound: bool,
    pub restore_cid: Option<String>,
}

const GSM_SIP_BRIDGE_BIN: &str = "gsm-sip-bridge";
/// Matches the current trap's `for _ in $(seq 1 20); do ...; sleep 0.25; done`
/// — a ~5s bound so a zombie process cannot hang shutdown.
const KILL_CONFIRM_MAX_POLLS: u32 = 20;

/// The whole-teardown budget (FR-010, FR-019). Must stay comfortably under
/// `stop_grace_period` in `docker/docker-compose.yml` — that value is what
/// the container runtime actually enforces, so being force-killed mid-
/// teardown means one of the two drifted; a contract test pins the
/// relationship (see `tests/test_shell_env_contracts.rs` and research.md R8).
pub const STOP_ALLOWANCE: std::time::Duration = std::time::Duration::from_secs(60);

/// Worst-case time a single `DeleteNetns` may take. Unlike `DeleteLink` it
/// carries no `timeout_secs` of its own (`ip netns del` unlinks a name and
/// does not block on device teardown), but it is not free either, so the
/// reserve budgets for it explicitly rather than assuming zero.
const DELETE_NETNS_BUDGET_SECS: u64 = 1;

/// What the non-abandonable release steps in `plan` need in the worst case —
/// the reserve [`TeardownBudget`] holds back so the fallback still has time
/// to reach them (FR-019).
///
/// **Derived from the plan, not a constant** (Greptile P1). It was a flat 15s,
/// which silently understated reality: a four-line deployment emits eight
/// `DeleteLink` steps bounded at [`DELETE_LINK_TIMEOUT_SECS`] each, so the
/// true worst case is ~40s plus namespace deletes — nearly three times the
/// reserve. A budget that under-reserves is worse than none: it lets the
/// fallback fire "in time" and then get force-killed mid-delete anyway,
/// leaving exactly the claimed `if_id` this feature exists to prevent.
/// Deriving it means it stays correct when the supported line count changes,
/// with nobody having to remember this constant exists.
pub fn release_reserve_for(plan: &[TeardownStep]) -> std::time::Duration {
    plan.iter()
        .map(|s| match s {
            // One attempt per link. The settle-and-retry in its `execute`
            // arm is deliberately *opportunistic* — taken only when the
            // budget still has slack — so it does not belong in the reserve.
            // Reserving for two pathological attempts per link would put the
            // reserve (~88s for four lines) above STOP_ALLOWANCE itself,
            // which would mark the budget exhausted before the first step
            // and starve every abandonable step from the outset: no IKE
            // terminate, no child waits, on every single stop.
            TeardownStep::DeleteLink { timeout_secs, .. } => {
                std::time::Duration::from_secs(u64::from(*timeout_secs))
            }
            TeardownStep::DeleteNetns { .. } => {
                std::time::Duration::from_secs(DELETE_NETNS_BUDGET_SECS)
            }
            _ => std::time::Duration::ZERO,
        })
        .sum()
}

/// Builds the exact, ordered teardown step sequence for this run — pure,
/// callable with zero real processes for testing (see `mod tests` below).
pub fn build_shutdown_plan<'a>(
    state: &'a StartedState,
    config_path: &str,
) -> Vec<TeardownStep<'a>> {
    let mut steps = Vec::new();

    // --- Terminate every VoWiFi line's IKE_SA, before charon is touched ----
    // (FR-003, invariant O-1). `vowifi_child_handles` is a flat list — it
    // does not distinguish which handle is charon — so this is placed before
    // the whole kill loop below rather than before one particular handle:
    // that is a strictly stronger guarantee than "before charon" alone.
    for line in &state.vowifi_lines {
        if let Some(sw) = &line.strongswan {
            steps.push(TeardownStep::TerminateIke {
                conn_name: sw.conn_name.clone(),
                timeout_secs: TERMINATE_TIMEOUT_SECS,
            });
        }
    }

    // --- VoWiFi + the two always-present supervisors, killed first --------
    if let Some(h) = &state.daemon_supervisor {
        steps.push(TeardownStep::KillChild {
            handle: h,
            signal: Signal::Term,
        });
    }
    if let Some(h) = &state.sip_agent_supervisor {
        steps.push(TeardownStep::KillChild {
            handle: h,
            signal: Signal::Term,
        });
    }
    for h in &state.vowifi_child_handles {
        steps.push(TeardownStep::KillChild {
            handle: h,
            signal: Signal::Term,
        });
    }
    // Confirm exit, escalating to SIGKILL, before anything these children
    // might be using (a line's tunnel device, its XFRM state) is deleted
    // (FR-001, FR-002, invariants O-2/O-3). Unlike the always-present
    // supervisors above, these children can run *inside* a line's namespace
    // (`vowifi-ims-agent`, via `ip netns exec`) or hold its XFRM SAs
    // (charon), so waiting for all of them — not just the in-namespace ones,
    // since the flat list cannot tell them apart — is what makes the later
    // `DeleteLink`/`FlushXfrm` steps safe. The escalating `KillChild` is
    // unconditional in the plan; signalling an already-exited process is a
    // harmless no-op at execution time.
    //
    // The SIGKILL gets its **own** confirming wait afterwards (Greptile P1).
    // Without it the guarantee above held only for children that exited on
    // the SIGTERM — precisely the ones that did *not* need escalating. A
    // child that ignored SIGTERM would be SIGKILLed and then left racing the
    // `DeleteLink` steps: SIGKILL is not synchronous, and until the kernel
    // has finished reaping the process it still holds its namespace
    // reference and its fds, which is exactly what makes an `ip link del`
    // fail and leave the `if_id` claimed into the next run — the failure
    // this whole feature exists to prevent.
    for h in &state.vowifi_child_handles {
        steps.push(TeardownStep::WaitForExit {
            handle: h,
            max_polls: KILL_CONFIRM_MAX_POLLS,
        });
        steps.push(TeardownStep::KillChild {
            handle: h,
            signal: Signal::Kill,
        });
        steps.push(TeardownStep::WaitForExit {
            handle: h,
            max_polls: KILL_CONFIRM_MAX_POLLS,
        });
    }

    // --- Legacy single-line VoLTE registration -----------------------------
    // SIGKILL, not SIGTERM: the child may be blocked mid-AT-transaction on
    // the modem's serial port, and only an unblockable kill guarantees the
    // kernel closes that fd *now*, before `volte-pdn down` reopens the port.
    if let Some(legacy) = &state.legacy_volte_registration {
        steps.push(TeardownStep::KillChild {
            handle: &legacy.supervisor_handle,
            signal: Signal::Kill,
        });
        steps.push(TeardownStep::WaitForExit {
            handle: &legacy.supervisor_handle,
            max_polls: KILL_CONFIRM_MAX_POLLS,
        });
        steps.push(TeardownStep::Run {
            argv: vec![
                GSM_SIP_BRIDGE_BIN.to_string(),
                "--config".to_string(),
                config_path.to_string(),
                "volte-cleanup".to_string(),
            ],
        });
        if !legacy.bridge_inbound {
            let mut argv = vec![
                GSM_SIP_BRIDGE_BIN.to_string(),
                "--config".to_string(),
                config_path.to_string(),
                "volte-pdn".to_string(),
                "--action".to_string(),
                "down".to_string(),
            ];
            if let Some(cid) = &legacy.restore_cid {
                argv.push("--restore-cid".to_string());
                argv.push(cid.clone());
            }
            steps.push(TeardownStep::Run { argv });
        }
    }

    // --- Auto-discovered multi-line VoLTE ----------------------------------
    // Every carrier-agent process killed first (SIGKILL, same AT-transaction
    // reasoning), THEN each line's own volte-cleanup run *inside its own
    // namespace* — netcfg::teardown's namespace-scoped ip/sysctl commands
    // silently find nothing if run from the wrong namespace — before that
    // namespace is deleted.
    if !state.volte_lines.is_empty() {
        if let Some(h) = &state.volte_bridge_supervisor {
            steps.push(TeardownStep::KillChild {
                handle: h,
                signal: Signal::Kill,
            });
        }
        for line in &state.volte_lines {
            for h in &line.carrier_agent_handles {
                steps.push(TeardownStep::KillChild {
                    handle: h,
                    signal: Signal::Kill,
                });
            }
        }
        for line in &state.volte_lines {
            for h in &line.carrier_agent_handles {
                steps.push(TeardownStep::WaitForExit {
                    handle: h,
                    max_polls: KILL_CONFIRM_MAX_POLLS,
                });
            }
        }
        for line in &state.volte_lines {
            steps.push(TeardownStep::RunInNetns {
                netns: line.netns.clone(),
                argv: vec![
                    GSM_SIP_BRIDGE_BIN.to_string(),
                    "--config".to_string(),
                    config_path.to_string(),
                    "volte-cleanup".to_string(),
                    "--line".to_string(),
                    line.index.to_string(),
                ],
            });
        }
        // Same "the device outlives the netns" reasoning as VoWiFi's veth
        // below (FR-018): the pair is never deleted today, so it — and
        // whatever was still referencing it — waits on `ip netns del` alone.
        for line in &state.volte_lines {
            if let Some(veth) = &line.veth_host {
                steps.push(TeardownStep::DeleteLink {
                    netns: None,
                    iface: veth.clone(),
                    timeout_secs: DELETE_LINK_TIMEOUT_SECS,
                });
            }
        }
    }

    // --- Shared pcscd -------------------------------------------------------
    if let Some(h) = &state.pcscd {
        steps.push(TeardownStep::KillChild {
            handle: h,
            signal: Signal::Term,
        });
    }

    // --- This deployment's own XFRM state, flushed only if every entry is --
    // --- identifiably ours (FR-004, FR-011, invariant O-4) -----------------
    // Run here — after every VoWiFi child has been terminated/waited for
    // above — rather than only at startup, where `reclaim_stale_xfrm` cannot
    // help: by the time a later run looks, the device is inside a namespace
    // nothing can address (research.md R5). After a clean terminate this
    // dump should already be empty; this is belt-and-braces, not the primary
    // mechanism (R1's `DeleteLink` below is).
    let vowifi_if_ids: BTreeSet<u32> = state
        .vowifi_lines
        .iter()
        .filter_map(|l| l.strongswan.as_ref().map(|sw| sw.if_id))
        .collect();
    if !vowifi_if_ids.is_empty() {
        steps.push(TeardownStep::FlushXfrm {
            ours: vowifi_if_ids,
        });
    }

    // --- Delete every VoWiFi line's tunnel interface and veth end (FR-005, -
    // --- invariant O-5) — the load-bearing step: only destroying the -------
    // --- device releases its `if_id` (research.md R1). ---------------------
    for line in &state.vowifi_lines {
        if let Some(sw) = &line.strongswan {
            steps.push(TeardownStep::DeleteLink {
                netns: Some(line.netns.clone()),
                iface: sw.tun_iface.clone(),
                timeout_secs: DELETE_LINK_TIMEOUT_SECS,
            });
        }
        if !line.veth_host.is_empty() {
            steps.push(TeardownStep::DeleteLink {
                netns: None,
                iface: line.veth_host.clone(),
                timeout_secs: DELETE_LINK_TIMEOUT_SECS,
            });
        }
    }

    // --- Finally, every namespace this run created (FR-006, FR-007, --------
    // --- invariants O-6/O-7) — after every device that lived in one has ----
    // --- already been deleted above. ----------------------------------------
    for netns in &state.started_netns {
        steps.push(TeardownStep::DeleteNetns {
            netns: netns.clone(),
        });
    }

    steps
}

/// What became of one [`TeardownStep`] — FR-012's report is rendered from a
/// sequence of these. Distinct from a plain bool/Result because "we declined
/// on purpose" (foreign XFRM state, FR-011) and "the budget ran out and we
/// skipped this on purpose" (FR-019) are not failures and must not read as
/// one, while still needing to be named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// Executed and, as far as this step can tell, succeeded.
    Ok,
    /// Executed, and either failed or deliberately declined (foreign XFRM
    /// state, FR-011) — the reason is stated either way.
    NotReleased(String),
    /// Never executed: the budget ran out and this step was abandonable
    /// (FR-019).
    Abandoned,
}

/// One step's recorded outcome, paired with a short label naming the
/// resource it concerned — FR-012's "naming the resource and the reason".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepReport {
    pub label: String,
    pub outcome: StepOutcome,
}

/// The whole teardown's result — FR-012's "report the outcome of the
/// teardown as a whole". Reporting only; per FR-020 this never raises an
/// alert and never changes the process exit code.
#[derive(Debug, Clone, Default)]
pub struct TeardownOutcome {
    pub steps: Vec<StepReport>,
}

impl TeardownOutcome {
    /// Resources that were not released, in order — the report's core
    /// content (SC-009).
    pub fn not_released(&self) -> impl Iterator<Item = (&str, &str)> {
        self.steps.iter().filter_map(|s| match &s.outcome {
            StepOutcome::NotReleased(reason) => Some((s.label.as_str(), reason.as_str())),
            _ => None,
        })
    }

    /// Steps abandoned to the budget fallback (FR-019, SC-010).
    pub fn abandoned(&self) -> impl Iterator<Item = &str> {
        self.steps
            .iter()
            .filter(|s| s.outcome == StepOutcome::Abandoned)
            .map(|s| s.label.as_str())
    }

    /// Whether every step that ran succeeded and nothing was abandoned —
    /// what "teardown completed cleanly" means.
    pub fn is_clean(&self) -> bool {
        self.steps.iter().all(|s| s.outcome == StepOutcome::Ok)
    }
}

/// The whole-teardown deadline required by FR-019, checked before each
/// *abandonable* step (see [`TeardownStep::is_abandonable`]) so that running
/// out of allowance costs the waits rather than the deletes that actually
/// release a resource — the step order is a dependency order, not a priority
/// order, and the deletes come last precisely because everything referencing
/// a device must go first (data-model.md `TeardownBudget`).
pub struct TeardownBudget {
    deadline: std::time::Instant,
    /// What the non-abandonable release steps still need; once less than
    /// this remains before `deadline`, every following abandonable step is
    /// skipped rather than attempted.
    reserve: std::time::Duration,
}

impl TeardownBudget {
    pub fn new(allowance: std::time::Duration, reserve: std::time::Duration) -> Self {
        Self {
            deadline: std::time::Instant::now() + allowance,
            reserve,
        }
    }

    /// A budget that never triggers the fallback — for tests and call sites
    /// that do not model the stop allowance at all.
    pub fn unbounded() -> Self {
        Self::new(
            std::time::Duration::from_secs(3600),
            std::time::Duration::ZERO,
        )
    }

    fn exhausted(&self) -> bool {
        std::time::Instant::now() + self.reserve >= self.deadline
    }
}

impl TeardownStep<'_> {
    /// A short, stable label naming the resource this step concerns, for
    /// [`StepReport`] — deliberately independent of whether the step
    /// succeeded, so the same label appears whether it's reported as `Ok`,
    /// `NotReleased` or `Abandoned`.
    fn label(&self) -> String {
        match self {
            TeardownStep::KillChild { handle, .. } => format!("process {}", handle.id()),
            TeardownStep::WaitForExit { handle, .. } => format!("process {} exiting", handle.id()),
            TeardownStep::RunInNetns { netns, argv } => {
                format!("`{}` in netns {netns}", argv.join(" "))
            }
            TeardownStep::Run { argv } => format!("`{}`", argv.join(" ")),
            TeardownStep::TerminateIke { conn_name, .. } => format!("IKE_SA {conn_name}"),
            TeardownStep::DeleteLink { netns, iface, .. } => match netns {
                Some(ns) => format!("{iface} in netns {ns}"),
                None => iface.clone(),
            },
            TeardownStep::FlushXfrm { .. } => "this deployment's XFRM state".to_string(),
            TeardownStep::DeleteNetns { netns } => format!("netns {netns}"),
        }
    }

    /// Whether the budget fallback (FR-019) may skip this step. `KillChild`
    /// is excluded even though it precedes a wait: signalling is
    /// non-blocking and cheap, so there is no time to save by skipping it,
    /// and skipping it would only make the following release steps more
    /// likely to find something still alive. `DeleteLink`/`DeleteNetns` are
    /// excluded because they are the steps that actually release a
    /// resource — the entire point of the fallback is to reach them.
    fn is_abandonable(&self) -> bool {
        matches!(
            self,
            TeardownStep::WaitForExit { .. }
                | TeardownStep::RunInNetns { .. }
                | TeardownStep::Run { .. }
                | TeardownStep::TerminateIke { .. }
                | TeardownStep::FlushXfrm { .. }
        )
    }

    /// `may_retry` is whether the budget still has slack for a step to make
    /// a second, best-effort attempt — see `DeleteLink`'s arm, the only one
    /// that uses it.
    fn execute(&self, runner: &dyn CommandRunner, may_retry: bool) -> StepOutcome {
        match self {
            TeardownStep::KillChild { handle, signal } => {
                runner.signal(handle, *signal);
                StepOutcome::Ok
            }
            TeardownStep::WaitForExit { handle, max_polls } => {
                for _ in 0..*max_polls {
                    if !runner.is_alive(handle) {
                        return StepOutcome::Ok;
                    }
                    runner.sleep(std::time::Duration::from_millis(250));
                }
                StepOutcome::NotReleased("did not exit within the kill-confirm bound".to_string())
            }
            TeardownStep::RunInNetns { netns, argv } => {
                let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                run_outcome(runner.run_in_netns(netns, &refs))
            }
            TeardownStep::Run { argv } => {
                let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                run_outcome(runner.run(&refs))
            }
            TeardownStep::TerminateIke {
                conn_name,
                timeout_secs,
            } => {
                let ts = timeout_secs.to_string();
                let env = format!(
                    "STRONGSWAN_CONF={}",
                    super::orchestrate::SHARED_STRONGSWAN_CONF
                );
                run_outcome(runner.run(&[
                    "timeout",
                    &ts,
                    "env",
                    &env,
                    "swanctl",
                    "--terminate",
                    "--ike",
                    conn_name,
                ]))
            }
            TeardownStep::DeleteLink {
                netns,
                iface,
                timeout_secs,
            } => {
                let ts = timeout_secs.to_string();
                let argv = ["timeout", ts.as_str(), "ip", "link", "del", iface.as_str()];
                let attempt = |r: &dyn CommandRunner| match netns {
                    Some(ns) => r.run_in_netns(ns, &argv),
                    None => r.run(&argv),
                };

                // Retry once after a short settle (Greptile P1: "kill
                // confirmation remains non-gating").
                //
                // The confirming wait before this step is best-effort by
                // design: the budget fallback may abandon it (FR-019
                // deliberately spends the last of the allowance on releases
                // rather than waits), and it can time out on a child stuck
                // in uninterruptible sleep. So this step can be reached with
                // a SIGKILLed child not yet reaped, still holding a socket
                // reference that makes the kernel's unregister block until
                // the bound fires.
                //
                // Gating on the confirmation instead — skipping the delete
                // when the child has not been confirmed gone — would be
                // strictly worse: it *guarantees* the `if_id` stays claimed
                // into the next run, which is the exact failure this whole
                // feature exists to prevent. The delete is the thing that
                // must happen; what it needs is tolerance, not a veto.
                //
                // A settle-and-retry is that tolerance, and it targets the
                // case that is actually recoverable: a normally-scheduled
                // process reaped moments after the SIGKILL. (A child wedged
                // in D-state is not recoverable by any amount of waiting —
                // that outcome is reported rather than papered over.)
                //
                // `may_retry` keeps it opportunistic. Reserving for two
                // pathological attempts per link would push the reserve
                // above the whole allowance and starve every abandonable
                // step on every stop; instead the reserve covers one attempt
                // and the retry is taken only while slack remains — which,
                // since a real delete completes in milliseconds, is every
                // realistic case. Under genuine budget exhaustion the single
                // best-effort delete is the correct FR-019 prioritisation.
                match run_outcome(attempt(runner)) {
                    StepOutcome::Ok => StepOutcome::Ok,
                    first_failure if !may_retry => first_failure,
                    _ => {
                        runner.sleep(DELETE_RETRY_SETTLE);
                        // The second attempt's outcome is the one reported:
                        // after the settle it is the more honest description
                        // of why the resource is still held.
                        run_outcome(attempt(runner))
                    }
                }
            }
            TeardownStep::FlushXfrm { ours } => {
                use super::epdg_iface::{classify_and_maybe_flush, XfrmFlushOutcome};
                match classify_and_maybe_flush(runner, ours) {
                    XfrmFlushOutcome::Empty | XfrmFlushOutcome::Flushed => StepOutcome::Ok,
                    XfrmFlushOutcome::LeftForeign => StepOutcome::NotReleased(
                        "XFRM state present that is not this deployment's — left untouched, \
                         same rule as at startup"
                            .to_string(),
                    ),
                    XfrmFlushOutcome::Unreadable => {
                        StepOutcome::NotReleased("could not read the host's XFRM state".to_string())
                    }
                }
            }
            TeardownStep::DeleteNetns { netns } => {
                run_outcome(runner.run(&["ip", "netns", "del", netns]))
            }
        }
    }
}

fn run_outcome(result: std::io::Result<std::process::Output>) -> StepOutcome {
    match result {
        Ok(out) if out.status.success() => StepOutcome::Ok,
        Ok(out) => {
            StepOutcome::NotReleased(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
        Err(e) => StepOutcome::NotReleased(e.to_string()),
    }
}

/// Executes every step in order — the only place a real signal/command is
/// ever issued. Best-effort throughout, matching the current trap's
/// `... 2>/dev/null || true` convention: one step's failure must not stop the
/// rest of cleanup. Once `budget` is exhausted, every remaining abandonable
/// step (see [`TeardownStep::is_abandonable`]) is skipped rather than run,
/// so the steps that actually release a device or namespace are always
/// reached (FR-019).
pub fn execute_shutdown_plan(
    steps: &[TeardownStep],
    runner: &dyn CommandRunner,
    budget: &TeardownBudget,
) -> TeardownOutcome {
    let mut outcome = TeardownOutcome::default();
    for step in steps {
        let label = step.label();
        // Evaluated per step, not once up front: the budget drains as the
        // plan runs, so a retry that had slack early may not later.
        let exhausted = budget.exhausted();
        let result = if step.is_abandonable() && exhausted {
            StepOutcome::Abandoned
        } else {
            step.execute(runner, !exhausted)
        };
        outcome.steps.push(StepReport {
            label,
            outcome: result,
        });
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::runner::{ChildSpec, MockCommandRunner};

    fn handle(runner: &MockCommandRunner, n: u32) -> Arc<ChildHandle> {
        // Each call returns a fresh handle; spawn N throwaway children to get
        // N distinct handles for a test's fixture.
        let mut h = runner.spawn(ChildSpec::new(["true"])).unwrap();
        for _ in 1..n {
            h = runner.spawn(ChildSpec::new(["true"])).unwrap();
        }
        Arc::new(h)
    }

    #[test]
    fn every_lines_child_kill_precedes_its_own_pdn_or_namespace_teardown() {
        let runner = MockCommandRunner::new();
        let carrier = handle(&runner, 1);
        let state = StartedState {
            volte_lines: vec![StartedVolteLine {
                index: 0,
                netns: "volte0".to_string(),
                carrier_agent_handles: vec![carrier.clone()],
                veth_host: None,
            }],
            started_netns: vec!["volte0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");

        let kill_pos = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::KillChild { handle: h, .. } if h.id() == carrier.id()))
            .expect("carrier kill step must exist");
        let cleanup_pos = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::RunInNetns { netns, .. } if netns == "volte0"))
            .expect("this line's volte-cleanup step must exist");
        let netns_del_pos = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::DeleteNetns { netns } if netns == "volte0"))
            .expect("this line's namespace deletion step must exist");

        assert!(
            kill_pos < cleanup_pos,
            "child kill must precede this line's PDN/namespace teardown"
        );
        assert!(
            cleanup_pos < netns_del_pos,
            "volte-cleanup must run before its namespace is deleted"
        );
    }

    #[test]
    fn a_volte_lines_cleanup_step_is_scoped_to_run_inside_its_own_namespace() {
        let runner = MockCommandRunner::new();
        let carrier = handle(&runner, 1);
        let state = StartedState {
            volte_lines: vec![StartedVolteLine {
                index: 2,
                netns: "volte2".to_string(),
                carrier_agent_handles: vec![carrier.clone()],
                veth_host: None,
            }],
            started_netns: vec!["volte2".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");

        let cleanup_step = steps
            .iter()
            .find(|s| matches!(s, TeardownStep::RunInNetns { .. }))
            .expect("a RunInNetns step must exist");
        match cleanup_step {
            TeardownStep::RunInNetns { netns, argv } => {
                assert_eq!(netns, "volte2");
                assert!(argv.contains(&"--line".to_string()));
                assert!(argv.contains(&"2".to_string()));
            }
            _ => unreachable!(),
        }
        // Never a bare `Run` (unscoped) for a multi-line VoLTE cleanup — that
        // would execute in the default namespace, where netcfg::teardown's
        // ip/sysctl commands silently find nothing.
        assert!(!steps.iter().any(
            |s| matches!(s, TeardownStep::Run { argv } if argv.iter().any(|a| a == "volte-cleanup"))
        ));
    }

    #[test]
    fn a_child_that_may_block_mid_at_transaction_is_killed_not_terminated() {
        // Covers both the legacy single-line VoLTE path and the
        // auto-discovered multi-line carrier-agent path — both may be
        // blocked mid-AT-transaction on the modem's serial port, so both
        // MUST use SIGKILL: a graceful SIGTERM could leave the child holding
        // the port past any timeout, racing `volte-pdn down`'s reopen.
        let runner = MockCommandRunner::new();
        let legacy_handle = handle(&runner, 1);
        let carrier_handle = handle(&runner, 1);
        let state = StartedState {
            legacy_volte_registration: Some(LegacyVolteRegistration {
                supervisor_handle: legacy_handle.clone(),
                bridge_inbound: false,
                restore_cid: None,
            }),
            volte_lines: vec![StartedVolteLine {
                index: 0,
                netns: "volte0".to_string(),
                carrier_agent_handles: vec![carrier_handle.clone()],
                veth_host: None,
            }],
            started_netns: vec!["volte0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");

        for h in [legacy_handle, carrier_handle] {
            let kill_step = steps
                .iter()
                .find(|s| matches!(s, TeardownStep::KillChild { handle, .. } if handle.id() == h.id()))
                .unwrap_or_else(|| panic!("expected a kill step for {h:?}"));
            match kill_step {
                TeardownStep::KillChild { signal, .. } => {
                    assert_eq!(
                        *signal,
                        Signal::Kill,
                        "must be SIGKILL, not SIGTERM, for {h:?}"
                    )
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn the_always_present_supervisors_get_sigterm_not_sigkill() {
        // The circuit-switched daemon and the shared vowifi-sip-agent have no
        // serial-port-blocking concern the way an AT-transaction-driving
        // child does — matches the current trap's plain `kill` (SIGTERM).
        let runner = MockCommandRunner::new();
        let daemon = handle(&runner, 1);
        let sip_agent = handle(&runner, 1);
        let state = StartedState {
            daemon_supervisor: Some(daemon.clone()),
            sip_agent_supervisor: Some(sip_agent.clone()),
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        for h in [daemon, sip_agent] {
            let step = steps
                .iter()
                .find(|s| matches!(s, TeardownStep::KillChild { handle, .. } if handle.id() == h.id()))
                .unwrap();
            match step {
                TeardownStep::KillChild { signal, .. } => assert_eq!(*signal, Signal::Term),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn pcscd_is_killed_before_any_namespace_is_deleted() {
        let runner = MockCommandRunner::new();
        let pcscd = handle(&runner, 1);
        let state = StartedState {
            pcscd: Some(pcscd.clone()),
            started_netns: vec!["ims0".to_string(), "ims1".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        let pcscd_pos = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::KillChild { handle, .. } if handle.id() == pcscd.id()))
            .unwrap();
        for netns in ["ims0", "ims1"] {
            let del_pos = steps
                .iter()
                .position(|s| matches!(s, TeardownStep::DeleteNetns { netns: n } if n == netns))
                .unwrap();
            assert!(pcscd_pos < del_pos);
        }
    }

    #[test]
    fn every_started_namespace_gets_exactly_one_delete_step() {
        let state = StartedState {
            started_netns: vec!["ims0".to_string(), "volte1".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        for netns in ["ims0", "volte1"] {
            let count = steps
                .iter()
                .filter(|s| matches!(s, TeardownStep::DeleteNetns { netns: n } if n == netns))
                .count();
            assert_eq!(count, 1, "{netns} should be deleted exactly once");
        }
    }

    #[test]
    fn an_empty_started_state_produces_an_empty_plan() {
        // Nothing started this run (e.g. a fatal precondition failed before
        // anything spawned) -> nothing to tear down. Guards against a plan
        // builder that assumes some subsystem is always present.
        let state = StartedState::default();
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        assert!(steps.is_empty());
    }

    #[test]
    fn legacy_volte_registration_restores_the_displaced_cid_when_recorded() {
        let runner = MockCommandRunner::new();
        let legacy_handle = handle(&runner, 1);
        let state = StartedState {
            legacy_volte_registration: Some(LegacyVolteRegistration {
                supervisor_handle: legacy_handle.clone(),
                bridge_inbound: false,
                restore_cid: Some("3".to_string()),
            }),
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        let pdn_down = steps
            .iter()
            .find(|s| matches!(s, TeardownStep::Run { argv } if argv.iter().any(|a| a == "volte-pdn")))
            .expect("volte-pdn down step must exist for the legacy non-bridge-inbound path");
        match pdn_down {
            TeardownStep::Run { argv } => {
                assert!(argv.contains(&"--restore-cid".to_string()));
                assert!(argv.contains(&"3".to_string()));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn legacy_volte_bridge_inbound_skips_the_direct_pdn_down_step() {
        // bridge_inbound writes a line manifest; `volte-cleanup` (already
        // emitted unconditionally above) tears every recorded line down from
        // it — running a second, unscoped `volte-pdn down` on top would be
        // guessing at the CLI default rather than the exact line that
        // registered, so it must not appear.
        let runner = MockCommandRunner::new();
        let legacy_handle = handle(&runner, 1);
        let state = StartedState {
            legacy_volte_registration: Some(LegacyVolteRegistration {
                supervisor_handle: legacy_handle.clone(),
                bridge_inbound: true,
                restore_cid: None,
            }),
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        assert!(!steps.iter().any(
            |s| matches!(s, TeardownStep::Run { argv } if argv.iter().any(|a| a == "volte-pdn"))
        ));
    }

    #[test]
    fn executing_the_plan_against_a_mock_runner_issues_exactly_the_built_steps() {
        let runner = MockCommandRunner::new();
        let daemon = handle(&runner, 1);
        let state = StartedState {
            daemon_supervisor: Some(daemon.clone()),
            started_netns: vec!["ims0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        let outcome = execute_shutdown_plan(&steps, &runner, &TeardownBudget::unbounded());

        assert_eq!(runner.signals_for(&daemon), vec![Signal::Term]);
        let netns_deletes = runner.run_calls.lock().unwrap();
        assert!(netns_deletes
            .iter()
            .any(|argv| argv == &["ip", "netns", "del", "ims0"]));
        assert!(outcome.is_clean(), "a clean mock run should report clean");
    }

    // ------------------------------------------------------------------
    // specs/041-shutdown-resource-cleanup: ordering invariants O-1..O-11
    // (data-model.md), the budget fallback, and idempotence. See
    // contracts/observable-contracts.md C1 for the full intended sequence.
    // ------------------------------------------------------------------

    fn strongswan_line(idx: u32, netns: &str, if_id: u32) -> StartedVowifiLine {
        StartedVowifiLine {
            index: idx,
            strongswan: Some(StrongswanTeardownInfo {
                conn_name: format!("ims{idx}"),
                tun_iface: format!("tun23-{idx}"),
                if_id,
            }),
            netns: netns.to_string(),
            veth_host: format!("veth-sip{idx}"),
        }
    }

    fn swu_line(idx: u32, netns: &str) -> StartedVowifiLine {
        StartedVowifiLine {
            index: idx,
            strongswan: None,
            netns: netns.to_string(),
            veth_host: format!("veth-sip{idx}"),
        }
    }

    #[test]
    fn o1_terminate_ike_precedes_every_vowifi_child_kill_including_charons() {
        let runner = MockCommandRunner::new();
        // The flat handle list cannot say which one is charon, so the
        // invariant this test actually needs is the stronger one: every
        // TerminateIke precedes the *whole* kill loop.
        let charon = handle(&runner, 1);
        let state = StartedState {
            vowifi_lines: vec![strongswan_line(0, "ims0", 23)],
            vowifi_child_handles: vec![charon.clone()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");

        let terminate_pos = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::TerminateIke { conn_name, .. } if conn_name == "ims0"))
            .expect("a TerminateIke step must exist");
        let kill_pos = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::KillChild { handle: h, signal: Signal::Term } if h.id() == charon.id()))
            .expect("charon's kill step must exist");
        assert!(terminate_pos < kill_pos);
    }

    #[test]
    fn a_swu_line_emits_no_terminate_ike_step() {
        let state = StartedState {
            vowifi_lines: vec![swu_line(0, "ims0")],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        assert!(!steps
            .iter()
            .any(|s| matches!(s, TeardownStep::TerminateIke { .. })));
    }

    #[test]
    fn o2_every_vowifi_child_gets_a_wait_with_kill_escalation() {
        let runner = MockCommandRunner::new();
        let ims_agent = handle(&runner, 1);
        let state = StartedState {
            vowifi_child_handles: vec![ims_agent.clone()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");

        let term_pos = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::KillChild { handle: h, signal: Signal::Term } if h.id() == ims_agent.id()))
            .expect("an initial SIGTERM must exist");
        let wait_pos = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::WaitForExit { handle: h, .. } if h.id() == ims_agent.id()))
            .expect("a WaitForExit must exist (today's gap this feature closes)");
        let kill_pos = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::KillChild { handle: h, signal: Signal::Kill } if h.id() == ims_agent.id()))
            .expect("an escalating SIGKILL must exist");
        assert!(term_pos < wait_pos, "must wait after the initial SIGTERM");
        assert!(wait_pos < kill_pos, "escalation must follow the wait");

        // Greptile P1: the SIGKILL needs its own confirming wait. Without
        // one, the only children whose exit was ever confirmed are those
        // that went down on the SIGTERM — i.e. exactly the ones that did not
        // need escalating — while a child that ignored SIGTERM races the
        // device deletes, still holding the namespace the kernel has not
        // finished reaping it out of.
        let confirm_pos = steps
            .iter()
            .skip(kill_pos)
            .position(|s| matches!(s, TeardownStep::WaitForExit { handle: h, .. } if h.id() == ims_agent.id()))
            .map(|p| p + kill_pos)
            .expect("the escalating SIGKILL must be followed by its own confirming wait");
        assert!(confirm_pos > kill_pos);
    }

    #[test]
    fn every_delete_step_is_preceded_by_a_confirmed_kill_of_every_child() {
        // The stronger form of O-2/O-3: not merely "a wait exists", but
        // "the *last* wait for every child precedes the first delete", which
        // is what actually makes the deletes safe.
        let runner = MockCommandRunner::new();
        let a = handle(&runner, 1);
        let b = handle(&runner, 1);
        let state = StartedState {
            vowifi_lines: vec![strongswan_line(0, "ims0", 23)],
            vowifi_child_handles: vec![a.clone(), b.clone()],
            started_netns: vec!["ims0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        let first_delete = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::DeleteLink { .. }))
            .unwrap();
        for h in [&a, &b] {
            let last_wait = steps
                .iter()
                .rposition(|s| matches!(s, TeardownStep::WaitForExit { handle: x, .. } if x.id() == h.id()))
                .expect("every child must be waited for");
            let last_kill = steps
                .iter()
                .rposition(
                    |s| matches!(s, TeardownStep::KillChild { handle: x, .. } if x.id() == h.id()),
                )
                .unwrap();
            assert!(last_kill < last_wait, "the final signal must be confirmed");
            assert!(
                last_wait < first_delete,
                "confirm every child before deleting devices"
            );
        }
    }

    #[test]
    fn the_release_reserve_covers_every_non_abandonable_bound_in_the_plan() {
        // Greptile P1: this was a flat 15s constant while a four-line
        // deployment's release phase is bounded at ~40s+, so the fallback
        // could fire "in time" and still be force-killed mid-delete.
        // Deriving it from the plan keeps it right at any line count.
        let state = StartedState {
            vowifi_lines: vec![
                strongswan_line(0, "ims0", 23),
                strongswan_line(1, "ims1", 24),
                strongswan_line(2, "ims2", 25),
                strongswan_line(3, "ims3", 26),
            ],
            started_netns: vec![
                "ims0".to_string(),
                "ims1".to_string(),
                "ims2".to_string(),
                "ims3".to_string(),
            ],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        let reserve = release_reserve_for(&steps);

        let worst_case: u64 = steps
            .iter()
            .map(|s| match s {
                TeardownStep::DeleteLink { timeout_secs, .. } => u64::from(*timeout_secs),
                TeardownStep::DeleteNetns { .. } => DELETE_NETNS_BUDGET_SECS,
                _ => 0,
            })
            .sum();
        assert_eq!(reserve.as_secs(), worst_case);
        assert!(
            reserve.as_secs() >= 8 * u64::from(DELETE_LINK_TIMEOUT_SECS),
            "four lines means eight bounded link deletes; reserve was {}s",
            reserve.as_secs()
        );
        assert!(
            reserve < STOP_ALLOWANCE,
            "the reserve must still fit inside the allowance it is carved out of"
        );
    }

    #[test]
    fn o3_o5_o6_every_wait_precedes_every_delete_link_and_delete_netns() {
        let runner = MockCommandRunner::new();
        let ims_agent = handle(&runner, 1);
        let state = StartedState {
            vowifi_lines: vec![strongswan_line(0, "ims0", 23)],
            vowifi_child_handles: vec![ims_agent],
            started_netns: vec!["ims0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");

        let last_wait = steps
            .iter()
            .rposition(|s| matches!(s, TeardownStep::WaitForExit { .. }))
            .expect("a wait step must exist");
        let first_delete_link = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::DeleteLink { .. }))
            .expect("a DeleteLink step must exist");
        let netns_del_pos = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::DeleteNetns { netns } if netns == "ims0"))
            .expect("the namespace delete must exist");

        assert!(
            last_wait < first_delete_link,
            "O-3: waits before device deletes"
        );
        assert!(
            first_delete_link < netns_del_pos,
            "O-6: device deletes before namespace delete"
        );
    }

    #[test]
    fn o4_flush_xfrm_follows_terminate_ike_and_the_vowifi_wait_escalate_block() {
        let runner = MockCommandRunner::new();
        let charon = handle(&runner, 1);
        let state = StartedState {
            vowifi_lines: vec![strongswan_line(0, "ims0", 23)],
            vowifi_child_handles: vec![charon],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");

        let terminate_pos = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::TerminateIke { .. }))
            .unwrap();
        let last_wait = steps
            .iter()
            .rposition(|s| matches!(s, TeardownStep::WaitForExit { .. }))
            .unwrap();
        let flush_pos = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::FlushXfrm { .. }))
            .expect("a FlushXfrm step must exist when a strongswan line is present");
        assert!(terminate_pos < flush_pos);
        assert!(last_wait < flush_pos);
    }

    #[test]
    fn no_strongswan_lines_means_no_flush_xfrm_step() {
        let state = StartedState {
            vowifi_lines: vec![swu_line(0, "ims0")],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        assert!(!steps
            .iter()
            .any(|s| matches!(s, TeardownStep::FlushXfrm { .. })));
    }

    #[test]
    fn flush_xfrm_carries_exactly_this_runs_strongswan_if_ids() {
        let state = StartedState {
            vowifi_lines: vec![
                strongswan_line(0, "ims0", 23),
                strongswan_line(1, "ims1", 24),
                swu_line(2, "ims2"),
            ],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        let flush = steps
            .iter()
            .find_map(|s| match s {
                TeardownStep::FlushXfrm { ours } => Some(ours),
                _ => None,
            })
            .unwrap();
        assert_eq!(flush, &BTreeSet::from([23, 24]));
    }

    #[test]
    fn o5_o6_a_strongswan_lines_tun_and_veth_are_deleted_before_its_namespace() {
        let state = StartedState {
            vowifi_lines: vec![strongswan_line(0, "ims0", 23)],
            started_netns: vec!["ims0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");

        let tun_del = steps
            .iter()
            .position(|s| {
                matches!(s, TeardownStep::DeleteLink { netns: Some(ns), iface, .. }
                    if ns == "ims0" && iface == "tun23-0")
            })
            .expect("the tun interface must be deleted inside its netns");
        let veth_del = steps
            .iter()
            .position(|s| {
                matches!(s, TeardownStep::DeleteLink { netns: None, iface, .. } if iface == "veth-sip0")
            })
            .expect("the host-side veth end must be deleted in the container's own namespace");
        let netns_del = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::DeleteNetns { netns } if netns == "ims0"))
            .unwrap();
        assert!(tun_del < netns_del);
        assert!(veth_del < netns_del);
    }

    #[test]
    fn a_swu_lines_veth_is_still_deleted_even_with_no_tun_to_delete() {
        let state = StartedState {
            vowifi_lines: vec![swu_line(0, "ims0")],
            started_netns: vec!["ims0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        assert!(steps.iter().any(
            |s| matches!(s, TeardownStep::DeleteLink { netns: None, iface, .. } if iface == "veth-sip0")
        ));
        assert!(!steps
            .iter()
            .any(|s| matches!(s, TeardownStep::DeleteLink { netns: Some(_), .. })));
    }

    #[test]
    fn o7_a_namespace_with_no_started_line_at_all_still_gets_deleted() {
        // A line that failed before any StartedVowifiLine/StartedVolteLine
        // could be recorded (FR-007) — only started_netns knows about it.
        let state = StartedState {
            started_netns: vec!["ims3".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        assert!(steps
            .iter()
            .any(|s| matches!(s, TeardownStep::DeleteNetns { netns } if netns == "ims3")));
    }

    #[test]
    fn o8_every_terminate_and_delete_link_step_carries_a_nonzero_bound() {
        let state = StartedState {
            vowifi_lines: vec![strongswan_line(0, "ims0", 23)],
            started_netns: vec!["ims0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        for step in &steps {
            match step {
                TeardownStep::TerminateIke { timeout_secs, .. }
                | TeardownStep::DeleteLink { timeout_secs, .. } => {
                    assert!(*timeout_secs > 0, "{step:?} must carry a nonzero bound");
                }
                _ => {}
            }
        }
    }

    #[test]
    fn o9_a_volte_lines_relative_order_is_unchanged_by_the_bearer_unification() {
        let runner = MockCommandRunner::new();
        let carrier = handle(&runner, 1);
        let state = StartedState {
            volte_lines: vec![StartedVolteLine {
                index: 0,
                netns: "volte0".to_string(),
                carrier_agent_handles: vec![carrier.clone()],
                veth_host: Some("veth-tel0".to_string()),
            }],
            started_netns: vec!["volte0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");

        let kill_pos = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::KillChild { handle: h, .. } if h.id() == carrier.id()))
            .unwrap();
        let cleanup_pos = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::RunInNetns { netns, .. } if netns == "volte0"))
            .unwrap();
        let netns_del_pos = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::DeleteNetns { netns } if netns == "volte0"))
            .unwrap();
        assert!(kill_pos < cleanup_pos, "kill must still precede cleanup");
        assert!(
            cleanup_pos < netns_del_pos,
            "cleanup must still precede namespace deletion"
        );
    }

    #[test]
    fn o11_a_volte_lines_veth_is_deleted_before_its_namespace() {
        let state = StartedState {
            volte_lines: vec![StartedVolteLine {
                index: 0,
                netns: "volte0".to_string(),
                carrier_agent_handles: vec![],
                veth_host: Some("veth-tel0".to_string()),
            }],
            started_netns: vec!["volte0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        let veth_del = steps
            .iter()
            .position(|s| {
                matches!(s, TeardownStep::DeleteLink { netns: None, iface, .. } if iface == "veth-tel0")
            })
            .expect("VoLTE's veth must now be explicitly deleted (FR-018) — it never was before");
        let netns_del = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::DeleteNetns { netns } if netns == "volte0"))
            .unwrap();
        assert!(veth_del < netns_del);
    }

    #[test]
    fn a_volte_line_with_no_veth_emits_no_delete_link_step() {
        // The diagnostic single-`--modem` path: no carrier veth was ever
        // created for it, so there is nothing to delete.
        let state = StartedState {
            volte_lines: vec![StartedVolteLine {
                index: 0,
                netns: "volte0".to_string(),
                carrier_agent_handles: vec![],
                veth_host: None,
            }],
            started_netns: vec!["volte0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        assert!(!steps
            .iter()
            .any(|s| matches!(s, TeardownStep::DeleteLink { .. })));
    }

    #[test]
    fn fr008_building_the_same_plan_twice_yields_identical_steps() {
        let state = StartedState {
            vowifi_lines: vec![strongswan_line(0, "ims0", 23)],
            volte_lines: vec![StartedVolteLine {
                index: 1,
                netns: "volte1".to_string(),
                carrier_agent_handles: vec![],
                veth_host: Some("veth-tel1".to_string()),
            }],
            started_netns: vec!["ims0".to_string(), "volte1".to_string()],
            ..Default::default()
        };
        let a = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        let b = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        assert_eq!(a, b);
    }

    #[test]
    fn o10_the_budget_fallback_still_reaches_every_delete_link_and_delete_netns() {
        // An immediately-exhausted budget must not cost a single
        // DeleteLink/DeleteNetns step — those are the ones that actually
        // release the if_id (FR-019).
        let runner = MockCommandRunner::new();
        let ims_agent = handle(&runner, 1);
        let state = StartedState {
            vowifi_lines: vec![strongswan_line(0, "ims0", 23)],
            vowifi_child_handles: vec![ims_agent],
            started_netns: vec!["ims0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        let exhausted = TeardownBudget::new(std::time::Duration::ZERO, std::time::Duration::ZERO);
        let outcome = execute_shutdown_plan(&steps, &runner, &exhausted);

        let expected_release_steps = steps
            .iter()
            .filter(|s| {
                matches!(
                    s,
                    TeardownStep::DeleteLink { .. } | TeardownStep::DeleteNetns { .. }
                )
            })
            .count();
        let actually_released = outcome
            .steps
            .iter()
            .zip(&steps)
            .filter(|(r, s)| {
                matches!(
                    s,
                    TeardownStep::DeleteLink { .. } | TeardownStep::DeleteNetns { .. }
                ) && r.outcome == StepOutcome::Ok
            })
            .count();
        assert_eq!(
            actually_released, expected_release_steps,
            "every release step must still run even with a zero budget"
        );
        assert!(
            outcome.abandoned().next().is_some(),
            "an abandonable step (the wait) should have been skipped"
        );

        // The tun delete runs *inside* ims0 (`DeleteLink { netns: Some(_), .. }`),
        // so it lands in run_in_netns_calls, not run_calls.
        let netns_calls = runner.run_in_netns_calls.lock().unwrap();
        assert!(netns_calls.iter().any(|(ns, c)| ns == "ims0"
            && c.contains(&"del".to_string())
            && c.contains(&"tun23-0".to_string())));
    }

    // --- Greptile P1: the kill confirmation is not gating ------------------
    //
    // It cannot be: gating the delete on a confirmation that may legitimately
    // be abandoned (budget) or time out (D-state child) would *guarantee* the
    // if_id stays claimed, which is the failure this feature exists to
    // prevent. The delete must still be attempted — what it gains instead is
    // a settle-and-retry, so a child reaped moments after its SIGKILL does
    // not cost the identifier.

    fn count_netns_attempts(runner: &MockCommandRunner, netns: &str, iface: &str) -> usize {
        runner
            .run_in_netns_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(ns, c)| {
                ns == netns && c.contains(&"del".to_string()) && c.contains(&iface.to_string())
            })
            .count()
    }

    #[test]
    fn a_failed_device_delete_settles_and_retries_once() {
        let runner = MockCommandRunner::new();
        // Seed the delete itself as failing, as it would if a not-yet-reaped
        // child still held a reference to the device.
        runner.set_run_output("netns:ims0:timeout 5 ip link del tun23-0", failure_output());
        let state = StartedState {
            vowifi_lines: vec![strongswan_line(0, "ims0", 23)],
            started_netns: vec!["ims0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        let outcome = execute_shutdown_plan(&steps, &runner, &TeardownBudget::unbounded());

        assert_eq!(
            count_netns_attempts(&runner, "ims0", "tun23-0"),
            2,
            "a failed delete must be retried once after a settle"
        );
        assert!(
            runner.sleeps.lock().unwrap().contains(&DELETE_RETRY_SETTLE),
            "the retry must be preceded by the settle pause"
        );
        // Still failing after the retry -> reported, never silently dropped.
        assert!(outcome
            .not_released()
            .any(|(label, _)| label.contains("tun23-0")));
    }

    #[test]
    fn a_device_delete_that_succeeds_is_never_retried() {
        let runner = MockCommandRunner::new();
        let state = StartedState {
            vowifi_lines: vec![strongswan_line(0, "ims0", 23)],
            started_netns: vec!["ims0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        execute_shutdown_plan(&steps, &runner, &TeardownBudget::unbounded());
        assert_eq!(count_netns_attempts(&runner, "ims0", "tun23-0"), 1);
    }

    #[test]
    fn an_exhausted_budget_still_attempts_every_delete_but_skips_the_retry() {
        // The delete itself is never sacrificed to the budget — only its
        // opportunistic second attempt is.
        let runner = MockCommandRunner::new();
        runner.set_run_output("netns:ims0:timeout 5 ip link del tun23-0", failure_output());
        let state = StartedState {
            vowifi_lines: vec![strongswan_line(0, "ims0", 23)],
            started_netns: vec!["ims0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        let exhausted = TeardownBudget::new(std::time::Duration::ZERO, std::time::Duration::ZERO);
        execute_shutdown_plan(&steps, &runner, &exhausted);

        assert_eq!(
            count_netns_attempts(&runner, "ims0", "tun23-0"),
            1,
            "the delete is still attempted under budget pressure, the retry is not"
        );
    }

    #[test]
    fn failure_output_seeds_actually_fail() {
        // Guards the three tests above: if `failure_output()` ever stopped
        // registering as a failure, they would silently assert nothing.
        assert!(!failure_output().status.success());
    }

    #[test]
    fn a_generous_budget_abandons_nothing() {
        let runner = MockCommandRunner::new();
        let state = StartedState {
            vowifi_lines: vec![strongswan_line(0, "ims0", 23)],
            started_netns: vec!["ims0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        let outcome = execute_shutdown_plan(&steps, &runner, &TeardownBudget::unbounded());
        assert_eq!(outcome.abandoned().count(), 0);
        assert!(outcome.is_clean());
    }

    #[test]
    fn flush_xfrm_reports_not_released_when_foreign_state_is_present() {
        let runner = MockCommandRunner::new();
        runner.set_run_output(
            "ip xfrm state",
            success_output("src 10.0.0.1 dst 10.0.0.2\n\tif_id 0x99\n"),
        );
        runner.set_run_output("ip xfrm policy", success_output(""));
        let state = StartedState {
            vowifi_lines: vec![strongswan_line(0, "ims0", 23)],
            started_netns: vec!["ims0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        let outcome = execute_shutdown_plan(&steps, &runner, &TeardownBudget::unbounded());
        assert_eq!(
            outcome.not_released().count(),
            1,
            "the foreign XFRM state must be reported, not silently skipped"
        );
        assert!(!outcome.is_clean());

        let calls = runner.run_calls.lock().unwrap();
        assert!(
            !calls.iter().any(|c| c.contains(&"flush".to_string())),
            "foreign XFRM state must never be flushed, at stop any more than at startup"
        );
    }

    fn failure_output() -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(256), // exit code 1
            stdout: vec![],
            stderr: b"RTNETLINK answers: Device or resource busy".to_vec(),
        }
    }

    fn success_output(stdout: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: vec![],
        }
    }
}
