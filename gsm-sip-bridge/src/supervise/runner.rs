//! The injectable boundary between orchestration decision logic and the outside
//! world (specs/021-entrypoint-supervise-rust, research.md R1/R2).
//!
//! Every current `docker/entrypoint.sh` shell-out — `ip`, `dig`, `swanctl`,
//! `stty`, raw serial AT writes, and the spawning/signalling/liveness-checking
//! of long-running children (`charon`, `pcscd`, the vowifi/volte agents, the
//! circuit-switched daemon, keepalive loops) — goes through [`CommandRunner`].
//! Decision logic (what to run, in what order, on what observed state) is
//! written against the trait, not against `std::process` directly, so it can be
//! exercised in tests without root, hardware, or any of those real processes.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// One long-running child process this feature's supervisors start (charon,
/// pcscd, an ims/carrier agent, the circuit-switched daemon, a keepalive
/// loop, ...). Constructed by the caller; `CommandRunner::spawn` decides how
/// to actually start it.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    pub argv: Vec<String>,
    /// Run inside this network namespace via `ip netns exec`, matching the
    /// current script's per-line `ip netns exec "$netns" ...` invocations.
    pub netns: Option<String>,
    /// Tee stdout+stderr to this file, matching e.g. the ims-agent's
    /// `tee "$agent_log"` and charon's `> >(tee "$log_file")`.
    pub stdout_capture_path: Option<std::path::PathBuf>,
}

impl ChildSpec {
    pub fn new(argv: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            argv: argv.into_iter().map(Into::into).collect(),
            netns: None,
            stdout_capture_path: None,
        }
    }

    pub fn in_netns(mut self, netns: impl Into<String>) -> Self {
        self.netns = Some(netns.into());
        self
    }

    pub fn capture_stdout_to(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.stdout_capture_path = Some(path.into());
        self
    }
}

/// Opaque handle to a spawned child. Real impl maps it to a real
/// `std::process::Child`; the mock impl maps it to a synthetic bookkeeping
/// entry the test controls directly — callers never see a raw pid, so no test
/// needs a real OS process to exist (research.md R2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChildHandle(u64);

/// Signals sent to a [`ChildHandle`]. `Kill` (SIGKILL) is required, not
/// optional, for any child that may be blocked mid-AT-transaction on a serial
/// port — see the ordering-invariant tests in `shutdown.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
    Stop,
    Cont,
}

/// The trait every orchestration decision is written against.
pub trait CommandRunner: Send + Sync {
    /// One-shot command, captures stdout/stderr/status. Replaces a direct
    /// `Command::new(...)` call site.
    fn run(&self, argv: &[&str]) -> io::Result<Output>;

    /// Same, prefixed with `ip netns exec <netns>` — replaces the current
    /// script's `ip netns exec "$netns" ...` invocations.
    fn run_in_netns(&self, netns: &str, argv: &[&str]) -> io::Result<Output>;

    /// Reads a log/state file (charon.log, a reset log, an agent's tee'd
    /// output, a P-CSCF source-path file). Modeled separately from `run`
    /// because the current script scrapes files that accumulate across the
    /// container's lifetime, independent of any single command's own stdout.
    fn read_file(&self, path: &Path) -> io::Result<String>;

    /// Writes a rendered asset or state file.
    fn write_file(&self, path: &Path, contents: &str) -> io::Result<()>;

    /// Starts a long-running child, returns a handle to it.
    fn spawn(&self, spec: ChildSpec) -> io::Result<ChildHandle>;

    /// Starts a fire-and-forget child — matches the bash convention of
    /// backgrounding a command with a trailing `&` and never referencing its
    /// pid again (e.g. every `swanctl --initiate`, which the original script
    /// redirects to a log file and immediately forgets about). Deliberately
    /// returns no [`ChildHandle`]: there is no legitimate later use for one
    /// (nothing will ever signal or query this specific process again), and
    /// a caller holding a handle here would be tempted to track it — leading
    /// to exactly the bug this method exists to prevent (a real review
    /// finding: `spawn` + discard the handle still inserts a table entry
    /// that nothing ever reaps, so repeated recovery attempts over a
    /// long-running container's lifetime accumulate zombie processes and
    /// leaked table entries without bound). The real implementation still
    /// reaps the child (so it never becomes a zombie) — see
    /// `RealCommandRunner`'s doc comment on its own impl.
    fn spawn_detached(&self, spec: ChildSpec) -> io::Result<()>;

