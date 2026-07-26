# Phase 0 Research: Container Orchestration Move into the Rust Supervisor

## R1: CommandRunner trait shape

**Decision**: Split into two families on one trait:

```rust
pub trait CommandRunner: Send + Sync {
    // Transient leaf operations (replace direct Command::new call sites)
    fn run(&self, argv: &[&str]) -> io::Result<std::process::Output>;
    fn run_in_netns(&self, netns: &str, argv: &[&str]) -> io::Result<std::process::Output>;
    fn read_file(&self, path: &Path) -> io::Result<String>;
    fn write_file(&self, path: &Path, contents: &str) -> io::Result<()>;

    // Long-running child ownership (clarified 2026-07-26: in scope)
    fn spawn(&self, spec: ChildSpec) -> io::Result<ChildHandle>;
    fn signal(&self, handle: ChildHandle, sig: Signal);
    fn is_alive(&self, handle: ChildHandle) -> bool;
    fn wait(&self, handle: ChildHandle) -> Option<i32>; // blocks until exit, returns status
}
```

**Rationale**: The current script's decision logic is entangled with *how* it observes the
world — `grep` against a tailed log file, `swanctl --list-sas`'s stdout, `kill -0`'s exit
code. Separating "fetch the current observation" (`read_file`, `run` capturing `Output`) from
"decide what to do with it" (pure functions over `String`/`Output`) means the parsing logic —
which is where the actual bugs hide (P-CSCF regex, CSIM-failure detection, CHILD_SA-established
detection) — is unit-tested with **zero** `CommandRunner` involvement, exactly like the existing
`extract_latest_pcscf` and `netcfg::parse_ip_addr_show`. The runner only needs to be mocked for
the *decision sequencing* tests (what does the supervisor do next, given these observations).

**Alternatives considered**:
- *Runner returns only `Output`, no `read_file`*: rejected — charon.log accumulates across the
  container's lifetime and is scraped independently of any single command's own stdout; modeling
  it as a file read is what the current script actually does, and forcing it through a command
  (`cat file`) would be an indirection with no benefit.
- *Fully async trait (`async fn`)*: rejected per the concurrency-model decision in plan.md — the
  whole codebase's equivalent work is threads + blocking I/O; introducing `async_trait` here would
  be a second concurrency shape for no behavioral gain.
- *One giant `Runner::execute(Step)` enum covering everything, no separate spawn/signal*: considered
  and rejected — long-running children need a stable handle across multiple ticks (is it still
  alive? kill it specifically), which a fire-and-forget `execute` can't represent cleanly; the
  split mirrors how `std::process::Child` already differs from `Command::output()`.

## R2: ChildHandle / process identity in tests

**Decision**: `ChildHandle(u64)` — an opaque id. `RealCommandRunner` maps it to a real
`std::process::Child` (keyed in an internal `Mutex<HashMap<u64, Child>>`); `MockCommandRunner`
maps it to a synthetic bookkeeping entry the test controls (alive/dead, last signal received,
exit code to report). Neither the trait nor calling code ever sees a raw pid, so tests never need
a real OS process to exist.

**Rationale**: Matches the constitution's mock-only-for-hardware carve-out precisely — the
*process identity* is what's being faked, not the decision logic around it.

## R3: Log-scraping stays pure

