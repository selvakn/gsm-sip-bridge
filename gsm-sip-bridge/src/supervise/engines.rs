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
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Keeps only the lines of the **shared** charon log that belong to
/// `conn_name`.
///
/// Every line's charon used to own its whole log file, so no filtering was
/// needed. Now one daemon serves all lines into one file, and the only thing
/// attributing an event to a line is the `<name|uniqueid>` prefix charon emits
/// because `strongswan.conf` sets `ike_name = yes`. That directive is
/// therefore load-bearing: drop it and every line would read every other
/// line's establishment and P-CSCF events as its own, so line 1 would report
/// itself up the moment line 0 connected.
fn lines_for_conn<'a>(
    charon_log: &'a str,
    conn_name: &str,
    // `DoubleEndedIterator`, not plain `Iterator`: `extract_latest_pcscf`
    // scans backwards for the most recent address.
) -> impl DoubleEndedIterator<Item = &'a str> + 'a {
    let needle = format!("<{conn_name}|");
    charon_log.lines().filter(move |l| l.contains(&needle))
}

/// 1:1 port of `grep -q "CHILD_SA.*established"`, scoped to one connection.
pub fn charon_log_shows_established(charon_log: &str, conn_name: &str) -> bool {
    lines_for_conn(charon_log, conn_name).any(|l| {
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
pub fn extract_latest_pcscf(charon_log: &str, conn_name: &str) -> Option<String> {
    const MARKER: &str = "received P-CSCF server IP ";
    let is_valid_addr = |s: &str| {
        let is_v4 = s.split('.').count() == 4 && s.chars().all(|c| c.is_ascii_digit() || c == '.');
        let is_v6 = s.contains(':') && s.chars().all(|c| c.is_ascii_hexdigit() || c == ':');
        is_v4 || is_v6
    };
    lines_for_conn(charon_log, conn_name)
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

/// 1:1 port of `grep -q '^ims:' <<<"$sas_output"`, scoped to one connection —
/// the shared daemon's unfiltered `--list-sas` would otherwise let one line's
/// healthy SA mask another's missing one.
pub fn list_sas_has_child_sa(sas_output: &str, conn_name: &str) -> bool {
    let prefix = format!("{conn_name}:");
    sas_output.lines().any(|l| l.starts_with(&prefix))
}

/// The single charon daemon shared by every strongswan-engine line.
///
/// One charon *per line* was the original design, and it was wrong for a
/// reason that only appears once a second line exists: charon's socket-default
/// plugin sets `SO_REUSEADDR` but never `SO_REUSEPORT`, so N charon processes
/// in one network namespace all wildcard-bind `0.0.0.0:500`/`0.0.0.0:4500` and
/// exactly **one** of them is delivered every inbound IKE packet. The losers
/// retransmit into the void and give up, which the carrier surfaces to callers
/// as the line being "switched off".
///
/// Observed live 2026-07-29: on one boot line 0 established and line 1 timed
/// out; on the very next restart of the same image the two swapped. That
/// coin-flip is what identified it as a local port collision rather than
/// anything carrier-side.
///
/// Sharing the daemon costs nothing in isolation. A CHILD_SA is bound to its
/// line by the `if_id` in its own connection block and by the pre-created
/// `tunN` XFRM interface sitting in that line's netns; the kernel keys its
/// SA/policy lookup on `if_id` regardless of which daemon installed the state.
/// This is exactly the arrangement strongSwan expects of a gateway terminating
/// many independent tunnels.
///
/// What sharing *does* cost is recovery blast radius, so the daemon is only
/// ever restarted when it is genuinely dead ([`SharedCharon::restart_if_dead`]).
/// Every other fault — a stale SA, a vanished interface, a missing CHILD_SA —
/// is recovered per connection through `--terminate --ike <conn>` /
/// `--initiate --child <conn>`, leaving other lines' tunnels untouched.
pub struct SharedCharon {
    pub strongswan_conf: String,
    pub swanctl_top_conf: String,
    pub charon_log: PathBuf,
    /// `Mutex`, not the `RefCell` the per-line engines use: this is reached
    /// concurrently from every line's supervisor thread, and holding the lock
    /// across the check-and-spawn is what makes "restart it only if nobody
    /// else already has" atomic.
    handle: Mutex<Option<Arc<ChildHandle>>>,
}

impl SharedCharon {
    pub fn new(strongswan_conf: String, swanctl_top_conf: String, charon_log: PathBuf) -> Self {
        Self {
            strongswan_conf,
            swanctl_top_conf,
            charon_log,
            handle: Mutex::new(None),
        }
    }

    fn env_prefix(&self) -> String {
        format!("STRONGSWAN_CONF={}", self.strongswan_conf)
    }

    /// Spawns the daemon unless it is already running. Idempotent and safe to
    /// call concurrently from every line's startup thread — lines start on
    /// their own threads, and whichever arrives first wins.
    ///
    /// Returns the handle **only to the caller that actually spawned it**, so
    /// the daemon gets registered into `StartedState` exactly once instead of
    /// once per line.
    pub fn ensure_started(&self, runner: &dyn CommandRunner) -> Option<Arc<ChildHandle>> {
        let mut guard = self.handle.lock().unwrap();
        if guard.as_ref().is_some_and(|h| runner.is_alive(h)) {
            return None;
        }
        self.spawn_locked(runner, &mut guard)
    }

    /// Restarts the daemon **only if it is actually dead**, reporting whether
    /// it did.
    ///
    /// Both halves of that condition matter. A line whose vici calls fail
    /// while the daemon is alive must not take every other line's tunnel down
    /// with it; and a second line noticing the same death a moment later must
    /// not kill the replacement the first line just spawned.
    pub fn restart_if_dead(&self, runner: &dyn CommandRunner) -> bool {
        let mut guard = self.handle.lock().unwrap();
        if guard.as_ref().is_some_and(|h| runner.is_alive(h)) {
            return false;
        }
        self.spawn_locked(runner, &mut guard).is_some()
    }

    fn spawn_locked(
        &self,
        runner: &dyn CommandRunner,
        guard: &mut Option<Arc<ChildHandle>>,
    ) -> Option<Arc<ChildHandle>> {
        if let Some(old) = guard.take() {
            // Signalling alone was never enough (Greptile P1 on the per-line
            // version this replaces): RealCommandRunner's tracked-children
            // table only drops an entry once something reaps it, so a bare
            // signal leaked an entry on every restart — and let a
            // slow-to-terminate charon keep contending for the vici socket
            // and pidfile with the replacement about to be spawned.
            runner.reap(&old);
        }
        let _ = runner.write_file(&self.charon_log, "");
        let _ = runner.run(&["rm", "-f", "/var/run/charon.pid"]);

        let env = self.env_prefix();
        // `env` must be argv[0], not the `STRONGSWAN_CONF=...` string itself:
        // RealCommandRunner passes argv[0] straight to `Command::new`, so
        // without it the spawn tries to exec the environment assignment as a
        // program, fails with ENOENT, and leaves no daemon running while the
        // rest of the sequence proceeds as though one were.
        let spawned = runner
            .spawn(
                ChildSpec::new(["env", env.as_str(), "/usr/libexec/ipsec/charon"])
                    .capture_stdout_to(self.charon_log.clone()),
            )
            .ok()
            .map(Arc::new);
        if spawned.is_some() {
            // Let the vici socket come up before any swanctl call. A
            // `--load-all` issued too early fails silently (its result is
            // deliberately ignored), and the `--initiate` that follows then
            // fails with "CHILD_SA config not found" — leaving the line
            // re-initiating against an empty config forever, since
            // steady-state's ChildSaMissing branch only re-initiates and
            // never reloads.
            runner.sleep(Duration::from_secs(2));
        }
        guard.clone_from(&spawned);
        spawned
    }

    pub fn is_alive(&self, runner: &dyn CommandRunner) -> bool {
        self.handle
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|h| runner.is_alive(h))
    }

    /// The daemon's current handle, so a caller that has just driven a
    /// recovery can make sure the (possibly replaced) process is registered
    /// for shutdown.
    pub fn current_handle(&self) -> Option<Arc<ChildHandle>> {
        self.handle.lock().unwrap().clone()
    }

    /// `swanctl --load-all` over the shared `conf.d`, loading the union of
    /// every line's connection file.
    ///
    /// It must stay a whole-directory load: `--load-all` *unloads* any
    /// connection missing from what it just read, so loading one line's file
    /// alone would evict the other lines' connections. Loading the union is
    /// idempotent and order-independent, which is what makes it safe for
    /// concurrently-starting lines to each call it.
    pub fn load_all(&self, runner: &dyn CommandRunner) {
        let env = self.env_prefix();
        let _ = runner.run(&[
            "env",
            env.as_str(),
            "swanctl",
            "--load-all",
            "--file",
            &self.swanctl_top_conf,
        ]);
    }
}

/// One line's strongSwan engine state — this line's own identifiers, plus a
/// handle on the daemon every line shares. Anything not per line (the daemon,
/// its log, its vici socket, its config paths) lives in [`SharedCharon`].
pub struct StrongswanEngine {
    pub idx: u32,
    /// This line's swanctl connection *and* child name (`ims0`, `ims1`, ...).
    /// Unique per line so that `--initiate`/`--terminate` address exactly one
    /// line, and so the shared charon log's `<name|N>` prefixes can be
    /// attributed back to one line.
    pub conn_name: String,
    pub netns: String,
    pub tun_iface: String,
    /// This line's XFRM `if_id`, needed to recreate the interface after
    /// `SteadyStateHealth::TunVanished` (see `recreate_interface`).
    pub if_id: String,
    pub shared: Arc<SharedCharon>,
}

impl StrongswanEngine {
    fn env_prefix(&self) -> String {
        self.shared.env_prefix()
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
}

impl TunnelEngine for StrongswanEngine {
    fn is_tunnel_established(&self, runner: &dyn CommandRunner) -> bool {
        runner
            .read_file(&self.shared.charon_log)
            .map(|c| charon_log_shows_established(&c, &self.conn_name))
            .unwrap_or(false)
    }

    fn latest_pcscf(&self, runner: &dyn CommandRunner) -> Option<String> {
        runner
            .read_file(&self.shared.charon_log)
            .ok()
            .and_then(|c| extract_latest_pcscf(&c, &self.conn_name))
    }

    fn is_process_alive(&self, runner: &dyn CommandRunner) -> bool {
        self.shared.is_alive(runner)
    }

    fn terminate(&self, runner: &dyn CommandRunner) {
        self.swanctl(runner, &["--terminate", "--ike", &self.conn_name]);
    }

    fn reinitiate(&self, runner: &dyn CommandRunner) {
        self.swanctl_background(runner, &["--initiate", "--child", &self.conn_name]);
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
        // `--ike <conn>` scopes the query to this line. Without it the shared
        // daemon reports every line's SAs, and another line's healthy
        // CHILD_SA would mask this line's missing one.
        let sas_output = match runner.run(&[
            "env",
            &env,
            "swanctl",
            "--list-sas",
            "--ike",
            &self.conn_name,
        ]) {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
            _ => {
                println!(
                    "[supervise] line {}: swanctl --list-sas failed (vici connection broken)",
                    self.idx
                );
                return SteadyStateHealth::ViciBroken;
            }
        };
        if !list_sas_has_child_sa(&sas_output, &self.conn_name) {
            println!(
                "[supervise] line {}: {} CHILD_SA missing; re-initiating",
                self.idx, self.conn_name
            );
            return SteadyStateHealth::ChildSaMissing;
        }
        SteadyStateHealth::Ok
    }

    /// Recovery for "the process died" and "vici is unreachable".
    ///
    /// Restarting the daemon is the one genuinely *global* action here, so it
    /// happens only when the daemon is actually dead — and `restart_if_dead`
    /// makes that check-and-respawn atomic, so two lines noticing the same
    /// death don't restart it twice or kill each other's replacement. While
    /// the daemon is alive, this line's trouble is its own, and recovery stays
    /// scoped to its connection so other lines' tunnels keep running.
    fn restart_process(&self, runner: &dyn CommandRunner) {
        let respawned = self.shared.restart_if_dead(runner);
        // Safe either way: loading the union of conf.d cannot evict anything.
        // Required after a respawn (a fresh daemon has nothing loaded), and a
        // harmless no-op otherwise.
        self.shared.load_all(runner);
        if !respawned {
            // Only meaningful against a daemon still holding a stale SA for
            // this connection; a freshly spawned one has nothing to terminate.
            self.terminate(runner);
        }
        self.reinitiate(runner);
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
    pub dialer_handle: RefCell<Option<std::sync::Arc<ChildHandle>>>,
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
            .as_ref()
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
            // Same fix as StrongswanEngine::restart_charon (Greptile P1):
            // reap the old dialer before spawning its replacement, so its
            // table entry doesn't leak and it can't overlap/contend with
            // the new one.
            runner.reap(&old);
        }
        let _ = runner.write_file(&self.log_file, "");
        *self.dialer_handle.borrow_mut() = self.spawn_dialer(runner).map(std::sync::Arc::new);
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
            "12[IKE] <ims0|1> CHILD_SA ims0{1} established with SPIs...",
            "ims0"
        ));
        assert!(!charon_log_shows_established("nothing relevant", "ims0"));
    }

    #[test]
    fn one_lines_established_tunnel_is_not_read_as_another_lines() {
        // The invariant that makes a shared charon log safe. Both lines write
        // into one file now, so without `<conn|` scoping line 1 would report
        // itself up the instant line 0 connected — and then sit waiting for a
        // P-CSCF that belongs to someone else.
        let log = "12[IKE] <ims0|1> CHILD_SA ims0{1} established with SPIs...\n";
        assert!(charon_log_shows_established(log, "ims0"));
        assert!(
            !charon_log_shows_established(log, "ims1"),
            "line 1 must not see line 0's CHILD_SA as its own"
        );
    }

    #[test]
    fn extract_latest_pcscf_picks_the_chronologically_last_valid_line() {
        let log = "<ims0|1> received P-CSCF server IP 10.0.0.1\n<ims0|1> received P-CSCF server IP 2001:db8::1\n<ims0|1> received P-CSCF server IP 10.0.0.9\n";
        assert_eq!(
            extract_latest_pcscf(log, "ims0"),
            Some("10.0.0.9".to_string())
        );
    }

    #[test]
    fn each_line_extracts_only_its_own_pcscf_from_the_shared_log() {
        // Two lines interleaved in one file, line 1's address written last.
        // Each must still read its own — handing line 0 line 1's P-CSCF would
        // point its IMS agent at the wrong carrier's proxy.
        let log = "<ims0|1> received P-CSCF server IP 10.0.0.1\n\
                   <ims1|2> received P-CSCF server IP 10.9.9.9\n";
        assert_eq!(
            extract_latest_pcscf(log, "ims0"),
            Some("10.0.0.1".to_string())
        );
        assert_eq!(
            extract_latest_pcscf(log, "ims1"),
            Some("10.9.9.9".to_string())
        );
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
        let log = "[CFG] <ims0|1> received P-CSCF server IP 2401:4900:c4:4035::8\n[CFG] <ims0|1> received P-CSCF server IP 2401:4900:c4:4035::b\n";
        assert_eq!(
            extract_latest_pcscf(log, "ims0"),
            Some("2401:4900:c4:4035::b".to_string())
        );
    }

    #[test]
    fn list_sas_detection_matches_the_grep_pattern() {
        assert!(list_sas_has_child_sa(
            "ims0: #1, ESTABLISHED\n  local ...",
            "ims0"
        ));
        assert!(!list_sas_has_child_sa("other: #1, ESTABLISHED\n", "ims0"));
        // A sibling line's SA must not count as this line's.
        assert!(!list_sas_has_child_sa("ims1: #1, ESTABLISHED\n", "ims0"));
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

        fn shared_charon() -> Arc<SharedCharon> {
            Arc::new(SharedCharon::new(
                "/etc/strongswan-shared.conf".to_string(),
                "/etc/swanctl/swanctl.conf".to_string(),
                PathBuf::from("/tmp/charon.log"),
            ))
        }

        fn strongswan_engine() -> StrongswanEngine {
            StrongswanEngine {
                idx: 0,
                conn_name: "ims0".to_string(),
                netns: "ims".to_string(),
                tun_iface: "tun23".to_string(),
                if_id: "23".to_string(),
                shared: shared_charon(),
            }
        }

        #[test]
        fn the_daemon_is_spawned_once_no_matter_how_many_lines_share_it() {
            // This is the fix itself. N lines used to mean N charon
            // processes, all wildcard-bound to UDP 500/4500 in one netns,
            // where only one of them ever received a reply.
            let runner = MockCommandRunner::new();
            let shared = shared_charon();

            let first = shared.ensure_started(&runner);
            let second = shared.ensure_started(&runner);

            assert!(
                first.is_some(),
                "the first caller spawns it and is handed the handle to register"
            );
            assert!(
                second.is_none(),
                "a later line must find it already running, not spawn a second"
            );
            let charon_spawns = runner
                .spawn_specs
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.argv.iter().any(|a| a == "/usr/libexec/ipsec/charon"))
                .count();
            assert_eq!(charon_spawns, 1);
        }

        #[test]
        fn restart_process_leaves_a_live_shared_daemon_alone() {
            // The property that makes sharing safe: one line's vici trouble
            // must not restart the daemon out from under every other line.
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            engine.shared.ensure_started(&runner);
            let id = engine.shared.current_handle().unwrap().id();

            engine.restart_process(&runner);

            assert_eq!(
                engine.shared.current_handle().unwrap().id(),
                id,
                "a live shared daemon must survive one line's recovery"
            );
            let specs = runner.spawn_specs.lock().unwrap();
            assert!(
                specs
                    .iter()
                    .any(|s| s.argv.contains(&"--initiate".to_string())
                        && s.argv.contains(&"ims0".to_string())),
                "recovery must still re-initiate, scoped to this line's connection"
            );
        }

        #[test]
        fn terminate_is_scoped_to_this_lines_connection() {
            let runner = MockCommandRunner::new();
            let engine = strongswan_engine();
            engine.terminate(&runner);
            let calls = runner.run_calls.lock().unwrap();
            assert!(
                calls
                    .iter()
                    .any(|c| c.contains(&"--terminate".to_string())
                        && c.contains(&"ims0".to_string()))
            );
            assert!(
                !calls.iter().any(|c| c.contains(&"ims".to_string())),
                "the bare `ims` name would terminate every line's IKE_SA at once"
            );
        }

        #[test]
        fn restart_process_clears_the_log_removes_the_pidfile_and_respawns_a_dead_charon() {
            let runner = MockCommandRunner::new();
            runner.set_born_dead_if_argv_contains("charon");
            let engine = strongswan_engine();
            engine.shared.ensure_started(&runner);
            let old_id = engine.shared.current_handle().unwrap().id();

            engine.restart_process(&runner);

            let calls = runner.run_calls.lock().unwrap();
            assert!(calls
                .iter()
                .any(|c| c == &["rm", "-f", "/var/run/charon.pid"]));
            let new = engine
                .shared
                .current_handle()
                .expect("a replacement must have been spawned");
            assert_ne!(new.id(), old_id, "the handle must be a new child");
        }

        #[test]
        fn restart_process_reaps_the_old_charon_before_spawning_the_new_one() {
            // Regression test (Greptile P1): signaling the old handle
            // without ever waiting on it left a permanently un-reaped entry
            // in RealCommandRunner's tracked-children table (a leak across
            // repeated recoveries) and let a slow-to-die old charon overlap
            // with its replacement. `wait()` must be called on the old
            // handle before restart_process returns.
            let runner = MockCommandRunner::new();
            runner.set_born_dead_if_argv_contains("charon");
            let engine = strongswan_engine();
            engine.shared.ensure_started(&runner);
            let old_id = engine.shared.current_handle().unwrap().id();

            engine.restart_process(&runner);

            // `reap` = SIGTERM, then poll is_alive until it is really gone
            // (escalating to SIGKILL). Asserting on the signals rather than
            // on a wait() call, because reap deliberately no longer waits:
            // waiting untracked the child up front, which is what made a
            // concurrently-held handle silently unusable.
            let sigs = runner.signals_for_id(old_id);
            assert!(
                sigs.contains(&crate::supervise::runner::Signal::Term),
                "the replaced child must be terminated, got {sigs:?}"
            );
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
            runner.set_born_dead_if_argv_contains("charon");
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
            runner.set_born_dead_if_argv_contains("charon");
            let engine = strongswan_engine();
            engine.restart_process(&runner);
            assert!(
                runner
                    .sleeps
                    .lock()
                    .unwrap()
                    .contains(&std::time::Duration::from_secs(2)),
                "a respawn must sleep 2s before --load-all, matching \
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

        #[test]
        fn swu_restart_process_reaps_the_old_dialer_before_spawning_the_new_one() {
            // Same Greptile P1 fix as StrongswanEngine::restart_charon: the
            // old dialer handle must actually be waited on, not just
            // signaled, or it leaks in RealCommandRunner's tracked-children
            // table and can overlap with its replacement.
            let runner = MockCommandRunner::new();
            let engine = swu_engine();
            let old = runner.spawn(ChildSpec::new(["swu-dialer"])).unwrap();
            let old_id = old.id();
            *engine.dialer_handle.borrow_mut() = Some(std::sync::Arc::new(old));

            engine.restart_process(&runner);

            let sigs = runner.signals_for_id(old_id);
            assert!(
                sigs.contains(&crate::supervise::runner::Signal::Term),
                "the replaced child must be terminated, got {sigs:?}"
            );
        }
    }
}
