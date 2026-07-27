//! Concrete [`TunnelEngine`] implementations (specs/021-entrypoint-supervise-rust
//! Phase 3) — `StrongswanEngine` (1:1 port of `start_line_strongswan`'s charon/
//! swanctl choreography) and `SwuEngine` (1:1 port of `start_line_swu`'s dialer
//! process). Every log-parsing check is a pure function, ported from the
//! current script's `grep`/`sed` pattern with the same fixture the original
//! bash comment used, tested independently of any [`CommandRunner`].

use super::line_supervisor::{SteadyStateHealth, TunnelEngine};
use super::runner::{ChildHandle, ChildSpec, CommandRunner};
use std::cell::RefCell;
use std::path::PathBuf;

/// 1:1 port of `grep -q "CHILD_SA.*established"`.
pub fn charon_log_shows_established(charon_log: &str) -> bool {
    charon_log.lines().any(|l| {
        l.contains("CHILD_SA")
            && l.find("CHILD_SA")
                .is_some_and(|pos| l[pos..].contains("established"))
    })
}

/// 1:1 port of the establish-time loop's P-CSCF extraction — same function
/// as `render`'s log-scraping sibling, `extract_latest_pcscf` (bash side,
/// pending its own port); duplicated here as a pure function rather than
/// imported, since this crate has no shared "bash-parity log parsers" module
/// yet — both read the identical `received P-CSCF server IP ...` marker.
pub fn extract_latest_pcscf(charon_log: &str) -> Option<String> {
    const MARKER: &str = "received P-CSCF server IP ";
    let is_valid_addr = |s: &str| {
        let is_v4 = s.split('.').count() == 4 && s.chars().all(|c| c.is_ascii_digit() || c == '.');
        let is_v6 = s.contains(':') && s.chars().all(|c| c.is_ascii_hexdigit() || c == ':');
        is_v4 || is_v6
    };
    charon_log
        .lines()
        // `grep -oE 'received P-CSCF server IP .*'` matches the marker
        // ANYWHERE in the line, not just at its start — real charon output
        // prefixes every line with a facility tag (`[CFG] received P-CSCF
        // server IP ...`), which `strip_prefix` (requiring an exact line
        // start) never matches. Caught live: this bug made the establish
        // loop treat every successful connection as "stuck without P-CSCF"
        // forever, so it kept terminating and re-initiating a perfectly
        // good tunnel every ~30s instead of ever reporting success.
        .filter_map(|l| l.find(MARKER).map(|pos| &l[pos + MARKER.len()..]))
        .map(str::trim)
        .rfind(|s| is_valid_addr(s))
        .map(str::to_string)
}

/// 1:1 port of `grep -q '^ims:' <<<"$sas_output"`.
pub fn list_sas_has_ims_child_sa(sas_output: &str) -> bool {
    sas_output.lines().any(|l| l.starts_with("ims:"))
}

/// One line's strongSwan engine state — the rendered per-line conf paths and
/// identifiers `start_line_strongswan` computed once at startup.
pub struct StrongswanEngine {
    pub idx: u32,
    pub strongswan_conf: String,
    pub swanctl_top_conf: String,
    pub charon_log: PathBuf,
    pub netns: String,
    pub tun_iface: String,
    /// This line's XFRM `if_id`, needed to recreate the interface after
    /// `SteadyStateHealth::TunVanished` (see `recreate_interface`).
    pub if_id: String,
    /// The currently-running charon process, if one has been spawned yet —
    /// `RefCell` because `TunnelEngine`'s methods take `&self` (the shared
    /// state-machine functions in `line_supervisor` don't need mutable
    /// access), but `restart_process` must replace this engine's own charon
    /// handle.
    pub charon_handle: RefCell<Option<ChildHandle>>,
}

impl StrongswanEngine {
    fn env_prefix(&self) -> String {
        format!("STRONGSWAN_CONF={}", self.strongswan_conf)
    }

    /// Blocking `swanctl` invocation — matches the bash calls with no
    /// trailing `&` (`--load-all`, `--terminate`), which the original script
    /// runs to completion before continuing.
    fn swanctl(&self, runner: &dyn CommandRunner, args: &[&str]) {
        let env = self.env_prefix();
        let mut argv = vec!["env", env.as_str(), "swanctl"];
        argv.extend_from_slice(args);
        let _ = runner.run(&argv);
    }