    /// Signals a previously spawned child. Best-effort, matching the current
    /// script's `kill ... 2>/dev/null || true` convention — a signal to an
    /// already-dead child is not an error.
    fn signal(&self, handle: ChildHandle, sig: Signal);

    /// `kill -0`-equivalent liveness check.
    fn is_alive(&self, handle: ChildHandle) -> bool;

    /// Blocks until the child exits, returns its exit status if available.
    fn wait(&self, handle: ChildHandle) -> Option<i32>;

    /// Blocks the calling thread for `d`. Routed through the runner (rather
    /// than a bare `std::thread::sleep` call at each site) so decision logic
    /// with real, load-bearing sleep durations (poll cadences, settle delays)
    /// stays exactly as tested as everything else: `MockCommandRunner`
    /// records the requested duration and returns immediately, so a
    /// table-driven test exercising a 15-attempt, 1s-apart poll loop runs in
    /// microseconds, not 15 real seconds — while still asserting the actual
    /// requested durations, so a change to a cadence constant still fails a
    /// test.
    fn sleep(&self, d: std::time::Duration);
}

/// Production implementation: real `std::process::Command`/`Child`, real
/// filesystem I/O. Long-running children are tracked in an internal table
/// keyed by opaque [`ChildHandle`] so the trait's callers never touch a raw
/// pid directly (matching the mock's shape exactly).
pub struct RealCommandRunner {
    next_id: AtomicU64,
    children: Mutex<HashMap<u64, Child>>,
}

impl Default for RealCommandRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl RealCommandRunner {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            children: Mutex::new(HashMap::new()),
        }
    }

    fn build_command(spec: &ChildSpec) -> Command {
        let (program, args): (&str, &[String]) = match &spec.netns {
            Some(netns) => {
                // `ip netns exec <netns> <argv...>` — built once here so
                // every long-running child honors `spec.netns` the same way,
                // rather than each caller remembering the prefix itself.
                let mut cmd = Command::new("ip");
                cmd.args(["netns", "exec", netns]).args(&spec.argv);
                return cmd;
            }
            None => (spec.argv[0].as_str(), &spec.argv[1..]),
        };
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd
    }
}

impl CommandRunner for RealCommandRunner {
    fn run(&self, argv: &[&str]) -> io::Result<Output> {
        Command::new(argv[0]).args(&argv[1..]).output()
    }

    fn run_in_netns(&self, netns: &str, argv: &[&str]) -> io::Result<Output> {
        Command::new("ip")
            .args(["netns", "exec", netns])
            .args(argv)
            .output()
    }

