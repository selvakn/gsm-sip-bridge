//! The injectable boundary between orchestration decision logic and the outside
//! world (specs/021-entrypoint-supervise-rust, research.md R1/R2).
//!
//! Every `docker/entrypoint.sh` shell-out — `ip`, `dig`, `swanctl`,
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
///
/// # Deliberately neither `Copy` nor `Clone`
///
/// This handle is a *claim on a live child*, and [`CommandRunner::wait`] —
/// the one operation that ends the child's trackability for everyone —
/// consumes it. That is not stylistic. The most expensive defect in this
/// module's history was reaching a handle *after* something had waited on it.
///
/// [`RealCommandRunner::wait`] removes the table entry before blocking
/// (closing a pid-reuse race — see its doc comment), so a waited-on handle is
/// permanently un-signalable: a later `signal`/`is_alive` is a silent no-op
/// and the child can never be stopped, continued, or killed again.
/// `MockCommandRunner` did not model that, so the failure was invisible to
/// unit tests — it shipped **seven times**, in the vowifi-usim-bridge holder,
/// all three VoLTE supervision loops, the circuit-switched daemon loop, the
/// shared vowifi-sip-agent loop, and the per-line vowifi-ims-agent loop, and
/// was caught only by live hardware and code review, one site at a time.
///
/// Without `Copy`, `wait`'s by-value signature makes every one of those a
/// borrow-checker error. Three shapes remain, and they are the correct ones:
///
/// - **Sole owner, wants the exit status** — [`CommandRunner::wait`]. Only
///   reachable when nothing else holds the handle, which is the invariant
///   that was being violated.
/// - **Wants the child gone** — [`CommandRunner::reap`]. Borrows, because it
///   signals and then polls rather than untracking up front, so it is safe
///   even while others hold a claim.
/// - **Shared between a supervision loop and the shutdown plan** —
///   `Arc<ChildHandle>`. Both clones can `signal`/`is_alive`/`reap`; neither
///   can `wait`, which is exactly right.
///
/// If you want to clone this bare, you want `Arc<ChildHandle>` or `is_alive`.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct ChildHandle(u64);

impl ChildHandle {
    /// The handle's opaque identity, for logging and for test assertions that
    /// need to refer to a child *after* ownership has moved on (into an
    /// engine's `RefCell`, into a shutdown plan, into `reap`).
    ///
    /// Read-only on purpose: the tuple field stays private, so an id cannot
    /// be turned back into a `ChildHandle`. There is no way to fabricate a
    /// claim on a child you did not spawn.
    pub fn id(&self) -> u64 {
        self.0
    }
}

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