    /// Backgrounded `swanctl` invocation — matches the bash calls that end
    /// in `&` (every `--initiate`). Fire-and-forget: like the bash version,
    /// the spawned process's handle is never tracked or waited on (the
    /// script only ever redirects its output to a log file, `>>"...
    /// -initiate-$idx.log" 2>&1 &`, and never references its pid again).
    /// Not backgrounding this was a real bug caught by review: running
    /// `--initiate` synchronously via `run()` blocks the calling supervisor
    /// tick for as long as IKE negotiation takes, exactly the polling stall
    /// specs/012-strongswan-epdg's own establish/steady-state loops are
    /// designed to avoid by backgrounding it in the first place.
    fn swanctl_background(&self, runner: &dyn CommandRunner, args: &[&str]) {
        let env = self.env_prefix();
        let mut argv = vec!["env".to_string(), env, "swanctl".to_string()];
        argv.extend(args.iter().map(|s| s.to_string()));
        // spawn_detached, not spawn: a real review finding caught that
        // `spawn` + discarding the returned handle still leaves an
        // un-reapable entry in RealCommandRunner's table forever (nothing
        // holds the handle needed to remove it) — every repeated tunnel
        // recovery over a long-running container's lifetime would leak one.
        // `spawn_detached` never inserts a tracked entry at all, matching
        // this call's true fire-and-forget nature.
        let _ = runner.spawn_detached(ChildSpec::new(argv));
    }

    /// The full restart choreography, 1:1 port of both the "charon exited"
    /// and "vici connection broken" steady-state branches (identical body):
    /// clear the log, remove the stale unqualified pidfile (charon's own
    /// "already running" guard checks it regardless of this line's own
    /// `pidfile =` directive), respawn charon, `swanctl --load-all`, then
    /// `swanctl --initiate --child ims`.
    fn restart_charon(&self, runner: &dyn CommandRunner) {
        if let Some(old) = self.charon_handle.borrow_mut().take() {
            runner.signal(old, super::runner::Signal::Term);
        }
        let _ = runner.write_file(&self.charon_log, "");
        let _ = runner.run(&["rm", "-f", "/var/run/charon.pid"]);

        let env = self.env_prefix();
        // `env` must be argv[0] here, not the `STRONGSWAN_CONF=...` string
        // itself — a real bug review caught: RealCommandRunner passes
        // argv[0] straight to `Command::new`, so without this prefix the
        // spawn attempted to exec the environment assignment as a program
        // and failed with ENOENT, silently leaving no charon running while
        // the rest of the restart sequence (load-all, initiate) proceeded
        // as if it had.
        if let Ok(handle) = runner.spawn(
            ChildSpec::new(["env", env.as_str(), "/usr/libexec/ipsec/charon"])
                .capture_stdout_to(self.charon_log.clone()),
        ) {
            *self.charon_handle.borrow_mut() = Some(handle);
        }

        // 1:1 port of the current script's own `sleep 2 # let the vici
        // socket come up before swanctl talks to it` — present at every one
        // of its charon-respawn sites. Missing here (an FR-009 gap found
        // live, T049): `--load-all` issued before charon's vici socket is
        // listening silently fails (its result is deliberately ignored,
        // matching the script's own `|| true`), then `--initiate` fails
        // with "CHILD_SA config 'ims' not found" since nothing was ever
        // loaded — and steady-state's ChildSaMissing branch only ever
        // re-initiates, never reloads, so the tunnel never recovers on its
        // own afterward. Observed live: killing charon mid-session left the
        // line permanently stuck re-initiating against an empty vici
        // config every 30s, while the healthcheck kept passing on the
        // stale (pre-kill) tun23-0 address — a silently broken tunnel
        // reporting healthy.
        runner.sleep(std::time::Duration::from_secs(2));

        self.swanctl(runner, &["--load-all", "--file", &self.swanctl_top_conf]);
        self.swanctl_background(runner, &["--initiate", "--child", "ims"]);
    }
}

impl TunnelEngine for StrongswanEngine {
    fn is_tunnel_established(&self, runner: &dyn CommandRunner) -> bool {
        runner
            .read_file(&self.charon_log)
            .map(|c| charon_log_shows_established(&c))
            .unwrap_or(false)
    }

