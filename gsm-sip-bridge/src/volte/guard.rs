//! Stops VoWiFi and VoLTE registering at the same time.
//!
//! Both paths register **the same IMPU with the same IMEI-derived
//! `+sip.instance`**. The network therefore treats the second registration as
//! a re-registration of the first and tears the first binding down — the same
//! hazard `crate::vowifi::ims_mode` already documents for the modem's own IMS
//! stack, and the reason that module disables it before VoWiFi starts.
//!
//! Until now this was only a warning in the documentation. It is a real
//! correctness risk: nothing prevented a `volte-register` from silently
//! killing a live VoWiFi registration, and the failure is invisible from the
//! VoLTE side — the network accepts *our* REGISTER perfectly happily.
//!
//! Two separate protections here:
//!
//! 1. **Cross-path**: refuse to register over LTE while a VoWiFi agent is
//!    running. Detected by scanning `/proc` for the agent's command line —
//!    the same signal `docker/entrypoint.sh` uses (`pkill -f
//!    vowifi-ims-agent`), and unlike Agent A's status port it does not depend
//!    on veth/netns reachability.
//! 2. **Same-path**: a lock file so two `volte-register` invocations cannot
//!    fight over one SIM.
//!
//! Both are advisory and both can be overridden, because an operator
//! deliberately testing interference is a legitimate thing to do.

use std::path::{Path, PathBuf};

/// Process name fragment identifying a running VoWiFi Agent A.
const VOWIFI_AGENT_MARKER: &str = "vowifi-ims-agent";

/// Default path for the VoLTE registration lock.
pub const DEFAULT_LOCK_PATH: &str = "/tmp/volte-registration.lock";

/// True when an argv belongs to a VoWiFi Agent A.
///
/// Matches on argv *structure*, not on the raw command line containing the
/// name anywhere. A substring test looks equivalent and is not: a shell
/// running a script that merely mentions `vowifi-ims-agent` — including
/// `docker/entrypoint.sh`'s own `pkill -f vowifi-ims-agent` — matches it, and
/// then a perfectly legitimate VoLTE registration is refused for no reason.
/// That false positive showed up the first time this ran against a real
/// process list.
///
/// The rule: the process must be this binary (argv[0]), and must have been
/// invoked with the agent subcommand as a whole argument.
pub fn is_vowifi_agent_argv(args: &[String]) -> bool {
    let Some(argv0) = args.first() else {
        return false;
    };
    if !argv0.contains("gsm-sip-bridge") {
        return false;
    }
    // `exec -a "gsm-sip-bridge vowifi-ims-agent ..."` collapses the whole
    // invocation into argv[0]; supervisors do this, so accept it too.
    if argv0.split_whitespace().any(|t| t == VOWIFI_AGENT_MARKER) {
        return true;
    }
    args[1..].iter().any(|a| a == VOWIFI_AGENT_MARKER)
}

/// Splits a NUL-separated `/proc/<pid>/cmdline` into its arguments.
pub fn parse_proc_cmdline(raw: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(raw)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// PIDs of running VoWiFi agents.
///
/// Only sees processes in this PID namespace: a VoWiFi agent in a *different*
/// container is invisible here. That limit is why the check is advisory and
/// why the operator-facing message says what it checked.
pub fn vowifi_agent_pids() -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pids = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let raw = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
        if is_vowifi_agent_argv(&parse_proc_cmdline(&raw)) {
            pids.push(pid);
        }
    }
    pids
}

