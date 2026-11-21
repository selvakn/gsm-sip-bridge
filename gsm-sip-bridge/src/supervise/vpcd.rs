//! Shared pcscd + vpcd reader startup and readiness gate
//! (specs/021-entrypoint-supervise-rust Phase 4), ported from the
//! one-shared-pcscd block the entrypoint script used to carry. One pcscd
//! instance serves every strongswan-engine line's SIM through one vpcd reader
//! with N slots; each line's `vowifi-usim-bridge` connects to its own slot's
//! port. Driven from `supervise::orchestrate`, which decides whether a vpcd
//! reader is needed at all — an all-card-reader deployment needs none, since
//! pcscd picks a physical reader up from USB itself. Such a deployment can
//! also use a pcscd the host already runs instead (`[vowifi].pcscd`).

use super::runner::{ChildHandle, ChildSpec, CommandRunner};
use crate::config::PcscdMode;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Matches the original script's `for _ in $(seq 1 20); do ...; sleep 0.5; done`
/// readiness poll — a ~10s bound.
const READY_POLL_ATTEMPTS: u32 = 20;
const READY_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Matches the daemon-style `sleep 5` between a pcscd exit and its respawn.
pub const PCSCD_RESTART_DELAY: Duration = Duration::from_secs(5);
/// How many times `start_pcscd_with_retries` will (re)spawn pcscd and wait
/// for the vpcd reader before giving up — bounds startup at
/// `PCSCD_STARTUP_ATTEMPTS * (READY_POLL bound + PCSCD_RESTART_DELAY)`,
/// roughly a minute, rather than either failing on the very first hiccup or
/// hanging forever on a genuinely misconfigured port (github.com/selvakn/
/// gsm-sip-bridge/issues/53).
pub const PCSCD_STARTUP_ATTEMPTS: u32 = 4;

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
    /// pcscd is running but never put up a control socket of its own that
    /// accepts a connection, so no PC/SC client could reach it yet.
    SocketNotReady,
    /// Neither of the above, but the vpcd port never answered within the
    /// poll window.
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

/// pcsc-lite's default control socket, used by pcscd and every PC/SC client
/// (charon's `eap-sim-pcsc`, the IMS agent, `vowifi-plmn`).
pub const PCSCD_SOCKET_DEFAULT: &str = "/run/pcscd/pcscd.comm";

/// The control socket the PC/SC clients this supervisor starts will dial:
/// the client library honours `PCSCLITE_CSOCK_NAME`, so the probe does too.
pub fn pcscd_socket_path() -> PathBuf {
    match std::env::var_os("PCSCLITE_CSOCK_NAME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(PCSCD_SOCKET_DEFAULT),
    }
}

/// Whether a pcscd we spawned is serving: its own control socket is in
/// place and accepts a connection. "Its own" matters on a host whose
/// systemd `pcscd.socket` already listens on the same path: until our pcscd
/// replaces that file, a connect would reach systemd instead (and activate
/// the host's pcscd, which then competes for the reader). So the file must
/// differ from the one that was there before the spawn, and only then is it
/// dialled. A path that was empty before the spawn can only have been
/// filled by our pcscd.
fn own_socket_ready(
    runner: &dyn CommandRunner,
    socket: &Path,
    before_spawn: Option<(u64, u64)>,
) -> bool {
    let now = runner.path_identity(socket);
    let replaced = match before_spawn {
        None => true,
        Some(old) => now.is_some() && now != Some(old),
    };
    replaced && runner.unix_connect_ok(socket)
}