    fn latest_pcscf(&self, runner: &dyn CommandRunner) -> Option<String> {
        runner
            .read_file(&self.charon_log)
            .ok()
            .and_then(|c| extract_latest_pcscf(&c))
    }

    fn is_process_alive(&self, runner: &dyn CommandRunner) -> bool {
        self.charon_handle
            .borrow()
            .is_some_and(|h| runner.is_alive(h))
    }

    fn terminate(&self, runner: &dyn CommandRunner) {
        self.swanctl(runner, &["--terminate", "--ike", "ims"]);
    }

    fn reinitiate(&self, runner: &dyn CommandRunner) {
        self.swanctl_background(runner, &["--initiate", "--child", "ims"]);
    }

    fn steady_state_health(&self, runner: &dyn CommandRunner) -> SteadyStateHealth {
        let tun_check = runner.run_in_netns(&self.netns, &["ip", "link", "show", &self.tun_iface]);
        if tun_check.map(|o| !o.status.success()).unwrap_or(true) {
            println!(
                "[supervise] line {}: {} missing from netns {}; recreating and forcing reinitiate",
                self.idx, self.tun_iface, self.netns
            );
            return SteadyStateHealth::TunVanished;
        }

        let env = self.env_prefix();
        let sas_output = match runner.run(&["env", &env, "swanctl", "--list-sas"]) {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
            _ => {
                println!(
                    "[supervise] line {}: swanctl --list-sas failed (vici connection broken); restarting charon for this line only",
                    self.idx
                );
                return SteadyStateHealth::ViciBroken;
            }
        };
        if !list_sas_has_ims_child_sa(&sas_output) {
            println!(
                "[supervise] line {}: ims CHILD_SA missing; re-initiating",
                self.idx
            );
            return SteadyStateHealth::ChildSaMissing;
        }
        SteadyStateHealth::Ok
    }

    fn restart_process(&self, runner: &dyn CommandRunner) {
        self.restart_charon(runner);
    }

    fn max_establish_attempts(&self) -> Option<u32> {
        None
    }

    fn reinitiate_cadence(&self) -> Option<u32> {
        Some(super::line_supervisor::STRONGSWAN_REINITIATE_EVERY)
    }

    fn recreate_interface(&self, runner: &dyn CommandRunner) {
        super::epdg_iface::ensure_epdg_interface(runner, &self.netns, &self.tun_iface, &self.if_id);
    }
}

/// 1:1 port of `grep -q "STATE CONNECTED"`.
pub fn swu_log_shows_connected(log: &str) -> bool {
    log.contains("STATE CONNECTED")
}

/// 1:1 port of the swu dialer's P-CSCF extraction (`grep 'P-CSCF IPV4
/// ADDRESS'`, falling back to the IPV6 line only when no IPv4 line matched).
pub fn extract_swu_pcscf(log: &str) -> Option<String> {
    let v4 = log
        .lines()
        .find(|l| l.contains("P-CSCF IPV4 ADDRESS"))
        .and_then(|l| l.split_whitespace().find(|tok| tok.contains('.')));
    if let Some(addr) = v4 {
        return Some(addr.to_string());
    }
    log.lines()
        .find(|l| l.contains("P-CSCF IPV6 ADDRESS"))
        .and_then(|l| {
            l.split_whitespace().find(|tok| {
                // Must actually look like an IPv6 address (only hex digits
                // and colons) — a naive `tok.contains(':')` also matches the
                // literal word "ADDRESS:" earlier in the same line.
                tok.contains(':') && tok.chars().all(|c| c.is_ascii_hexdigit() || c == ':')
            })
        })
        .map(str::to_string)
}

/// One line's swu engine state.
pub struct SwuEngine {
    pub idx: u32,
    pub modem: String,
    pub apn: String,
    pub mcc: String,
    pub mnc: String,
    pub netns: String,
    pub src_addr: Option<String>,
    pub log_file: PathBuf,
    pub dialer_handle: RefCell<Option<ChildHandle>>,
}