/// Poll bound for [`CommandRunner::reap`] at each of its two stages, matching
/// `shutdown`'s own `KILL_CONFIRM_MAX_POLLS` — a ~5s budget per stage, so a
/// process ignoring `SIGTERM` costs 5s before escalation rather than hanging
/// its supervision thread forever (which the `wait()` this replaced could).
const REAP_MAX_POLLS: u32 = 20;
const REAP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

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
    /// because the original script scrapes files that accumulate across the
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
    ///
    /// Borrows the handle, so the caller keeps it and can signal again.
    fn signal(&self, handle: &ChildHandle, sig: Signal);

    /// `kill -0`-equivalent liveness check. Borrows, so this is the operation
    /// to poll in a loop when the child must stay reachable — see
    /// [`ChildHandle`].
    fn is_alive(&self, handle: &ChildHandle) -> bool;

    /// Blocks until the child exits, returns its exit status if available.
    ///
    /// **Consumes the handle, and is only correct for a sole owner.**
    /// [`RealCommandRunner::wait`] untracks the child *before* blocking, so
    /// from the moment this is called the child can never be signalled again
    /// — by anyone. Taking the handle by value is what enforces that: a
    /// handle shared with a supervision loop or with [`super::shutdown`] is
    /// held as an `Arc<ChildHandle>` and simply cannot be passed here.
    ///
    /// If you want the child *dead* rather than its exit status, you want
    /// [`reap`](Self::reap).
    fn wait(&self, handle: ChildHandle) -> Option<i32>;

    /// Terminates a child and confirms it is really gone: `SIGTERM`, poll
    /// until it exits, escalate to `SIGKILL` if it will not.
    ///
    /// This is the correct replacement for the `signal(Term); wait();` pair
    /// that used to appear at every restart path. That pair was wrong twice
    /// over: `signal()` alone never removes the entry from
    /// `RealCommandRunner`'s tracked-children table (an unbounded leak across
    /// a long-running container's restarts), and the `wait()` that was
    /// supposed to fix that untracked the child up front — so if anything
    /// else still held the handle, it was silently defeated from then on.
    ///
    /// Polling `is_alive` instead has neither problem: the entry is dropped
    /// only once the child has *actually exited*, so a concurrent holder
    /// keeps working right up until there is nothing left to signal. That
    /// makes this safe to call while others still hold a claim, which is why
    /// it borrows. It also terminates: an old charon that ignores `SIGTERM`
    /// gets `SIGKILL` rather than blocking a supervision thread forever.
    ///
    /// Provided, not required — there is one correct implementation, written
    /// in terms of the methods above, so no implementor can get it wrong.
    fn reap(&self, handle: &ChildHandle) {
        self.signal(handle, Signal::Term);
        for _ in 0..REAP_MAX_POLLS {
            if !self.is_alive(handle) {
                return;
            }
            self.sleep(REAP_POLL_INTERVAL);
        }
        self.signal(handle, Signal::Kill);
        for _ in 0..REAP_MAX_POLLS {
            if !self.is_alive(handle) {
                return;
            }
            self.sleep(REAP_POLL_INTERVAL);
        }
    }

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

    /// Best-effort TCP connect probe (matches the original script's `exec
    /// 3<>"/dev/tcp/$host/$port"` readiness/keepalive check) — `true` iff a
    /// connection was established within a short timeout. Routed through the
    /// runner like every other real-world effect so the vpcd-readiness gate
    /// and tunnel keepalive decision logic stay testable without a real
    /// socket.
    fn tcp_connect_ok(&self, host: &str, port: u16) -> bool;
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

    fn signal(&self, handle: &ChildHandle, sig: Signal) {
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
        //
        // Why this closes it rather than just narrowing it: POSIX guarantees
        // a child's pid is NOT returned to the system's free pool until it
        // has both (a) exited and (b) been reaped by ITS OWN PARENT via
        // wait()/waitpid() — a zombie's pid is reserved the entire time it
        // sits unreaped, globally, not just "protected from us." The only
        // code in this whole process that ever calls wait()/try_wait() on a
        // handle tracked here is gated behind this same mutex (spawn_detached
        // reaps its OWN, unrelated children on a separate thread, never
        // touching `self.children`); this binary also registers no SIGCHLD
        // handler anywhere (grep the codebase: the only `signal::unix::
        // signal` call requests `SignalKind::terminate()`, and nothing uses
        // `tokio::process`, which is the only other thing that would install
        // one). So between this method's try_wait() and its `kill` call, the
        // pid cannot yet be reused by anything, in this process or any
        // other, because nothing capable of reaping it runs outside this
        // critical section. If a future change adds a second reaper of these
        // handles (e.g. a global SIGCHLD handler, or `tokio::process` for
        // these specific children) outside this lock, this reasoning stops
        // holding and the mutex alone no longer suffices.
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
        // (tools/count-unsafe.sh), and this is exactly the original script's
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

    fn is_alive(&self, handle: &ChildHandle) -> bool {
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

    fn tcp_connect_ok(&self, host: &str, port: u16) -> bool {
        use std::net::ToSocketAddrs;
        let Ok(mut addrs) = (host, port).to_socket_addrs() else {
            return false;
        };
        let Some(addr) = addrs.next() else {
            return false;
        };
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(3)).is_ok()
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
        assert!(
            !runner.is_alive(&handle),
            "`true` should have exited by now"
        );
        // Must not panic/error — and, per the fix, must not attempt to
        // signal the stale pid at all.
        runner.signal(&handle, Signal::Term);
        assert_eq!(runner.children.lock().unwrap().len(), 0);
    }

    /// `wait` untracks the child, and the type system now enforces that
    /// nothing can name it afterwards.
    ///
    /// This test used to end by calling `runner.signal(handle, Signal::Kill)`
    /// on the just-waited handle and asserting that it was harmlessly
    /// ignored. That line no longer compiles — `wait` takes the handle by
    /// value and `ChildHandle` is not `Copy` — which is the entire point of
    /// the change: the silent no-op that shipped seven times is now a
    /// borrow-checker error at the call site instead of something a test has
    /// to remember to check for.
    #[test]
    fn wait_untracks_the_child_and_consumes_the_handle() {
        let runner = RealCommandRunner::new();
        let handle = runner.spawn(ChildSpec::new(["true"])).unwrap();

        let status = runner.wait(handle);

        assert_eq!(status, Some(0));
        assert_eq!(runner.children.lock().unwrap().len(), 0);
        // `runner.signal(&handle, Signal::Kill);` here would be:
        //   error[E0382]: borrow of moved value: `handle`
    }

    #[test]
    fn a_handle_polled_via_is_alive_stays_signalable_the_whole_time_unlike_wait() {
        // Regression test for a real Greptile finding on the vowifi-usim-
        // bridge holder: its supervision loop used to block on wait(), which
        // (per wait_removes_the_entry_so_the_handle_cannot_be_signaled_
        // afterward above) removes the table entry before blocking — so
        // sim_recovery::reset_modem_sim's SIGSTOP/SIGCONT calls to that same
        // handle, issued from a different thread while the holder is still
        // alive, silently no-opped for the holder's entire lifetime.
        // Switching that loop to poll is_alive() instead keeps the table
        // entry — and therefore the handle's signalability — intact for as
        // long as the process is actually alive.
        let runner = std::sync::Arc::new(RealCommandRunner::new());
        let handle = runner.spawn(ChildSpec::new(["sleep", "2"])).unwrap();

        // Shared between the polling thread and this one — exactly the
        // `Arc<ChildHandle>` shape production now uses for a handle that a
        // supervision loop watches while something else must still signal it.
        let handle = std::sync::Arc::new(handle);
        let r1 = std::sync::Arc::clone(&runner);
        let h_poll = std::sync::Arc::clone(&handle);
        let poller = std::thread::spawn(move || {
            while r1.is_alive(&h_poll) {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });

        // Give the poller a moment to start, then confirm the handle is
        // still genuinely tracked and signalable mid-flight — this is
        // exactly the property wait() breaks.
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(runner.children.lock().unwrap().contains_key(&handle.0));
        runner.signal(&handle, Signal::Kill);
        poller.join().unwrap();
        assert!(!runner.is_alive(&handle));
    }

    #[test]
    fn a_still_running_child_can_be_signaled_and_is_observed_dead_after() {
        let runner = RealCommandRunner::new();
        let handle = runner.spawn(ChildSpec::new(["sleep", "30"])).unwrap();
        assert!(runner.is_alive(&handle));
        runner.signal(&handle, Signal::Kill);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!runner.is_alive(&handle));
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

        let handle = std::sync::Arc::new(handle);
        let r1 = std::sync::Arc::clone(&runner);
        let h1 = std::sync::Arc::clone(&handle);
        let liveness_checker = std::thread::spawn(move || {
            for _ in 0..50 {
                r1.is_alive(&h1);
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        });
        let r2 = std::sync::Arc::clone(&runner);
        let h2 = std::sync::Arc::clone(&handle);
        let signaler = std::thread::spawn(move || {
            r2.signal(&h2, Signal::Kill);
        });

        liveness_checker
            .join()
            .expect("liveness checker thread must not panic");
        signaler.join().expect("signaler thread must not panic");

        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!runner.is_alive(&handle));
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

/// The handle-lifecycle contract, asserted against **both** implementations.
///
/// This module exists because of a specific, expensive failure mode: for the
/// whole of specs/021, `MockCommandRunner` and `RealCommandRunner` disagreed
/// about what `wait()` does to a handle, and every unit test in the module
/// ran only against the mock. The same bug shipped seven times, and each one
/// was found by live hardware or code review rather than by the 650-test
/// suite that was supposed to cover exactly this code.
///
/// "Hardware" and "OS process semantics" are different axes. The mock is a
/// legitimate stand-in for the first; it was never entitled to invent its own
/// answers for the second. Anything asserted here runs twice — once against
/// the mock, once against real `sleep`/`true` processes — so a future
/// divergence fails a test in CI instead of a container in production.
#[cfg(test)]
mod conformance {
    use super::*;
    use std::sync::Arc;

    /// Lets the shared assertions below look at each implementation's own
    /// tracked-children table, which is the thing the two disagreed about.
    /// Test-only: production code never asks whether an id is tracked, it
    /// holds a handle or it does not.
    trait TracksChildren {
        fn tracks(&self, id: u64) -> bool;
    }

    impl TracksChildren for RealCommandRunner {
        fn tracks(&self, id: u64) -> bool {
            self.children.lock().unwrap().contains_key(&id)
        }
    }

    impl TracksChildren for MockCommandRunner {
        fn tracks(&self, id: u64) -> bool {
            self.child_ids().contains(&id)
        }
    }

    /// Every invariant callers are entitled to rely on, written once against
    /// the trait.
    fn assert_runner_conformance<R: CommandRunner>(runner: R, long_lived: &[&str]) {
        // 1. A freshly spawned child is alive.
        let handle = runner
            .spawn(ChildSpec::new(long_lived.iter().copied()))
            .unwrap();
        assert!(
            runner.is_alive(&handle),
            "a just-spawned long-lived child must be alive"
        );

        // 2. `is_alive` is non-consuming: polling it repeatedly, as every
        //    supervision loop does, must leave the handle usable — this is
        //    the property the seven-times bug destroyed.
        for _ in 0..3 {
            assert!(runner.is_alive(&handle), "is_alive must not consume");
        }

        // 3. A handle that has only ever been polled is still signalable.
        //    (`Term`'s default disposition terminates.)
        runner.signal(&handle, Signal::Term);

        // 4. ...and the child actually dies as a result, within a bounded
        //    time. `reap` is the operation that guarantees this.
        runner.reap(&handle);
        assert!(
            !runner.is_alive(&handle),
            "after reap the child must be gone"
        );

        // 5. `reap` is idempotent and safe on an already-dead child — the
        //    shutdown plan can and does run over children a supervision loop
        //    already replaced.
        runner.reap(&handle);

        // 6. Signalling a dead handle is a silent no-op, never a panic.
        runner.signal(&handle, Signal::Kill);
        assert!(!runner.is_alive(&handle));

        // 7. A handle shared as `Arc<ChildHandle>` — the shape production
        //    uses whenever a supervision loop and the shutdown plan both
        //    hold a claim — works identically through either clone.
        let shared = Arc::new(
            runner
                .spawn(ChildSpec::new(long_lived.iter().copied()))
                .unwrap(),
        );
        let other_claim = Arc::clone(&shared);
        assert!(runner.is_alive(&shared));
        assert!(
            runner.is_alive(&other_claim),
            "a second claim on the same child must see the same liveness"
        );
        runner.reap(&other_claim);
        assert!(
            !runner.is_alive(&shared),
            "reaping through one claim must be visible through the other"
        );

        // 8. `spawn` hands out distinct identities.
        let a = runner
            .spawn(ChildSpec::new(long_lived.iter().copied()))
            .unwrap();
        let b = runner
            .spawn(ChildSpec::new(long_lived.iter().copied()))
            .unwrap();
        assert_ne!(a.id(), b.id(), "each spawn must be a distinct child");
        runner.reap(&a);
        runner.reap(&b);
    }

    #[test]
    fn mock_runner_satisfies_the_handle_lifecycle_contract() {
        assert_runner_conformance(MockCommandRunner::new(), &["sleep", "60"]);
    }

    /// The same assertions against real `sleep(1)` processes. Slower than the
    /// mock but bounded, and it is the only thing that can catch the mock
    /// drifting away from OS semantics again.
    #[test]
    fn real_runner_satisfies_the_handle_lifecycle_contract() {
        assert_runner_conformance(RealCommandRunner::new(), &["sleep", "60"]);
    }

    /// The specific divergence that caused the seven bugs, pinned directly:
    /// `wait` untracks, `is_alive` (until the child exits) does not. Asserted
    /// on both implementations so they can never disagree about it again.
    fn assert_wait_untracks_but_polling_does_not<R: CommandRunner + TracksChildren>(runner: R) {
        let polled = runner.spawn(ChildSpec::new(["sleep", "60"])).unwrap();
        assert!(runner.is_alive(&polled));
        assert!(
            runner.is_alive(&polled),
            "polling must leave the child tracked and reachable"
        );
        runner.reap(&polled);

        let waited = runner.spawn(ChildSpec::new(["true"])).unwrap();
        let waited_id = waited.id();
        runner.wait(waited);
        // The handle is gone at the type level; all that is observable is
        // that nothing is tracked under its id any more.
        assert!(
            !runner.tracks(waited_id),
            "wait must untrack the child in every implementation"
        );
    }

    #[test]
    fn mock_wait_untracks_but_polling_does_not() {
        assert_wait_untracks_but_polling_does_not(MockCommandRunner::new());
    }

    #[test]
    fn real_wait_untracks_but_polling_does_not() {
        assert_wait_untracks_but_polling_does_not(RealCommandRunner::new());
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
        /// Overrides `tcp_connect_ok`'s return value for a given
        /// `"host:port"` key; unseeded keys default to `false` (no
        /// real network in tests).
        pub tcp_connect_results: Mutex<Map<String, bool>>,
        /// Argv substrings that make a future `spawn()` create its child
        /// already dead (`is_alive` false from the start) — lets a test
        /// force a deterministic `EstablishOutcome::FatalProcessDied` for a
        /// specific process (e.g. "charon") without racing a concurrently
        /// spawned sibling (e.g. vowifi-usim-bridge, spawned on its own
        /// thread) for handle-ID order.
        pub born_dead_substrings: Mutex<Vec<String>>,
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

        /// Seeds the stdout a future `run_in_netns(netns, argv)` returns.
        /// The convenience form of [`Self::set_run_output`] for the
        /// namespace-scoped variant, whose key encoding (`netns:<ns>:<argv>`)
        /// is an implementation detail callers should not have to spell out.
        pub fn set_netns_output(&self, netns: &str, argv: &[&str], stdout: &str) {
            let key = format!("netns:{netns}:{}", argv.join(" "));
            let mut out = empty_output(0);
            out.stdout = stdout.as_bytes().to_vec();
            self.run_outputs.lock().unwrap().insert(key, out);
        }

        /// Seeds `tcp_connect_ok(host, port)`'s return value.
        pub fn set_tcp_connect_ok(&self, host: &str, port: u16, ok: bool) {
            self.tcp_connect_results
                .lock()
                .unwrap()
                .insert(format!("{host}:{port}"), ok);
        }

        pub fn kill_child(&self, handle: &ChildHandle, exit_code: i32) {
            if let Some(c) = self.children.lock().unwrap().get_mut(&handle.0) {
                c.alive = false;
                c.exit_code = Some(exit_code);
            }
        }

        /// A future `spawn()` whose argv contains `needle` creates a child
        /// that is dead (`is_alive` false) from the moment it's spawned.
        pub fn set_born_dead_if_argv_contains(&self, needle: &str) {
            self.born_dead_substrings
                .lock()
                .unwrap()
                .push(needle.to_string());
        }

        /// Whether `wait()` was ever called for the child with this id.
        /// Takes an id rather than a `&ChildHandle` because the interesting
        /// assertion is almost always about a handle that has since been
        /// consumed by `reap`/`wait` — capture `handle.id()` before handing
        /// ownership away.
        pub fn waited_on(&self, id: u64) -> bool {
            self.wait_calls.lock().unwrap().iter().any(|h| h.id() == id)
        }

        /// Ids of every child `spawn` has handed out, oldest first — for
        /// tests asserting about a child the code under test spawned
        /// internally, whose handle the test therefore never sees.
        pub fn child_ids(&self) -> Vec<u64> {
            let mut ids: Vec<u64> = self.children.lock().unwrap().keys().copied().collect();
            ids.sort_unstable();
            ids
        }

        /// Signals delivered to the child with this id. The id-taking
        /// variant of [`Self::signals_for`], for assertions about a child
        /// whose handle has since been moved into a `RefCell`, a shutdown
        /// plan, or `wait` — capture `handle.id()` before handing it over.
        pub fn signals_for_id(&self, id: u64) -> Vec<Signal> {
            self.children
                .lock()
                .unwrap()
                .get(&id)
                .map(|c| c.signals_received.clone())
                .unwrap_or_default()
        }

        pub fn signals_for(&self, handle: &ChildHandle) -> Vec<Signal> {
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
            let born_dead = self
                .born_dead_substrings
                .lock()
                .unwrap()
                .iter()
                .any(|needle| spec.argv.iter().any(|a| a.contains(needle.as_str())));
            self.children.lock().unwrap().insert(
                id,
                MockChild {
                    alive: !born_dead,
                    exit_code: if born_dead { Some(1) } else { None },
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

        fn signal(&self, handle: &ChildHandle, sig: Signal) {
            if let Some(c) = self.children.lock().unwrap().get_mut(&handle.0) {
                c.signals_received.push(sig);
                // `Term` kills too, not just `Kill`: the default disposition
                // of SIGTERM is to terminate, and every child this
                // supervises is an ordinary process that does exactly that.
                // Modelling `Term` as survivable made `reap` always burn
                // through its full SIGTERM poll budget and escalate to
                // SIGKILL in tests, which is the opposite of what happens in
                // production and would have hidden a genuine "we never
                // escalate" regression behind an always-escalating mock.
                if matches!(sig, Signal::Kill | Signal::Term) {
                    c.alive = false;
                }
            }
        }

        fn is_alive(&self, handle: &ChildHandle) -> bool {
            self.children
                .lock()
                .unwrap()
                .get(&handle.0)
                .map(|c| c.alive)
                .unwrap_or(false)
        }

        fn wait(&self, handle: ChildHandle) -> Option<i32> {
            let id = handle.0;
            self.wait_calls.lock().unwrap().push(handle);
            // Mirrors `RealCommandRunner::wait`, which removes the entry
            // before blocking. The mock deliberately did NOT do this, and
            // that single divergence is what hid the same handle-lifecycle
            // bug seven separate times: a supervision loop that waited on a
            // handle it still needed passed every mock test and silently
            // broke against real processes. `assert_runner_conformance`
            // below now pins the two implementations to the same semantics.
            self.children
                .lock()
                .unwrap()
                .remove(&id)
                .and_then(|c| c.exit_code)
        }

        fn sleep(&self, d: std::time::Duration) {
            self.sleeps.lock().unwrap().push(d);
        }

        fn tcp_connect_ok(&self, host: &str, port: u16) -> bool {
            self.tcp_connect_results
                .lock()
                .unwrap()
                .get(&format!("{host}:{port}"))
                .copied()
                .unwrap_or(false)
        }
    }

    #[test]
    fn a_freshly_spawned_child_is_alive_with_no_signals_received() {
        let runner = MockCommandRunner::new();
        let handle = runner.spawn(ChildSpec::new(["echo", "hi"])).unwrap();
        assert!(runner.is_alive(&handle));
        assert!(runner.signals_for(&handle).is_empty());
    }

    #[test]
    fn killing_a_child_marks_it_dead_and_records_the_signal() {
        let runner = MockCommandRunner::new();
        let handle = runner.spawn(ChildSpec::new(["sleep", "100"])).unwrap();
        runner.signal(&handle, Signal::Kill);
        assert!(!runner.is_alive(&handle));
        assert_eq!(runner.signals_for(&handle), vec![Signal::Kill]);
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