/// The message shown when a VoWiFi agent is in the way.
pub fn conflict_message(pids: &[u32]) -> String {
    format!(
        "a VoWiFi agent is running (pid {}); registering over LTE now would present the \
         same IMPU and the same IMEI-derived +sip.instance, so the network would treat one \
         registration as a re-registration of the other and tear the first binding down. \
         Stop the VoWiFi agent first, or pass --force if you are deliberately testing this.",
        pids.iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Refuses to proceed while a VoWiFi agent holds a registration.
pub fn check_no_vowifi_conflict(force: bool) -> Result<(), String> {
    let pids = vowifi_agent_pids();
    if pids.is_empty() {
        return Ok(());
    }
    if force {
        tracing::warn!(
            pids = ?pids,
            "a VoWiFi agent is running and --force was given; the two registrations will \
             displace each other"
        );
        return Ok(());
    }
    Err(conflict_message(&pids))
}

/// True when a PID is still alive in this namespace.
fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Parses a lock file's contents into the PID that wrote it.
pub fn parse_lock(contents: &str) -> Option<u32> {
    contents.trim().parse().ok()
}

/// Held for the lifetime of a VoLTE registration; releases on drop.
#[derive(Debug)]
pub struct RegistrationGuard {
    path: PathBuf,
}

impl RegistrationGuard {
    /// Takes the VoLTE registration lock.
    ///
    /// A lock left behind by a crashed process is taken over rather than
    /// blocking forever — a stale file must not require manual cleanup before
    /// the feature works again.
    pub fn acquire(path: &Path) -> Result<Self, String> {
        if let Ok(existing) = std::fs::read_to_string(path) {
            match parse_lock(&existing) {
                Some(pid) if pid_alive(pid) => {
                    return Err(format!(
                        "another volte-register is already running (pid {pid}); \
                         two registrations for one SIM would displace each other"
                    ));
                }
                Some(pid) => {
                    tracing::debug!(pid, "taking over a stale VoLTE registration lock");
                }
                None => {}
            }
        }
        std::fs::write(path, std::process::id().to_string()).map_err(|e| {
            format!(
                "could not take the registration lock at {}: {e}",
                path.display()
            )
        })?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        // Only remove the lock if it is still ours — a takeover by another
        // process must not be undone by our exit.
        if let Ok(contents) = std::fs::read_to_string(&self.path) {
            if parse_lock(&contents) != Some(std::process::id()) {
                return;
            }
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn recognises_a_running_vowifi_agent() {
        assert!(is_vowifi_agent_argv(&argv(&[
            "/usr/local/bin/gsm-sip-bridge",
            "vowifi-ims-agent",
            "--line",
            "0"
        ])));
    }

    #[test]
    fn recognises_an_agent_launched_with_a_rewritten_argv0() {
        // Supervisors using `exec -a` collapse the invocation into argv[0].
        assert!(is_vowifi_agent_argv(&argv(&[
            "gsm-sip-bridge vowifi-ims-agent --line 0"
        ])));
    }

    #[test]
    fn a_shell_that_merely_mentions_the_agent_is_not_the_agent() {
        // Found against a real process list: a substring test matched the test
        // harness's own shell and refused a legitimate registration. It would
        // equally match entrypoint.sh's `pkill -f vowifi-ims-agent`.
        for other in [
            argv(&[
                "bash",
                "-c",
                "gsb volte-register; pkill -f vowifi-ims-agent",
            ]),
            argv(&["/bin/sh", "/entrypoint.sh"]),
            argv(&["grep", "vowifi-ims-agent", "/var/log/bridge.log"]),
            argv(&["pkill", "-f", "vowifi-ims-agent"]),
        ] {
            assert!(
                !is_vowifi_agent_argv(&other),
                "false positive on: {other:?}"
            );
        }
    }

    #[test]
    fn an_empty_argv_is_not_the_agent() {
        assert!(!is_vowifi_agent_argv(&[]));
    }

    #[test]
    fn proc_cmdline_splits_on_nul_and_drops_empties() {
        let raw = b"/usr/bin/gsm-sip-bridge\0vowifi-ims-agent\0--line\00\0";

        let args = parse_proc_cmdline(raw);

        assert_eq!(
            args,
            vec!["/usr/bin/gsm-sip-bridge", "vowifi-ims-agent", "--line", "0"]
        );
        assert!(is_vowifi_agent_argv(&args));
    }

    #[test]
    fn does_not_mistake_other_subcommands_for_the_agent() {
        // A false positive here would block a legitimate VoLTE registration.
        for other in [
            argv(&[
                "/usr/local/bin/gsm-sip-bridge",
                "volte-register",
                "--pcscf",
                "2402::1",
            ]),
            argv(&["/usr/local/bin/gsm-sip-bridge", "vowifi-status"]),
            argv(&["/usr/local/bin/gsm-sip-bridge", "vowifi-sip-agent"]),
            argv(&[
                "/usr/local/bin/gsm-sip-bridge",
                "vowifi-usim-bridge",
                "--modem",
                "/dev/ttyUSB6",
            ]),
        ] {
            assert!(!is_vowifi_agent_argv(&other), "should not match: {other:?}");
        }
    }

    #[test]
    fn conflict_message_names_the_pids_and_the_way_out() {
        let m = conflict_message(&[42, 43]);

        assert!(m.contains("42, 43"));
        assert!(m.contains("--force"), "must say how to override");
        assert!(m.contains("+sip.instance"), "must say why it matters");
    }

    #[test]
    fn the_test_runner_itself_is_never_seen_as_an_agent() {
        let own: Vec<String> = std::env::args().collect();

        assert!(!is_vowifi_agent_argv(&own), "own argv: {own:?}");
    }

    #[test]
    fn force_overrides_the_conflict_check() {
        // Whatever the machine state, --force must never refuse.
        assert!(check_no_vowifi_conflict(true).is_ok());
    }

    #[test]
    fn lock_is_taken_and_released() {
        let path = std::env::temp_dir().join(format!("volte-lock-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        {
            let _guard = RegistrationGuard::acquire(&path).expect("should acquire");
            assert!(path.exists());
        }

        assert!(!path.exists(), "lock must be released on drop");
    }

    #[test]
    fn a_live_holder_blocks_a_second_registration() {
        let path = std::env::temp_dir().join(format!("volte-lock-live-{}", std::process::id()));
        // Our own PID is alive by definition.
        std::fs::write(&path, std::process::id().to_string()).unwrap();

        let err = RegistrationGuard::acquire(&path).unwrap_err();

        assert!(err.contains("already running"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_stale_lock_is_taken_over_rather_than_blocking_forever() {
        let path = std::env::temp_dir().join(format!("volte-lock-stale-{}", std::process::id()));
        // PID 0 never exists as a /proc entry.
        std::fs::write(&path, "0").unwrap();

        let guard = RegistrationGuard::acquire(&path);

        assert!(guard.is_ok(), "a crashed run must not need manual cleanup");
        drop(guard);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_corrupt_lock_file_does_not_block() {
        let path = std::env::temp_dir().join(format!("volte-lock-corrupt-{}", std::process::id()));
        std::fs::write(&path, "not-a-pid").unwrap();

        assert!(RegistrationGuard::acquire(&path).is_ok());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn parse_lock_reads_a_pid_and_rejects_junk() {
        assert_eq!(parse_lock("1234\n"), Some(1234));
        assert_eq!(parse_lock(""), None);
        assert_eq!(parse_lock("abc"), None);
    }
}