/// Polls until a pcscd we spawned is ready for clients: still alive, its
/// control socket up (see [`own_socket_ready`]) and, if `vpcd` is given,
/// its vpcd driver bound without logging a failure and the base slot's port
/// answering a TCP connect. The vpcd checks keep the original script's
/// order (alive, log grep, connect) and its ~10s bound.
///
/// Before the socket check existed, a card-reader-only deployment counted
/// as ready the moment pcscd was spawned, and the first line's `vowifi-plmn`
/// card read, 50 ms later, found no pcscd to talk to. That line was then
/// skipped for the rest of the run.
pub fn wait_for_pcscd_ready(
    runner: &dyn CommandRunner,
    pcscd: &ChildHandle,
    socket: &Path,
    before_spawn: Option<(u64, u64)>,
    vpcd: Option<(&str, u16)>,
) -> ReadyOutcome {
    let mut socket_ready = false;
    for _ in 0..READY_POLL_ATTEMPTS {
        if !runner.is_alive(pcscd) {
            return ReadyOutcome::PcscdDied;
        }
        if vpcd.is_some() {
            if let Ok(log) = runner.read_file(Path::new(PCSCD_LOG_PATH)) {
                if driver_logged_bind_failure(&log) {
                    return ReadyOutcome::DriverBindFailed;
                }
            }
        }
        socket_ready = socket_ready || own_socket_ready(runner, socket, before_spawn);
        if socket_ready {
            match vpcd {
                None => return ReadyOutcome::Ready,
                Some((host, port)) if runner.tcp_connect_ok(host, port) => {
                    return ReadyOutcome::Ready;
                }
                Some(_) => {}
            }
        }
        runner.sleep(READY_POLL_INTERVAL);
    }
    if socket_ready {
        ReadyOutcome::TimedOut
    } else {
        ReadyOutcome::SocketNotReady
    }
}

/// Why [`start_pcscd`] gave up. Keeps "the process never even spawned"
/// (never retried; the caller has its own fatal message for it) apart from
/// "it spawned but never became ready" (the readiness-gate outcome the
/// retries apply to), and both apart from the two ways the host's pcscd
/// can be unusable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPcscdError {
    SpawnFailed,
    NotReady(ReadyOutcome),
    /// `[vowifi].pcscd = "external"`, but a resolved line is modem-backed
    /// and needs the vpcd reader only the bridge's own pcscd has.
    ExternalCannotServeVpcd,
    /// `[vowifi].pcscd = "external"`, but nothing accepted a connection on
    /// the control socket within the poll window.
    ExternalUnreachable,
}

/// The pcscd this pass's lines use.
#[derive(Debug, PartialEq, Eq)]
pub enum Pcscd {
    /// One the supervisor spawned and owns: torn down with the run.
    Managed(ChildHandle),
    /// The host's own (systemd's `pcscd.socket`, typically). The bridge
    /// never signals or tears it down.
    External,
}

/// Picks who runs pcscd for this pass (see [`PcscdMode`]). `Auto` uses the
/// host's pcscd only when no line needs vpcd and something already accepts
/// a connection on the control socket. A container has nothing there, so it
/// keeps spawning its own exactly as before; a stale socket file left by a
/// dead pcscd refuses the connect and so does not count either.
///
/// The probe can activate a systemd-managed pcscd. That is the pcscd `Auto`
/// is about to use anyway, and nothing is probed when vpcd is needed.
fn use_external(
    runner: &dyn CommandRunner,
    mode: PcscdMode,
    needs_vpcd: bool,
    socket: &Path,
) -> Result<bool, StartPcscdError> {
    match mode {
        PcscdMode::Managed => Ok(false),
        PcscdMode::External if needs_vpcd => Err(StartPcscdError::ExternalCannotServeVpcd),
        PcscdMode::External => Ok(true),
        PcscdMode::Auto => Ok(!needs_vpcd && runner.unix_connect_ok(socket)),
    }
}

