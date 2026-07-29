//! Shared pcscd + vpcd reader startup and readiness gate
//! (specs/021-entrypoint-supervise-rust Phase 4), ported from the
//! one-shared-pcscd block the entrypoint script used to carry. One pcscd
//! instance serves every strongswan-engine line's SIM through one vpcd reader
//! with N slots; each line's `vowifi-usim-bridge` connects to its own slot's
//! port. Driven from `supervise::orchestrate`, which decides whether a vpcd
//! reader is needed at all — an all-card-reader deployment needs none, since
//! pcscd picks a physical reader up from USB itself.

use super::runner::{ChildHandle, ChildSpec, CommandRunner};
use std::path::Path;
use std::time::Duration;

/// Matches the original script's `for _ in $(seq 1 20); do ...; sleep 0.5; done`
/// readiness poll — a ~10s bound.
const READY_POLL_ATTEMPTS: u32 = 20;
const READY_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Matches the daemon-style `sleep 5` between a pcscd exit and its respawn.
pub const PCSCD_RESTART_DELAY: Duration = Duration::from_secs(5);

const RENDER_CONF_PATH: &str = "/etc/reader.conf.d/vpcd";
const PCSCD_LOG_PATH: &str = "/tmp/pcscd.log";

/// Writes `/etc/reader.conf.d/vpcd` for `port`, matching
/// `render::render_vpcd_reader_conf`.
pub fn write_vpcd_reader_conf(runner: &dyn CommandRunner, port: u16) {
    let rendered = super::render::render_vpcd_reader_conf(port);
    let _ = runner.write_file(Path::new(RENDER_CONF_PATH), &rendered);
}

/// Spawns pcscd once (the caller is expected to supervise/respawn it via
/// `daemon_supervisor`-style looping, matching the original script's own
/// `while true; do pcscd --foreground ...; sleep 5; done`).
pub fn spawn_pcscd(runner: &dyn CommandRunner) -> Option<ChildHandle> {
    let _ = runner.run(&["mkdir", "-p", "/run/pcscd"]);
    runner
        .spawn(
            ChildSpec::new(["pcscd", "--foreground"])
                .capture_stdout_to(std::path::PathBuf::from(PCSCD_LOG_PATH)),
        )
        .ok()
}

/// Outcome of the readiness gate — mirrors the original script's `VPCD_READY`
/// plus its two distinct FATAL causes (pcscd died vs. the driver logging a
/// bind failure), so the caller can log the same specific guidance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyOutcome {
    Ready,
    /// pcscd itself is no longer running.
    PcscdDied,
    /// pcscd is running but its vpcd driver logged a bind failure (matches
    /// `grep -qiE "address in use|Open Port .* Failed"`).
    DriverBindFailed,
    /// Neither of the above, but the port never answered within the poll
    /// window.
    TimedOut,
}

/// 1:1 port of `grep -qiE "address in use|Open Port .* Failed"` — grep
/// matches per line, case-insensitively, and `Open Port .* Failed` requires
/// "Open Port" to appear before "Failed" *on that same line* (`.*` only
/// matches forward, the same ordering subtlety as `render`'s
/// `local_addrs.*@SRC_ADDR@` port — see that function's own regression test).
pub fn driver_logged_bind_failure(pcscd_log: &str) -> bool {
    pcscd_log.lines().any(|line| {
        let lower = line.to_lowercase();
        if lower.contains("address in use") {
            return true;
        }
        lower
            .find("open port")
            .is_some_and(|pos| lower[pos..].contains("failed"))
    })
}