impl SwuEngine {
    fn spawn_dialer(&self, runner: &dyn CommandRunner) -> Option<ChildHandle> {
        let mut argv = vec![
            "python3".to_string(),
            "-u".to_string(),
            "swu_emulator.py".to_string(),
            "-m".to_string(),
            self.modem.clone(),
            "-a".to_string(),
            self.apn.clone(),
            "-M".to_string(),
            self.mcc.clone(),
            "-N".to_string(),
            self.mnc.clone(),
            "-n".to_string(),
            self.netns.clone(),
        ];
        if let Some(src) = &self.src_addr {
            argv.push("-s".to_string());
            argv.push(src.clone());
        }
        runner
            .spawn(ChildSpec::new(argv).capture_stdout_to(self.log_file.clone()))
            .ok()
    }
}

impl TunnelEngine for SwuEngine {
    fn is_tunnel_established(&self, runner: &dyn CommandRunner) -> bool {
        runner
            .read_file(&self.log_file)
            .map(|c| swu_log_shows_connected(&c))
            .unwrap_or(false)
    }

    fn latest_pcscf(&self, runner: &dyn CommandRunner) -> Option<String> {
        runner
            .read_file(&self.log_file)
            .ok()
            .and_then(|c| extract_swu_pcscf(&c))
    }

    fn is_process_alive(&self, runner: &dyn CommandRunner) -> bool {
        self.dialer_handle
            .borrow()
            .is_some_and(|h| runner.is_alive(h))
    }

    // No in-place terminate/reinitiate concept for this engine (the current
    // script's own comment: "no re-initiate-in-place concept for this
    // engine — recovery is restarting the dialer for this line only").
    fn terminate(&self, _runner: &dyn CommandRunner) {}
    fn reinitiate(&self, _runner: &dyn CommandRunner) {}

    fn steady_state_health(&self, _runner: &dyn CommandRunner) -> SteadyStateHealth {
        // The swu steady-state loop has exactly one check (is the dialer
        // alive), already covered by `is_process_alive` before this is ever
        // consulted — so there is nothing left for this engine to report.
        SteadyStateHealth::Ok
    }

    fn restart_process(&self, runner: &dyn CommandRunner) {
        if let Some(old) = self.dialer_handle.borrow_mut().take() {
            runner.signal(old, super::runner::Signal::Term);
        }
        let _ = runner.write_file(&self.log_file, "");
        *self.dialer_handle.borrow_mut() = self.spawn_dialer(runner);
    }

    fn max_establish_attempts(&self) -> Option<u32> {
        Some(super::line_supervisor::SWU_MAX_ESTABLISH_ATTEMPTS)
    }

    fn reinitiate_cadence(&self) -> Option<u32> {
        None
    }

    fn recreate_interface(&self, _runner: &dyn CommandRunner) {
        // No pre-created interface concept for this engine — the dialer
        // manages its own tun device, and `restart_process` (a full dialer
        // respawn) is this engine's only recovery path, matching the
        // current script's own comment on `start_line_swu`.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charon_log_established_matches_the_grep_pattern() {
        assert!(charon_log_shows_established(
            "12[IKE] CHILD_SA ims{1} established with SPIs..."
        ));
        assert!(!charon_log_shows_established("nothing relevant"));
    }

    #[test]
    fn extract_latest_pcscf_picks_the_chronologically_last_valid_line() {
        let log = "received P-CSCF server IP 10.0.0.1\nreceived P-CSCF server IP 2001:db8::1\nreceived P-CSCF server IP 10.0.0.9\n";
        assert_eq!(extract_latest_pcscf(log), Some("10.0.0.9".to_string()));
    }

    #[test]
    fn extract_latest_pcscf_matches_real_charons_facility_tagged_log_lines() {
        // Regression test for a real bug found live: real charon output
        // prefixes every line with a facility tag (`[CFG] `, `[IKE] `, ...),
        // e.g. `[CFG] received P-CSCF server IP 2401:4900:c4:4035::8` — an
        // earlier version used `strip_prefix`, which requires the marker at
        // the exact start of the line and so never matched real output,
        // silently keeping every line "stuck without P-CSCF" forever (caught
        // by live-testing against the real EC20 + Airtel SIM: the tunnel
        // established successfully but the establish loop never noticed).
        let log = "[CFG] received P-CSCF server IP 2401:4900:c4:4035::8\n[CFG] received P-CSCF server IP 2401:4900:c4:4035::b\n";
        assert_eq!(
            extract_latest_pcscf(log),
            Some("2401:4900:c4:4035::b".to_string())
        );
    }