/// Makes a pcscd available to this pass's lines, per `mode`, and waits until
/// it serves clients (and, if `needs_vpcd`, its vpcd reader).
///
/// A managed pcscd is spawned and gated by [`wait_for_pcscd_ready`],
/// retrying the whole spawn+wait cycle up to [`PCSCD_STARTUP_ATTEMPTS`]
/// times rather than failing on the first attempt: a force-killed
/// container's old pcscd can still be holding `vpcd_host:vpcd_port` (a
/// host-shared port under `network_mode: host`) for a brief window after
/// `docker kill` returns, so the very next `docker start` can race it
/// (github.com/selvakn/gsm-sip-bridge/issues/53). Between attempts the
/// failed pcscd is reaped (not just signalled — see
/// [`CommandRunner::reap`]'s own doc comment on why a bare signal leaks a
/// tracked-children entry) so the next attempt isn't contending with it.
///
/// An external pcscd is only waited for: its socket must accept a connect
/// within the same ~10s bound. A spawn failure (pcscd never starts at all)
/// is never retried.
pub fn start_pcscd(
    runner: &dyn CommandRunner,
    mode: PcscdMode,
    needs_vpcd: bool,
    socket: &Path,
    vpcd_host: &str,
    vpcd_port: u16,
) -> Result<Pcscd, StartPcscdError> {
    if use_external(runner, mode, needs_vpcd, socket)? {
        for attempt in 1..=READY_POLL_ATTEMPTS {
            if runner.unix_connect_ok(socket) {
                return Ok(Pcscd::External);
            }
            if attempt < READY_POLL_ATTEMPTS {
                runner.sleep(READY_POLL_INTERVAL);
            }
        }
        return Err(StartPcscdError::ExternalUnreachable);
    }
    let vpcd = needs_vpcd.then_some((vpcd_host, vpcd_port));
    for attempt in 1..=PCSCD_STARTUP_ATTEMPTS {
        let before_spawn = runner.path_identity(socket);
        let handle = spawn_pcscd(runner).ok_or(StartPcscdError::SpawnFailed)?;
        let outcome = wait_for_pcscd_ready(runner, &handle, socket, before_spawn, vpcd);
        if outcome == ReadyOutcome::Ready {
            return Ok(Pcscd::Managed(handle));
        }
        // Reaped on every failure, not just a retried one: the caller's own
        // FATAL exit on the final attempt runs no shutdown/teardown plan at
        // all (`orchestrate::run` returns `ExitCode::FAILURE` straight from
        // this call), so a handle left unreaped here would still be alive
        // and holding the port when the container exits — recreating the
        // exact race this function exists to avoid, for whatever tries to
        // start next.
        runner.reap(&handle);
        if attempt < PCSCD_STARTUP_ATTEMPTS {
            println!(
                "[supervise] pcscd not ready (attempt {attempt}/{PCSCD_STARTUP_ATTEMPTS}, {outcome:?}); retrying in {PCSCD_RESTART_DELAY:?}"
            );
            runner.sleep(PCSCD_RESTART_DELAY);
        } else {
            return Err(StartPcscdError::NotReady(outcome));
        }
    }
    unreachable!("loop always returns by the final attempt")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::runner::{MockCommandRunner, Signal};

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

    const SOCK: &str = "/run/pcscd/pcscd.comm";

    fn sock() -> &'static Path {
        Path::new(SOCK)
    }

    fn vpcd() -> Option<(&'static str, u16)> {
        Some(("127.0.0.1", 15963))
    }

    #[test]
    fn ready_as_soon_as_the_socket_and_the_vpcd_port_answer() {
        let runner = MockCommandRunner::new();
        let pcscd = runner.spawn(ChildSpec::new(["pcscd"])).unwrap();
        runner.set_unix_connect_ok(SOCK, true);
        runner.set_tcp_connect_ok("127.0.0.1", 15963, true);
        assert_eq!(
            wait_for_pcscd_ready(&runner, &pcscd, sock(), None, vpcd()),
            ReadyOutcome::Ready
        );
    }

    #[test]
    fn pcscd_dying_is_reported_immediately_not_as_a_timeout() {
        let runner = MockCommandRunner::new();
        let pcscd = runner.spawn(ChildSpec::new(["pcscd"])).unwrap();
        runner.kill_child(&pcscd, 1);
        assert_eq!(
            wait_for_pcscd_ready(&runner, &pcscd, sock(), None, vpcd()),
            ReadyOutcome::PcscdDied
        );
    }

    #[test]
    fn a_bind_failure_in_the_log_is_reported_distinctly_from_a_plain_timeout() {
        let runner = MockCommandRunner::new();
        let pcscd = runner.spawn(ChildSpec::new(["pcscd"])).unwrap();
        runner.set_file(std::path::Path::new(PCSCD_LOG_PATH), "Address in use\n");
        assert_eq!(
            wait_for_pcscd_ready(&runner, &pcscd, sock(), None, vpcd()),
            ReadyOutcome::DriverBindFailed
        );
    }

    #[test]
    fn a_vpcd_port_that_never_answers_times_out_after_the_poll_bound() {
        let runner = MockCommandRunner::new();
        let pcscd = runner.spawn(ChildSpec::new(["pcscd"])).unwrap();
        runner.set_unix_connect_ok(SOCK, true);
        assert_eq!(
            wait_for_pcscd_ready(&runner, &pcscd, sock(), None, vpcd()),
            ReadyOutcome::TimedOut
        );
        assert_eq!(
            runner.sleeps.lock().unwrap().len() as u32,
            READY_POLL_ATTEMPTS
        );
    }

    #[test]
    fn a_card_reader_only_pcscd_is_not_ready_until_its_socket_accepts() {
        // The 2026-11-18 restart: with no vpcd to wait for, pcscd counted as
        // ready the moment it was spawned, and the first line's card read
        // found no pcscd to talk to. A socket that never accepts is now a
        // readiness failure of its own, not success.
        let runner = MockCommandRunner::new();
        let pcscd = runner.spawn(ChildSpec::new(["pcscd"])).unwrap();
        assert_eq!(
            wait_for_pcscd_ready(&runner, &pcscd, sock(), None, None),
            ReadyOutcome::SocketNotReady
        );
        assert_eq!(
            runner.sleeps.lock().unwrap().len() as u32,
            READY_POLL_ATTEMPTS
        );

        let runner = MockCommandRunner::new();
        let pcscd = runner.spawn(ChildSpec::new(["pcscd"])).unwrap();
        runner.set_unix_connect_ok(SOCK, true);
        assert_eq!(
            wait_for_pcscd_ready(&runner, &pcscd, sock(), None, None),
            ReadyOutcome::Ready
        );
        assert!(runner.sleeps.lock().unwrap().is_empty());
    }

    #[test]
    fn a_socket_file_that_was_there_before_the_spawn_is_never_dialled() {
        // systemd's pcscd.socket already listens on the path: dialling it
        // would "succeed" before our pcscd is up, and activate the host's
        // pcscd as a competitor for the reader.
        let runner = MockCommandRunner::new();
        let pcscd = runner.spawn(ChildSpec::new(["pcscd"])).unwrap();
        runner.set_unix_connect_ok(SOCK, true);
        runner.set_path_identities(SOCK, vec![Some((1, 100))]);
        assert_eq!(
            wait_for_pcscd_ready(&runner, &pcscd, sock(), Some((1, 100)), None),
            ReadyOutcome::SocketNotReady
        );
        assert!(runner.unix_connect_attempts.lock().unwrap().is_empty());
    }

    #[test]
    fn a_socket_file_replaced_by_our_pcscd_is_dialled_once_it_changes() {
        let runner = MockCommandRunner::new();
        let pcscd = runner.spawn(ChildSpec::new(["pcscd"])).unwrap();
        runner.set_unix_connect_ok(SOCK, true);
        // Unchanged, briefly gone while pcscd rebinds, then a new inode.
        runner.set_path_identities(SOCK, vec![Some((1, 100)), None, Some((1, 101))]);
        assert_eq!(
            wait_for_pcscd_ready(&runner, &pcscd, sock(), Some((1, 100)), None),
            ReadyOutcome::Ready
        );
        assert_eq!(runner.unix_connect_attempts.lock().unwrap().len(), 1);
        assert_eq!(runner.sleeps.lock().unwrap().len(), 2);
    }

    // start_pcscd: "fails once, succeeds on retry" is not exercised
    // end-to-end here — every attempt spawns pcscd with the same argv, and
    // MockCommandRunner has no way to make one spawn's readiness differ from
    // the next spawn of the same argv. The pieces that behavior is built from
    // (`wait_for_pcscd_ready`'s outcomes above, and `reap`'s own
    // conformance-tested semantics) are already covered; what's left to test
    // here is the loop's own contract and the mode decision.

    #[test]
    fn managed_ready_on_first_attempt_returns_immediately_with_no_retry() {
        let runner = MockCommandRunner::new();
        runner.set_unix_connect_ok(SOCK, true);
        runner.set_tcp_connect_ok("127.0.0.1", 15963, true);
        let result = start_pcscd(
            &runner,
            PcscdMode::Managed,
            true,
            sock(),
            "127.0.0.1",
            15963,
        );
        assert!(matches!(result, Ok(Pcscd::Managed(_))), "{result:?}");
        assert_eq!(runner.spawn_specs.lock().unwrap().len(), 1);
        assert!(runner.sleeps.lock().unwrap().is_empty(), "no retry needed");
    }

    #[test]
    fn managed_without_vpcd_still_waits_for_the_socket_and_fails_if_it_never_comes() {
        let runner = MockCommandRunner::new();
        let result = start_pcscd(
            &runner,
            PcscdMode::Managed,
            false,
            sock(),
            "127.0.0.1",
            15963,
        );
        assert_eq!(
            result,
            Err(StartPcscdError::NotReady(ReadyOutcome::SocketNotReady))
        );
        assert_eq!(
            runner.spawn_specs.lock().unwrap().len() as u32,
            PCSCD_STARTUP_ATTEMPTS
        );
    }

    #[test]
    fn exhausts_every_attempt_before_giving_up() {
        let runner = MockCommandRunner::new();
        // Every pcscd spawned this way is dead on arrival, so every attempt
        // hits `ReadyOutcome::PcscdDied` — the loop's own retry/backoff
        // machinery is what's under test, not any one outcome variant.
        runner.set_born_dead_if_argv_contains("pcscd");
        let result = start_pcscd(
            &runner,
            PcscdMode::Managed,
            true,
            sock(),
            "127.0.0.1",
            15963,
        );
        assert_eq!(
            result,
            Err(StartPcscdError::NotReady(ReadyOutcome::PcscdDied))
        );
        assert_eq!(
            runner.spawn_specs.lock().unwrap().len() as u32,
            PCSCD_STARTUP_ATTEMPTS,
            "one spawn per attempt"
        );
        assert_eq!(
            runner.sleeps.lock().unwrap().len() as u32,
            PCSCD_STARTUP_ATTEMPTS - 1,
            "no backoff sleep after the final, already-fatal attempt"
        );
        // Every attempt's pcscd is reaped, including the last: this function
        // is called right before a FATAL exit that runs no shutdown/teardown
        // plan at all, so an unreaped final attempt would leave a real
        // process alive and holding the port for whatever starts next
        // (github.com/selvakn/gsm-sip-bridge/issues/53's own failure mode,
        // reintroduced).
        let children = runner.children.lock().unwrap();
        assert_eq!(children.len() as u32, PCSCD_STARTUP_ATTEMPTS);
        for child in children.values() {
            assert!(
                child.signals_received.contains(&Signal::Term),
                "every attempt, including the final one, must be reaped"
            );
        }
    }

    #[test]
    fn auto_uses_a_pcscd_the_host_already_serves_and_spawns_none() {
        let runner = MockCommandRunner::new();
        runner.set_unix_connect_ok(SOCK, true);
        let result = start_pcscd(&runner, PcscdMode::Auto, false, sock(), "127.0.0.1", 15963);
        assert_eq!(result, Ok(Pcscd::External));
        assert!(runner.spawn_specs.lock().unwrap().is_empty());
    }

    #[test]
    fn auto_spawns_its_own_when_nothing_serves_the_socket() {
        // A container, or a stale socket file nothing listens on.
        let runner = MockCommandRunner::new();
        let _ = start_pcscd(&runner, PcscdMode::Auto, false, sock(), "127.0.0.1", 15963);
        assert!(!runner.spawn_specs.lock().unwrap().is_empty());
    }

    #[test]
    fn auto_always_spawns_its_own_when_a_line_needs_vpcd() {
        // The host's pcscd has no vpcd reader, so its presence is irrelevant
        // and is not even probed for before the spawn.
        let runner = MockCommandRunner::new();
        runner.set_unix_connect_ok(SOCK, true);
        runner.set_tcp_connect_ok("127.0.0.1", 15963, true);
        runner.set_path_identities(SOCK, vec![Some((1, 100)), Some((1, 101))]);
        let result = start_pcscd(&runner, PcscdMode::Auto, true, sock(), "127.0.0.1", 15963);
        assert!(matches!(result, Ok(Pcscd::Managed(_))), "{result:?}");
        assert_eq!(runner.spawn_specs.lock().unwrap().len(), 1);
        assert_eq!(
            runner.unix_connect_attempts.lock().unwrap().len(),
            1,
            "only the post-spawn readiness probe of our own socket"
        );
    }

    #[test]
    fn managed_spawns_its_own_even_when_the_host_serves_one() {
        let runner = MockCommandRunner::new();
        runner.set_unix_connect_ok(SOCK, true);
        runner.set_path_identities(SOCK, vec![Some((1, 100)), Some((1, 101))]);
        let result = start_pcscd(
            &runner,
            PcscdMode::Managed,
            false,
            sock(),
            "127.0.0.1",
            15963,
        );
        assert!(matches!(result, Ok(Pcscd::Managed(_))), "{result:?}");
        assert_eq!(runner.spawn_specs.lock().unwrap().len(), 1);
    }

    #[test]
    fn external_refuses_a_line_that_needs_vpcd_without_spawning() {
        let runner = MockCommandRunner::new();
        runner.set_unix_connect_ok(SOCK, true);
        let result = start_pcscd(
            &runner,
            PcscdMode::External,
            true,
            sock(),
            "127.0.0.1",
            15963,
        );
        assert_eq!(result, Err(StartPcscdError::ExternalCannotServeVpcd));
        assert!(runner.spawn_specs.lock().unwrap().is_empty());
    }

    #[test]
    fn external_waits_for_the_hosts_socket_and_never_spawns() {
        let runner = MockCommandRunner::new();
        let result = start_pcscd(
            &runner,
            PcscdMode::External,
            false,
            sock(),
            "127.0.0.1",
            15963,
        );
        assert_eq!(result, Err(StartPcscdError::ExternalUnreachable));
        assert!(runner.spawn_specs.lock().unwrap().is_empty());
        assert_eq!(
            runner.unix_connect_attempts.lock().unwrap().len() as u32,
            READY_POLL_ATTEMPTS
        );
        assert_eq!(
            runner.sleeps.lock().unwrap().len() as u32,
            READY_POLL_ATTEMPTS - 1
        );

        let runner = MockCommandRunner::new();
        runner.set_unix_connect_ok(SOCK, true);
        let result = start_pcscd(
            &runner,
            PcscdMode::External,
            false,
            sock(),
            "127.0.0.1",
            15963,
        );
        assert_eq!(result, Ok(Pcscd::External));
        assert!(runner.spawn_specs.lock().unwrap().is_empty());
    }
}
