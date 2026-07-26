//! Supervision for the always-on circuit-switched GSM-to-SIP daemon
//! (specs/021-entrypoint-supervise-rust Phase 3) — the simplest of the four
//! loops this feature moves out of bash: spawn, wait for exit, log, sleep,
//! respawn, forever. Ported 1:1 from `entrypoint.sh`'s
//! `while true; do "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG"; ...; sleep 5; done`.
//!
//! Moved into Rust per the 2026-07-26 clarification (spec Clarifications):
//! `entrypoint.sh` retains no supervision loops of any kind once this
//! feature's phases land, including this one.

use super::runner::{ChildHandle, ChildSpec, CommandRunner};
use std::time::Duration;

/// Matches the current script's `sleep 5` between a daemon exit and its
/// respawn.
pub const RESTART_DELAY: Duration = Duration::from_secs(5);

/// One iteration's outcome — returned rather than looping forever internally,
/// so callers (both `supervise::mod`'s real event loop and tests) can drive
/// exactly one spawn-wait-restart cycle without an actual `sleep`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The daemon exited; a caller should sleep `RESTART_DELAY` then spawn
    /// again.
    ExitedWillRestart { status: Option<i32> },
}

/// Spawns `argv` once and waits for it to exit, returning the outcome.
/// Blocking — matches the current script's own blocking `while true` body
/// (the loop itself runs on its own supervisor thread, mirroring the
/// existing `std::thread::spawn` convention used everywhere else in this
/// codebase for equivalent long-lived supervision work).
pub fn run_once(runner: &dyn CommandRunner, argv: &[&str]) -> std::io::Result<Outcome> {
    let handle = spawn_and_wait(runner, argv)?;
    Ok(Outcome::ExitedWillRestart {
        status: runner.wait(handle),
    })
}

fn spawn_and_wait(runner: &dyn CommandRunner, argv: &[&str]) -> std::io::Result<ChildHandle> {
    runner.spawn(ChildSpec::new(argv.iter().copied()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::runner::MockCommandRunner;

    // MOCK JUSTIFICATION (constitution Principle I): stands in for the real
    // gsm-sip-bridge daemon child process — a real one would mean this test
    // suite spawning itself recursively. The decision logic under test (spawn
    // once, wait, report the outcome) is real production code.
    #[test]
    fn run_once_spawns_exactly_the_given_argv_and_reports_an_exit_outcome() {
        let runner = MockCommandRunner::new();
        let argv = [
            "gsm-sip-bridge",
            "--config",
            "/etc/gsm-sip-bridge/config.toml",
        ];

        let outcome = run_once(&runner, &argv).unwrap();

        let Outcome::ExitedWillRestart { .. } = outcome;
        let specs = runner.spawn_specs.lock().unwrap();
        assert_eq!(specs.len(), 1, "exactly one spawn per run_once call");
        assert_eq!(
            specs[0].argv,
            argv.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn restart_delay_matches_the_original_scripts_five_second_sleep() {
        assert_eq!(RESTART_DELAY, Duration::from_secs(5));
    }
}