**Decision**: Every current `grep`-against-a-log-file check becomes a pure function
`fn parse_x(content: &str) -> X`, ported 1:1 from the existing regex/`grep` pattern (e.g.
`extract_latest_pcscf`'s "P-CSCF server IP" line-splitting, `AT+CSIM failed` substring check,
`CHILD_SA.*established` pattern, `swanctl --list-sas`'s `^ims:` line check). These already exist
as bash one-liners with tested regexes (`extract_latest_pcscf` even has a `sed`/`grep` comment
explaining the "last matching line overall, not last-of-one-family" fix from Greptile PR #2) — the
port is direct, and each becomes its own `#[test]` with the exact input that motivated the original
bash comment (an established SA line with no P-CSCF yet; a rekeyed P-CSCF; a non-CSIM failure).

**Rationale**: These are exactly the "hard-won operational knowledge trapped in comments" the spec
calls out (FR-009) — turning the comment's example into the test's fixture is the mechanism.

## R4: insta snapshot layout

**Decision**: `insta` with default settings — snapshots live in
`gsm-sip-bridge/src/supervise/snapshots/` (co-located with `render.rs`, insta's convention when
`#[test]`s live in the same crate as the code, via `cargo insta test`/`review`). Reviewed and
committed as regular text files (`.snap`), diffed in code review like any other file — no binary
format, no external snapshot service.

**Rationale**: Direct, greppable, and needs no new CI infrastructure — `cargo test` fails loudly on
a snapshot mismatch without `cargo-insta` installed (it just doesn't offer the interactive review
UI), so `make test` keeps working with only the dev-dependency added.

**Alternatives considered**: hand-rolled `assert_eq!` against inline `const EXPECTED: &str = "..."`
— rejected only because `insta` handles the "here is a 30-line rendered config file" case far more
readably (diff-on-failure) with negligible added dependency surface; the constitution's Simplicity
principle weighs added *maintenance* burden, not a well-scoped, widely-used dev-dependency.

## R5: bats-core for Phase 0

**Decision**: Vendor `bats-core` as a git-ignored/CI-fetched test harness (not a submodule to avoid
version-pin complexity) invoked via a new `make test-bash` target folded into `make test`. Covers
only the Phase-0-extracted pure helpers (`extract_latest_pcscf`, `render_line_*`) in
`docker/lib/render_helpers.sh`, which are deleted once Phase 1 ports them to Rust — so this
tooling's lifetime is deliberately one phase long.

**Rationale**: Phase 0's entire purpose is a safety net *before* the port; bats is the standard
tool for this and needs no bespoke test runner.

## R6: LineSupervisor state machine shape

**Decision**: One enum, mirroring the three existing loops' actual states (not an idealized
redesign):

```rust
enum LineState {
    Establishing { attempt: u32, stuck_without_pcscf: bool },
    Up { pcscf: String },
    Degraded { reason: DegradeReason },
    Restarting,
}
enum DegradeReason { ProcessDied, ViciBroken, TunVanished, ChildSaMissing }
```

`LineSupervisor::tick(&self, runner: &dyn CommandRunner) -> LineState` is called on the same 30s
(steady-state) / 2s (establishing) cadence the current script sleeps on — ported as named
constants, not redesigned. The strongswan- and swu-specific *actions* taken on each transition
differ (strongswan re-initiates via `swanctl`; swu just respawns the dialer process), so
`LineSupervisor` is generic over a small `TunnelEngine` trait (`reinitiate`, `is_established`,
`extract_pcscf`) implemented once per engine — this is what collapses the two engines' duplicate
loops into one state machine without losing engine-specific behavior.

**Rationale**: Directly satisfies FR-005 (one supervision implementation) while preserving every
existing transition (FR-008/FR-009) — the state names and transitions are read off the current
script's own control flow, not invented.

## R7: Shutdown plan step shape

**Decision**:

```rust
enum TeardownStep {
    KillChild { handle: ChildHandle, signal: Signal },
    WaitForExit { handle: ChildHandle, timeout: Duration },
    RunInNetns { netns: String, argv: Vec<String> },
    Run { argv: Vec<String> },
    DeleteNetns { netns: String },
}
struct ShutdownPlan { steps: Vec<TeardownStep> }
```

`build_shutdown_plan(&StartedState) -> ShutdownPlan` is pure and unit-tested for ordering
(kill-before-teardown, netns-scoped `volte-cleanup` before that namespace's deletion, SIGKILL for
AT-transaction-blockable children). `ShutdownPlan::execute(&dyn CommandRunner)` is the thin
executor — the only place a real signal is ever sent.

**Rationale**: Direct application of the `netcfg.rs` pattern (steps as data, executor separate) to
the trap/cleanup path, satisfying FR-004.

## R8: daemon_supervisor (circuit-switched GSM daemon)

**Decision**: Its own small module with one function,
`run_supervised(runner: &dyn CommandRunner, argv: &[&str]) -> !` (loop: spawn, wait, log exit
status, sleep 5s, respawn) — moved per the 2026-07-26 clarification so `entrypoint.sh` retains no
supervision loops of any kind. This is the simplest of the four moved loops (no state beyond
"restart after every exit"), so it's low-risk and a good first `supervise`-side integration point.

**Rationale**: Explicit clarification answer; also gives Phase 3 (or an early slice of it) a
trivial first case to validate the `CommandRunner` spawn/wait/signal contract against a real
process before the harder per-line state machine is built on top of it.