    #[test]
    fn list_sas_ims_detection_matches_the_grep_pattern() {
        assert!(list_sas_has_ims_child_sa(
            "ims: #1, ESTABLISHED\n  local ..."
        ));
        assert!(!list_sas_has_ims_child_sa("other: #1, ESTABLISHED\n"));
    }

    #[test]
    fn swu_connected_marker_matches_the_grep_pattern() {
        assert!(swu_log_shows_connected("... STATE CONNECTED ..."));
        assert!(!swu_log_shows_connected("STATE CONNECTING"));
    }

    #[test]
    fn extract_swu_pcscf_prefers_ipv4_over_ipv6() {
        let log = "P-CSCF IPV4 ADDRESS: 10.1.2.3\nP-CSCF IPV6 ADDRESS: 2001:db8::1\n";
        assert_eq!(extract_swu_pcscf(log), Some("10.1.2.3".to_string()));
    }

    #[test]
    fn extract_swu_pcscf_falls_back_to_ipv6_when_no_ipv4_present() {
        let log = "P-CSCF IPV6 ADDRESS: 2001:db8::1\n";
        assert_eq!(extract_swu_pcscf(log), Some("2001:db8::1".to_string()));
    }

    #[test]
    fn extract_swu_pcscf_none_when_neither_present() {
        assert_eq!(extract_swu_pcscf("STATE CONNECTED\n"), None);
    }

    mod engine_wiring_tests {
        use super::*;
        use crate::supervise::runner::MockCommandRunner;

        // MOCK JUSTIFICATION (constitution Principle I): stands in for real
        // charon/swanctl/the swu dialer/a live modem — none available in
        // CI. The wiring under test (which commands get issued, in what
        // order, on what engine method) is real production code.

        fn strongswan_engine() -> StrongswanEngine {
            StrongswanEngine {
                idx: 0,
                strongswan_conf: "/etc/strongswan-line-0.conf".to_string(),
                swanctl_top_conf: "/etc/swanctl-line-0.conf".to_string(),
                charon_log: PathBuf::from("/tmp/charon-0.log"),
                netns: "ims".to_string(),
                tun_iface: "tun23".to_string(),
                if_id: "23".to_string(),
                charon_handle: RefCell::new(None),
            }
        }

        #[test]
        fn terminate_issues_swanctl_terminate_ike_ims() {
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            engine.terminate(&runner);
            let calls = runner.run_calls.lock().unwrap();
            assert!(calls
                .iter()
                .any(|c| c.contains(&"--terminate".to_string()) && c.contains(&"ims".to_string())));
        }

        #[test]
        fn restart_process_clears_the_log_removes_the_pidfile_and_respawns_charon() {
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            let old = runner.spawn(ChildSpec::new(["charon"])).unwrap();
            *engine.charon_handle.borrow_mut() = Some(old);

            engine.restart_process(&runner);

            let calls = runner.run_calls.lock().unwrap();
            assert!(calls
                .iter()
                .any(|c| c == &["rm", "-f", "/var/run/charon.pid"]));
            assert!(engine.charon_handle.borrow().is_some());
            assert_ne!(*engine.charon_handle.borrow(), Some(old));
        }

        #[test]
        fn restart_process_spawns_charon_through_env_not_as_argv0() {
            // Regression test (Greptile P1): a real RealCommandRunner passes
            // argv[0] straight to `Command::new`, so the spawned command's
            // first argument MUST be the literal program to exec ("env"),
            // with the `STRONGSWAN_CONF=...` assignment as its own argument
            // — not the environment-variable string itself as argv[0],
            // which would fail with ENOENT against a real runner (a mock
            // can't catch this since it never actually execs anything).
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            engine.restart_process(&runner);
            let specs = runner.spawn_specs.lock().unwrap();
            let charon_spec = specs
                .iter()
                .find(|s| s.argv.iter().any(|a| a == "/usr/libexec/ipsec/charon"))
                .expect("a charon spawn must have been issued");
            assert_eq!(charon_spec.argv[0], "env");
            assert!(charon_spec.argv[1].starts_with("STRONGSWAN_CONF="));
        }