    fn read_file(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn write_file(&self, path: &Path, contents: &str) -> io::Result<()> {
        std::fs::write(path, contents)
    }

    fn spawn_detached(&self, spec: ChildSpec) -> io::Result<()> {
        let mut cmd = Self::build_command(&spec);
        if let Some(path) = &spec.stdout_capture_path {
            let file = std::fs::File::create(path)?;
            cmd.stdout(Stdio::from(file.try_clone()?));
            cmd.stderr(Stdio::from(file));
        }
        let mut child = cmd.spawn()?;
        // Never inserted into `self.children` — there is no handle for a
        // caller to hold, so nothing could ever remove it from that table.
        // Reaped by a dedicated thread instead (real review finding: without
        // this, a fire-and-forget child that WAS still inserted accumulated
        // an un-reapable table entry on every call, unboundedly, over a
        // long-running container's lifetime). This thread's only job is to
        // wait out the child so the kernel can release it; the exit status
        // is intentionally discarded, matching the bash original's own
        // `swanctl --initiate ... &` — its exit code was never checked
        // either.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    fn spawn(&self, spec: ChildSpec) -> io::Result<ChildHandle> {
        let mut cmd = Self::build_command(&spec);
        if let Some(path) = &spec.stdout_capture_path {
            let file = std::fs::File::create(path)?;
            cmd.stdout(Stdio::from(file.try_clone()?));
            cmd.stderr(Stdio::from(file));
        }
        let child = cmd.spawn()?;
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.children.lock().unwrap().insert(id, child);
        Ok(ChildHandle(id))
    }

    fn signal(&self, handle: ChildHandle, sig: Signal) {
        // Holds `children`'s lock for the ENTIRE check-then-signal
        // sequence — including the external `kill` invocation itself — not
        // just the pid lookup. A prior version released the lock as soon as
        // the pid was read, then shelled out to `kill` afterward; a second
        // review pass caught the resulting race: a concurrent `is_alive`/
        // `wait` call on another thread (this runner is `Send + Sync` and
        // genuinely used from multiple per-line supervisor threads) could
        // reap the same child in that window, and the OS could reuse its
        // pid, before our own `kill` call ever ran — so the signal could
        // land on a completely unrelated process despite the try_wait()
        // check having passed moments earlier. Holding the lock across the
        // whole thing means no other call on this runner can reap (or be
        // reaped as) this handle while a signal for it is in flight; the
        // `kill` subprocess itself is near-instant, so the brief
        // process-wide serialization this adds is an acceptable trade for
        // actually closing the race, not just narrowing it.
        let mut children = self.children.lock().unwrap();
        let Some(child) = children.get_mut(&handle.0) else {
            return;
        };
        let pid = match child.try_wait() {
            Ok(None) => child.id(),
            _ => {
                children.remove(&handle.0);
                return;
            }
        };
        // Shells out to `kill` rather than a raw `libc::kill(2)` call: this
        // crate's gsm-sip-bridge/src must contain zero `unsafe` blocks
        // (tools/count-unsafe.sh), and this is exactly the current script's
        // own `kill -TERM/-KILL/-STOP/-CONT "$pid" 2>/dev/null || true`
        // convention — a signal to an already-exited process is expected to
        // fail silently, not to be treated as an error.
        let flag = match sig {
            Signal::Term => "-TERM",
            Signal::Kill => "-KILL",
            Signal::Stop => "-STOP",
            Signal::Cont => "-CONT",
        };
        let _ = Command::new("kill").args([flag, &pid.to_string()]).status();
    }

    fn is_alive(&self, handle: ChildHandle) -> bool {
        let mut children = self.children.lock().unwrap();
        let Some(child) = children.get_mut(&handle.0) else {
            return false;
        };
        match child.try_wait() {
            Ok(None) => true,
            _ => {
                // Exited (or errored checking) — remove so no later signal()
                // can reach a since-reused pid.
                children.remove(&handle.0);
                false
            }
        }
    }

    fn wait(&self, handle: ChildHandle) -> Option<i32> {
        // Removed from the table before the (potentially blocking) wait()
        // call, both to release the lock while blocked and so the handle is
        // already gone — and therefore un-signalable — for the whole
        // duration of the wait, not just after it returns.
        let mut child = {
            let mut children = self.children.lock().unwrap();
            children.remove(&handle.0)?
        };
        child.wait().ok().and_then(|status| status.code())
    }

    fn sleep(&self, d: std::time::Duration) {
        std::thread::sleep(d);
    }
}

#[cfg(test)]
mod real_runner_tests {
    // Integration-style, not mock-based (constitution Principle I): these
    // exercise RealCommandRunner against genuinely spawned, always-available
    // processes (`true`, `sleep`) — no hardware/charon/pcscd needed, so no
    // mock justification applies here; this is exactly the kind of thing
    // that must NOT be tested only against MockCommandRunner; the previous
    // version of this file signaled a raw pid after reaping a child, which a
    // mock (which has no concept of PID reuse at all) could never have
    // caught.
    use super::*;

    #[test]
    fn a_reaped_child_is_removed_so_a_later_signal_is_a_safe_no_op() {
        let runner = RealCommandRunner::new();
        let handle = runner.spawn(ChildSpec::new(["true"])).unwrap();
        // Give the (near-instant) child a moment to actually exit.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!runner.is_alive(handle), "`true` should have exited by now");
        // Must not panic/error — and, per the fix, must not attempt to
        // signal the stale pid at all.
        runner.signal(handle, Signal::Term);
        assert_eq!(runner.children.lock().unwrap().len(), 0);
    }

    #[test]
    fn wait_removes_the_entry_so_the_handle_cannot_be_signaled_afterward() {
        let runner = RealCommandRunner::new();
        let handle = runner.spawn(ChildSpec::new(["true"])).unwrap();
        let status = runner.wait(handle);
        assert_eq!(status, Some(0));
        assert_eq!(runner.children.lock().unwrap().len(), 0);
        // No panic, no attempt to signal a since-possibly-reused pid.
        runner.signal(handle, Signal::Kill);
    }

    #[test]
    fn a_still_running_child_can_be_signaled_and_is_observed_dead_after() {
        let runner = RealCommandRunner::new();
        let handle = runner.spawn(ChildSpec::new(["sleep", "30"])).unwrap();
        assert!(runner.is_alive(handle));
        runner.signal(handle, Signal::Kill);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!runner.is_alive(handle));
    }

