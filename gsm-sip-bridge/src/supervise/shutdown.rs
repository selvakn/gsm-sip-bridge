//! Container teardown as an owned, ordered plan (specs/021-entrypoint-supervise-rust
//! Phase 2) — replacing `docker/entrypoint.sh`'s `cleanup()` trap, which
//! reconciled ~15 hand-tracked global PID arrays. [`build_shutdown_plan`] is
//! pure: given a record of what actually started this run, it returns the
//! exact step sequence, in order; [`ShutdownPlan::execute`] is the only place
//! any of it is actually run.
//!
//! Ordering mirrors the current trap 1:1: VoWiFi child processes first, then
//! (if a legacy single-line VoLTE registration ran) its PDN teardown, then
//! every auto-discovered multi-line VoLTE line's carrier-agent/bridge kill
//! followed by that line's *namespace-scoped* `volte-cleanup` before its
//! namespace is deleted, then the shared pcscd, then every started namespace
//! is deleted. See the ordering-invariant tests below and `data-model.md`.

use super::runner::{ChildHandle, CommandRunner, Signal};

/// One step of container teardown. `Run`/`RunInNetns` cover the current
/// trap's non-signal actions (`volte-cleanup`, `volte-pdn --action down`,
/// `ip netns del`); `KillChild`/`WaitForExit` cover its `kill`/poll-for-exit
/// pairs.
#[derive(Debug, Clone, PartialEq)]
pub enum TeardownStep {
    KillChild {
        handle: ChildHandle,
        signal: Signal,
    },
    /// Poll `is_alive` up to `max_polls` times (matching the current trap's
    /// bounded `for _ in $(seq 1 20); do pgrep ... || break; sleep 0.25;
    /// done`), so a zombie cannot hang shutdown.
    WaitForExit {
        handle: ChildHandle,
        max_polls: u32,
    },
    RunInNetns {
        netns: String,
        argv: Vec<String>,
    },
    Run {
        argv: Vec<String>,
    },
    DeleteNetns {
        netns: String,
    },
}

/// One VoLTE line whose namespace/processes were actually started this run —
/// the typed replacement for `VOLTE_STARTED_LINE_NETNS`/`VOLTE_STARTED_LINE_INDEX`.
#[derive(Debug, Clone)]
pub struct StartedVolteLine {
    pub index: u32,
    pub netns: String,
    pub carrier_agent_handles: Vec<ChildHandle>,
}

/// Everything that actually started this run, appended-to only on success
/// (mirroring `STARTED_NETNS`'s existing append-on-success discipline) — the
/// typed replacement for the current trap's ~15 global PID arrays.
#[derive(Debug, Clone, Default)]
pub struct StartedState {
    pub daemon_supervisor: Option<ChildHandle>,
    pub sip_agent_supervisor: Option<ChildHandle>,
    pub pcscd: Option<ChildHandle>,
    /// Every VoWiFi/strongswan-or-swu per-line child (charon, usim-bridge,
    /// ims-agent, swu dialer, keepalive/log-tail loops, ...) that should be
    /// killed before any namespace teardown, regardless of engine.
    pub vowifi_child_handles: Vec<ChildHandle>,
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
    pub volte_bridge_supervisor: Option<ChildHandle>,
}

#[derive(Debug, Clone)]
pub struct LegacyVolteRegistration {
    pub supervisor_handle: ChildHandle,
    pub bridge_inbound: bool,
    pub restore_cid: Option<String>,
}

const GSM_SIP_BRIDGE_BIN: &str = "gsm-sip-bridge";
/// Matches the current trap's `for _ in $(seq 1 20); do ...; sleep 0.25; done`
/// — a ~5s bound so a zombie process cannot hang shutdown.
const KILL_CONFIRM_MAX_POLLS: u32 = 20;