        #[test]
        fn restart_process_backgrounds_initiate_rather_than_blocking_on_it() {
            // Regression test (Greptile P2): the bash original runs
            // `--initiate` with a trailing `&` specifically so a slow IKE
            // negotiation can't block the establish/steady-state poll loop;
            // this must go through `spawn` (fire-and-forget), never a
            // blocking `run`.
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            engine.restart_process(&runner);

            let run_calls = runner.run_calls.lock().unwrap();
            assert!(
                !run_calls
                    .iter()
                    .any(|c| c.contains(&"--initiate".to_string())),
                "--initiate must never be issued as a blocking `run` call"
            );
            let spawn_specs = runner.spawn_specs.lock().unwrap();
            assert!(spawn_specs
                .iter()
                .any(|s| s.argv.contains(&"--initiate".to_string())));
        }

        #[test]
        fn restart_process_sleeps_before_load_all_to_let_the_vici_socket_come_up() {
            // Regression test for a real bug found live (T049/FR-009 audit):
            // the bash original has `sleep 2 # let the vici socket come up
            // before swanctl talks to it` at every one of its charon-respawn
            // sites, including this one — but this port's restart_charon
            // had it only at the *initial* startup call site (orchestrate.rs),
            // not here. Missing it here meant `--load-all` could run before
            // the freshly respawned charon's vici socket was listening,
            // silently failing to load anything, so the follow-up
            // `--initiate` failed with "CHILD_SA config 'ims' not found" —
            // and steady-state's ChildSaMissing branch only ever
            // re-initiates, never reloads, so the tunnel never recovered on
            // its own. Observed live: killing charon mid-session left the
            // line permanently stuck, while the healthcheck kept passing on
            // the stale pre-kill tun23-0 address.
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            engine.restart_process(&runner);
            assert!(
                runner
                    .sleeps
                    .lock()
                    .unwrap()
                    .contains(&std::time::Duration::from_secs(2)),
                "restart_process must sleep 2s before --load-all, matching \
                 the bash original at every one of its charon-respawn sites"
            );
        }

        #[test]
        fn reinitiate_also_backgrounds_initiate() {
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            engine.reinitiate(&runner);
            assert!(runner.run_calls.lock().unwrap().is_empty());
            let specs = runner.spawn_specs.lock().unwrap();
            assert!(specs
                .iter()
                .any(|s| s.argv.contains(&"--initiate".to_string())));
        }

        #[test]
        fn tun_iface_missing_is_reported_as_tun_vanished() {
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            // No output seeded for the `ip link show` probe -> MockCommandRunner
            // defaults to a successful (status 0) empty Output, so seed a
            // failing one explicitly for this netns-scoped call. Key format
            // ("netns:<netns>:<argv joined with spaces>") matches
            // MockCommandRunner::run_in_netns's own lookup key.
            use std::os::unix::process::ExitStatusExt;
            runner.set_run_output(
                "netns:ims:ip link show tun23",
                std::process::Output {
                    status: std::process::ExitStatus::from_raw(256),
                    stdout: vec![],
                    stderr: vec![],
                },
            );
            assert_eq!(
                engine.steady_state_health(&runner),
                SteadyStateHealth::TunVanished
            );
        }

        fn swu_engine() -> SwuEngine {
            SwuEngine {
                idx: 0,
                modem: "/dev/ttyUSB2".to_string(),
                apn: "ims".to_string(),
                mcc: "404".to_string(),
                mnc: "10".to_string(),
                netns: "ims".to_string(),
                src_addr: None,
                log_file: PathBuf::from("/tmp/swu-0.log"),
                dialer_handle: RefCell::new(None),
            }
        }

        #[test]
        fn swu_terminate_and_reinitiate_are_genuinely_no_ops() {
            let runner = MockCommandRunner::new();
            let engine = swu_engine();
            engine.terminate(&runner);
            engine.reinitiate(&runner);
            assert!(runner.run_calls.lock().unwrap().is_empty());
        }

        #[test]
        fn swu_restart_process_respawns_the_dialer_with_the_same_line_params() {
            let runner = MockCommandRunner::new();
            let engine = swu_engine();
            engine.restart_process(&runner);
            let specs = runner.spawn_specs.lock().unwrap();
            assert_eq!(specs.len(), 1);
            assert!(specs[0].argv.contains(&"-m".to_string()));
            assert!(specs[0].argv.contains(&"/dev/ttyUSB2".to_string()));
        }
    }
}