/// Polls for the vpcd reader's readiness: pcscd still alive, no bind-failure
/// logged, and the base slot's port actually answers a TCP connect. Matches
/// the original script's own ordering (pcscd-alive check first, then the log
/// grep, then the connect probe) and its ~10s bound.
pub fn wait_for_vpcd_ready(
    runner: &dyn CommandRunner,
    pcscd: &ChildHandle,
    vpcd_host: &str,
    vpcd_port: u16,
) -> ReadyOutcome {
    for _ in 0..READY_POLL_ATTEMPTS {
        if !runner.is_alive(pcscd) {
            return ReadyOutcome::PcscdDied;
        }
        if let Ok(log) = runner.read_file(Path::new(PCSCD_LOG_PATH)) {
            if driver_logged_bind_failure(&log) {
                return ReadyOutcome::DriverBindFailed;
            }
        }
        if runner.tcp_connect_ok(vpcd_host, vpcd_port) {
            return ReadyOutcome::Ready;
        }
        runner.sleep(READY_POLL_INTERVAL);
    }
    ReadyOutcome::TimedOut
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::runner::MockCommandRunner;

    // MOCK JUSTIFICATION (constitution Principle I): stands in for real
    // pcscd/vpcd/a real TCP listener — none available in CI. The
    // ordering/decision logic under test is real production code.

    #[test]
    fn driver_logged_bind_failure_matches_address_in_use() {
        assert!(driver_logged_bind_failure("some line\nAddress in use\n"));
    }

    #[test]
    fn driver_logged_bind_failure_matches_open_port_failed_case_insensitively() {
        assert!(driver_logged_bind_failure("Open Port 0x3E5B Failed\n"));
    }

    #[test]
    fn driver_logged_bind_failure_false_on_clean_log() {
        assert!(!driver_logged_bind_failure("pcscd set up with 1 reader\n"));
    }

    #[test]
    fn driver_logged_bind_failure_requires_open_port_before_failed_on_the_same_line() {
        // "Failed" appearing before "Open Port" on the same line must NOT
        // match — grep's `.*` only matches forward, same ordering subtlety
        // as render.rs's local_addrs/@SRC_ADDR@ regression test.
        assert!(!driver_logged_bind_failure(
            "Failed to do something unrelated, then later mentions Open Port\n"
        ));
    }

    #[test]
    fn ready_as_soon_as_the_port_answers() {
        let runner = MockCommandRunner::new();
        let pcscd = runner.spawn(ChildSpec::new(["pcscd"])).unwrap();
        runner.set_tcp_connect_ok("127.0.0.1", 15963, true);
        assert_eq!(
            wait_for_vpcd_ready(&runner, &pcscd, "127.0.0.1", 15963),
            ReadyOutcome::Ready
        );
    }

    #[test]
    fn pcscd_dying_is_reported_immediately_not_as_a_timeout() {
        let runner = MockCommandRunner::new();
        let pcscd = runner.spawn(ChildSpec::new(["pcscd"])).unwrap();
        runner.kill_child(&pcscd, 1);
        assert_eq!(
            wait_for_vpcd_ready(&runner, &pcscd, "127.0.0.1", 15963),
            ReadyOutcome::PcscdDied
        );
    }

    #[test]
    fn a_bind_failure_in_the_log_is_reported_distinctly_from_a_plain_timeout() {
        let runner = MockCommandRunner::new();
        let pcscd = runner.spawn(ChildSpec::new(["pcscd"])).unwrap();
        runner.set_file(std::path::Path::new(PCSCD_LOG_PATH), "Address in use\n");
        assert_eq!(
            wait_for_vpcd_ready(&runner, &pcscd, "127.0.0.1", 15963),
            ReadyOutcome::DriverBindFailed
        );
    }

    #[test]
    fn never_ready_and_no_failure_signal_times_out_after_the_poll_bound() {
        let runner = MockCommandRunner::new();
        let pcscd = runner.spawn(ChildSpec::new(["pcscd"])).unwrap();
        assert_eq!(
            wait_for_vpcd_ready(&runner, &pcscd, "127.0.0.1", 15963),
            ReadyOutcome::TimedOut
        );
        assert_eq!(
            runner.sleeps.lock().unwrap().len() as u32,
            READY_POLL_ATTEMPTS
        );
    }
}