    #[test]
    fn concurrent_signal_and_liveness_checks_on_the_same_handle_never_panic_or_misbehave() {
        // Regression test for the second review finding: signal() used to
        // release the table lock before actually shelling out to `kill`,
        // so a concurrent wait()/is_alive() on another thread could reap
        // the same handle (and the OS could reuse its pid) in that window.
        // Holding the lock for the whole check-then-signal sequence
        // serializes these instead of letting them interleave — this
        // hammers both from separate threads concurrently and just
        // requires it not to panic/deadlock and to end in a consistent
        // state (the child is gone, one way or another).
        let runner = std::sync::Arc::new(RealCommandRunner::new());
        let handle = runner.spawn(ChildSpec::new(["sleep", "2"])).unwrap();

        let r1 = std::sync::Arc::clone(&runner);
        let liveness_checker = std::thread::spawn(move || {
            for _ in 0..50 {
                r1.is_alive(handle);
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        });
        let r2 = std::sync::Arc::clone(&runner);
        let signaler = std::thread::spawn(move || {
            r2.signal(handle, Signal::Kill);
        });

        liveness_checker
            .join()
            .expect("liveness checker thread must not panic");
        signaler.join().expect("signaler thread must not panic");

        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!runner.is_alive(handle));
    }

    #[test]
    fn spawn_detached_never_leaves_a_table_entry_even_after_the_child_exits() {
        // Regression test for a real review finding: `spawn` + discarding
        // the handle still inserted an entry that nothing could ever remove
        // (no handle survives to call is_alive/wait/signal with). Repeating
        // this many times must never grow the table — spawn_detached must
        // never insert into it at all.
        let runner = RealCommandRunner::new();
        for _ in 0..10 {
            runner.spawn_detached(ChildSpec::new(["true"])).unwrap();
        }
        // Give the (near-instant) children a moment to actually exit, so
        // this also confirms they don't linger as zombies.
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(
            runner.children.lock().unwrap().len(),
            0,
            "spawn_detached must never insert a tracked table entry"
        );
    }
}

#[cfg(test)]
pub use mock::MockCommandRunner;

#[cfg(test)]
mod mock {
    use super::*;
    use std::collections::HashMap as Map;
    use std::sync::atomic::AtomicU64;
    use std::sync::Mutex;

    #[derive(Debug, Clone, Default)]
    pub struct MockChild {
        pub alive: bool,
        pub exit_code: Option<i32>,
        pub signals_received: Vec<Signal>,
    }

    /// Test-only `CommandRunner`: every call is recorded, and every
    /// observation (a "run" invocation's output, a file's content, a child's
    /// liveness) is whatever the test injected beforehand. No real process,
    /// no real filesystem, no root, no hardware.
    ///
    /// MOCK JUSTIFICATION (constitution Principle I, Integration-First
    /// Testing): stands in for `charon`/`pcscd`/`swanctl`/a live serial
    /// modem — exactly the "hardware not available in CI" carve-out the
    /// constitution names. The decision logic under test (what command runs
    /// next, what state transition follows) is real production code; only
    /// the process/filesystem boundary is faked.
    #[derive(Default)]
    pub struct MockCommandRunner {
        pub run_calls: Mutex<Vec<Vec<String>>>,
        pub run_in_netns_calls: Mutex<Vec<(String, Vec<String>)>>,
        pub run_outputs: Mutex<Map<String, Output>>,
        pub files: Mutex<Map<std::path::PathBuf, String>>,
        pub spawn_specs: Mutex<Vec<ChildSpec>>,
        pub children: Mutex<Map<u64, MockChild>>,
        /// Every requested sleep duration, in call order — recorded instead
        /// of actually slept, so tests exercising real cadence constants stay
        /// fast while still able to assert on them.
        pub sleeps: Mutex<Vec<std::time::Duration>>,
        /// Every handle `wait()` was called on, in call order — lets tests
        /// assert that a signaled-and-forgotten child was actually reaped,
        /// not just signaled (the class of bug a real review found in this
        /// PR: a handle nothing ever waits on leaks forever in
        /// `RealCommandRunner`'s table).
        pub wait_calls: Mutex<Vec<ChildHandle>>,
        next_id: AtomicU64,
    }

    impl MockCommandRunner {
        pub fn new() -> Self {
            Self {
                next_id: AtomicU64::new(1),
                ..Default::default()
            }
        }

        /// Seeds the content `read_file(path)` returns.
        pub fn set_file(&self, path: impl Into<std::path::PathBuf>, contents: impl Into<String>) {
            self.files
                .lock()
                .unwrap()
                .insert(path.into(), contents.into());
        }

