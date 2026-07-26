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

    /// Signals a previously spawned child. Best-effort, matching the current
    /// script's `kill ... 2>/dev/null || true` convention — a signal to an
    /// already-dead child is not an error.
    fn signal(&self, handle: ChildHandle, sig: Signal);

    /// `kill -0`-equivalent liveness check.
    fn is_alive(&self, handle: ChildHandle) -> bool;

    /// Blocks until the child exits, returns its exit status if available.
    fn wait(&self, handle: ChildHandle) -> Option<i32>;
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
        let pid = {
            let mut children = self.children.lock().unwrap();
            let Some(child) = children.get_mut(&handle.0) else {
                return;
            };
            child.id()
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
        matches!(child.try_wait(), Ok(None))
    }

    fn wait(&self, handle: ChildHandle) -> Option<i32> {
        let mut children = self.children.lock().unwrap();
        let child = children.get_mut(&handle.0)?;
        child.wait().ok().and_then(|status| status.code())
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
            self.children
                .lock()
                .unwrap()
                .get(&handle.0)
                .and_then(|c| c.exit_code)
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