/// Builds the exact, ordered teardown step sequence for this run — pure,
/// callable with zero real processes for testing (see `mod tests` below).
pub fn build_shutdown_plan(state: &StartedState, config_path: &str) -> Vec<TeardownStep> {
    let mut steps = Vec::new();

    // --- VoWiFi + the two always-present supervisors, killed first --------
    if let Some(h) = state.daemon_supervisor {
        steps.push(TeardownStep::KillChild {
            handle: h,
            signal: Signal::Term,
        });
    }
    if let Some(h) = state.sip_agent_supervisor {
        steps.push(TeardownStep::KillChild {
            handle: h,
            signal: Signal::Term,
        });
    }
    for &h in &state.vowifi_child_handles {
        steps.push(TeardownStep::KillChild {
            handle: h,
            signal: Signal::Term,
        });
    }

    // --- Legacy single-line VoLTE registration -----------------------------
    // SIGKILL, not SIGTERM: the child may be blocked mid-AT-transaction on
    // the modem's serial port, and only an unblockable kill guarantees the
    // kernel closes that fd *now*, before `volte-pdn down` reopens the port.
    if let Some(legacy) = &state.legacy_volte_registration {
        steps.push(TeardownStep::KillChild {
            handle: legacy.supervisor_handle,
            signal: Signal::Kill,
        });
        steps.push(TeardownStep::WaitForExit {
            handle: legacy.supervisor_handle,
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
        if let Some(h) = state.volte_bridge_supervisor {
            steps.push(TeardownStep::KillChild {
                handle: h,
                signal: Signal::Kill,
            });
        }
        for line in &state.volte_lines {
            for &h in &line.carrier_agent_handles {
                steps.push(TeardownStep::KillChild {
                    handle: h,
                    signal: Signal::Kill,
                });
            }
        }
        for line in &state.volte_lines {
            for &h in &line.carrier_agent_handles {
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
    }

    // --- Shared pcscd, then every namespace this run created ---------------
    if let Some(h) = state.pcscd {
        steps.push(TeardownStep::KillChild {
            handle: h,
            signal: Signal::Term,
        });
    }
    for netns in &state.started_netns {
        steps.push(TeardownStep::DeleteNetns {
            netns: netns.clone(),
        });
    }

    steps
}

impl TeardownStep {
    fn execute(&self, runner: &dyn CommandRunner) {
        match self {
            TeardownStep::KillChild { handle, signal } => runner.signal(*handle, *signal),
            TeardownStep::WaitForExit { handle, max_polls } => {
                for _ in 0..*max_polls {
                    if !runner.is_alive(*handle) {
                        break;
                    }
                    runner.sleep(std::time::Duration::from_millis(250));
                }
            }
            TeardownStep::RunInNetns { netns, argv } => {
                let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                let _ = runner.run_in_netns(netns, &refs);
            }
            TeardownStep::Run { argv } => {
                let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                let _ = runner.run(&refs);
            }
            TeardownStep::DeleteNetns { netns } => {
                let _ = runner.run(&["ip", "netns", "del", netns]);
            }
        }
    }
}

/// Executes every step in order — the only place a real signal/command is
/// ever issued. Best-effort throughout, matching the current trap's
/// `... 2>/dev/null || true` convention: one step's failure must not stop the
/// rest of cleanup.
pub fn execute_shutdown_plan(steps: &[TeardownStep], runner: &dyn CommandRunner) {
    for step in steps {
        step.execute(runner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::runner::{ChildSpec, MockCommandRunner};

    fn handle(runner: &MockCommandRunner, n: u32) -> ChildHandle {
        // Each call returns a fresh handle; spawn N throwaway children to get
        // N distinct handles for a test's fixture.
        let mut h = runner.spawn(ChildSpec::new(["true"])).unwrap();
        for _ in 1..n {
            h = runner.spawn(ChildSpec::new(["true"])).unwrap();
        }
        h
    }

    #[test]
    fn every_lines_child_kill_precedes_its_own_pdn_or_namespace_teardown() {
        let runner = MockCommandRunner::new();
        let carrier = handle(&runner, 1);
        let state = StartedState {
            volte_lines: vec![StartedVolteLine {
                index: 0,
                netns: "volte0".to_string(),
                carrier_agent_handles: vec![carrier],
            }],
            started_netns: vec!["volte0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");

        let kill_pos = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::KillChild { handle: h, .. } if *h == carrier))
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
                carrier_agent_handles: vec![carrier],
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
                supervisor_handle: legacy_handle,
                bridge_inbound: false,
                restore_cid: None,
            }),
            volte_lines: vec![StartedVolteLine {
                index: 0,
                netns: "volte0".to_string(),
                carrier_agent_handles: vec![carrier_handle],
            }],
            started_netns: vec!["volte0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");

        for h in [legacy_handle, carrier_handle] {
            let kill_step = steps
                .iter()
                .find(|s| matches!(s, TeardownStep::KillChild { handle, .. } if *handle == h))
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
            daemon_supervisor: Some(daemon),
            sip_agent_supervisor: Some(sip_agent),
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        for h in [daemon, sip_agent] {
            let step = steps
                .iter()
                .find(|s| matches!(s, TeardownStep::KillChild { handle, .. } if *handle == h))
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
            pcscd: Some(pcscd),
            started_netns: vec!["ims0".to_string(), "ims1".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        let pcscd_pos = steps
            .iter()
            .position(|s| matches!(s, TeardownStep::KillChild { handle, .. } if *handle == pcscd))
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
        let steps =
            build_shutdown_plan(&StartedState::default(), "/etc/gsm-sip-bridge/config.toml");
        assert!(steps.is_empty());
    }

    #[test]
    fn legacy_volte_registration_restores_the_displaced_cid_when_recorded() {
        let runner = MockCommandRunner::new();
        let legacy_handle = handle(&runner, 1);
        let state = StartedState {
            legacy_volte_registration: Some(LegacyVolteRegistration {
                supervisor_handle: legacy_handle,
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
                supervisor_handle: legacy_handle,
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
            daemon_supervisor: Some(daemon),
            started_netns: vec!["ims0".to_string()],
            ..Default::default()
        };
        let steps = build_shutdown_plan(&state, "/etc/gsm-sip-bridge/config.toml");
        execute_shutdown_plan(&steps, &runner);

        assert_eq!(runner.signals_for(daemon), vec![Signal::Term]);
        let netns_deletes = runner.run_calls.lock().unwrap();
        assert!(netns_deletes
            .iter()
            .any(|argv| argv == &["ip", "netns", "del", "ims0"]));
    }
}