        /// Seeds the `Output` a future `run(argv)` with this exact argv0
        /// returns (keyed by argv joined with spaces, good enough for tests
        /// that don't need to disambiguate identical commands).
        pub fn set_run_output(&self, argv_key: &str, output: Output) {
            self.run_outputs
                .lock()
                .unwrap()
                .insert(argv_key.to_string(), output);
        }

        pub fn kill_child(&self, handle: ChildHandle, exit_code: i32) {
            if let Some(c) = self.children.lock().unwrap().get_mut(&handle.0) {
                c.alive = false;
                c.exit_code = Some(exit_code);
            }
        }

        pub fn signals_for(&self, handle: ChildHandle) -> Vec<Signal> {
            self.children
                .lock()
                .unwrap()
                .get(&handle.0)
                .map(|c| c.signals_received.clone())
                .unwrap_or_default()
        }
    }

    fn empty_output(status: i32) -> Output {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(status),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    impl CommandRunner for MockCommandRunner {
        fn run(&self, argv: &[&str]) -> io::Result<Output> {
            let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            let key = owned.join(" ");
            self.run_calls.lock().unwrap().push(owned);
            Ok(self
                .run_outputs
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .unwrap_or_else(|| empty_output(0)))
        }

        fn run_in_netns(&self, netns: &str, argv: &[&str]) -> io::Result<Output> {
            let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            let key = format!("netns:{netns}:{}", owned.join(" "));
            self.run_in_netns_calls
                .lock()
                .unwrap()
                .push((netns.to_string(), owned));
            Ok(self
                .run_outputs
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .unwrap_or_else(|| empty_output(0)))
        }

        fn read_file(&self, path: &Path) -> io::Result<String> {
            self.files
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }

        fn write_file(&self, path: &Path, contents: &str) -> io::Result<()> {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), contents.to_string());
            Ok(())
        }

        fn spawn(&self, spec: ChildSpec) -> io::Result<ChildHandle> {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            self.children.lock().unwrap().insert(
                id,
                MockChild {
                    alive: true,
                    exit_code: None,
                    signals_received: Vec::new(),
                },
            );
            self.spawn_specs.lock().unwrap().push(spec);
            Ok(ChildHandle(id))
        }

        fn spawn_detached(&self, spec: ChildSpec) -> io::Result<()> {
            // Recorded in the same `spawn_specs` list as tracked spawns —
            // tests assert "was this argv issued," not "was it tracked" —
            // but, matching the real runner, never given a handle and never
            // inserted into `children` (there is nothing to leak here since
            // the mock has no OS process to reap).
            self.spawn_specs.lock().unwrap().push(spec);
            Ok(())
        }

        fn signal(&self, handle: ChildHandle, sig: Signal) {
            if let Some(c) = self.children.lock().unwrap().get_mut(&handle.0) {
                c.signals_received.push(sig);
                if matches!(sig, Signal::Kill) {
                    c.alive = false;
                }
            }
        }

        fn is_alive(&self, handle: ChildHandle) -> bool {
            self.children
                .lock()
                .unwrap()
                .get(&handle.0)
                .map(|c| c.alive)
                .unwrap_or(false)
        }

        fn wait(&self, handle: ChildHandle) -> Option<i32> {
            self.wait_calls.lock().unwrap().push(handle);
            self.children
                .lock()
                .unwrap()
                .get(&handle.0)
                .and_then(|c| c.exit_code)
        }

        fn sleep(&self, d: std::time::Duration) {
            self.sleeps.lock().unwrap().push(d);
        }
    }

    #[test]
    fn a_freshly_spawned_child_is_alive_with_no_signals_received() {
        let runner = MockCommandRunner::new();
        let handle = runner.spawn(ChildSpec::new(["echo", "hi"])).unwrap();
        assert!(runner.is_alive(handle));
        assert!(runner.signals_for(handle).is_empty());
    }

    #[test]
    fn killing_a_child_marks_it_dead_and_records_the_signal() {
        let runner = MockCommandRunner::new();
        let handle = runner.spawn(ChildSpec::new(["sleep", "100"])).unwrap();
        runner.signal(handle, Signal::Kill);
        assert!(!runner.is_alive(handle));
        assert_eq!(runner.signals_for(handle), vec![Signal::Kill]);
    }

    #[test]
    fn read_file_returns_seeded_content() {
        let runner = MockCommandRunner::new();
        runner.set_file("/tmp/charon-0.log", "hello");
        assert_eq!(
            runner.read_file(Path::new("/tmp/charon-0.log")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn read_file_on_unseeded_path_is_not_found() {
        let runner = MockCommandRunner::new();
        assert!(runner.read_file(Path::new("/tmp/nope.log")).is_err());
    }
}
