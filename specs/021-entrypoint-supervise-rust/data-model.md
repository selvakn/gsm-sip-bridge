# Phase 1 Data Model: Container Orchestration Move into the Rust Supervisor

## CommandRunner (trait, `supervise::runner`)

The single injectable boundary between decision logic and the outside world. Two
implementations: `RealCommandRunner` (production — `std::process::Command`/`Child`,
`std::fs`) and `MockCommandRunner` (test-only — in-memory bookkeeping, `#[cfg(test)]`
or a `test-support` feature).

| Method | Purpose |
|---|---|
| `run(argv) -> Output` | One-shot command, captures stdout/stderr/status. Replaces direct `ip`/`dig`/`swanctl`/`stty` shell-outs. |
| `run_in_netns(netns, argv) -> Output` | Same, prefixed with `ip netns exec <netns>`. |
| `read_file(path) -> String` | Reads a log/state file (charon.log, reset_log, agent_log, pcscf_path). |
| `write_file(path, contents)` | Writes a rendered asset or state file. |
| `spawn(ChildSpec) -> ChildHandle` | Starts a long-running child (charon, pcscd, an agent, a keepalive loop, the GSM daemon), returns an opaque handle. |
| `signal(handle, Signal)` | SIGTERM/SIGKILL/SIGSTOP/SIGCONT a previously spawned child. |
| `is_alive(handle) -> bool` | `kill -0`-equivalent liveness check. |
| `wait(handle) -> Option<i32>` | Blocks until exit, returns status. |

**Validation rule**: every `MockCommandRunner` use in a test carries a `// MOCK JUSTIFICATION:`
comment naming the real component it stands in for (charon/pcscd/swanctl/a live serial modem),
per the constitution's Integration-First Testing principle.

## ChildSpec / ChildHandle

- `ChildSpec { argv: Vec<String>, netns: Option<String>, stdout_capture_path: Option<PathBuf> }` —
  everything needed to start a child exactly as the current script does (e.g. `tee`-ing charon's
  stdout to a per-line log file).
- `ChildHandle(u64)` — opaque; real impl maps to a `std::process::Child`, mock impl maps to a
  synthetic table entry.

## LineState (enum, `supervise::line_supervisor`)

- `Establishing { attempt: u32, stuck_without_pcscf: bool }` — tunnel not yet up; mirrors the
  current establish-time wait loop's `attempt`/`stuck_without_pcscf` locals.
- `Up { pcscf: String }` — tunnel established, current P-CSCF address recorded.
- `Degraded { reason: DegradeReason }` — one steady-state failure detected this tick.
- `Restarting` — recovery action issued, next tick re-evaluates.

`DegradeReason`: `ProcessDied | ViciBroken | TunVanished | ChildSaMissing | PcscfChanged`. Each
maps 1:1 to one of the current steady-state loop's four `if` branches plus the P-CSCF-changed
check.

**State transitions** (validated by table-driven tests in `test_supervise_line.rs`):

| From | Observation | To | Action |
|---|---|---|---|
| Establishing | CHILD_SA established + P-CSCF found | Up | record P-CSCF, start line tail |
| Establishing | CHILD_SA established, no P-CSCF, N×2s elapsed | Establishing (stuck) | terminate + re-initiate |
| Establishing | process died | (terminal — line skipped) | log FATAL, return to caller |
| Up | process died | Degraded(ProcessDied) → Restarting | kill stale pidfile, respawn, reload, re-initiate, restart ims-agent |
| Up | `swanctl --list-sas` fails | Degraded(ViciBroken) → Restarting | same recovery sequence |
| Up | tun iface missing from netns | Degraded(TunVanished) → Restarting | recreate iface, terminate+reinitiate |
| Up | `ims:` CHILD_SA absent from list-sas | Degraded(ChildSaMissing) → Restarting | re-initiate only |
| Up | P-CSCF in log ≠ recorded | Degraded(PcscfChanged) → Up | refresh file, restart ims-agent only |

## TeardownStep / ShutdownPlan (`supervise::shutdown`)

- `TeardownStep`: `KillChild{handle, signal}`, `WaitForExit{handle, timeout}`,
  `RunInNetns{netns, argv}`, `Run{argv}`, `DeleteNetns{netns}`.
- `ShutdownPlan { steps: Vec<TeardownStep> }`, built by `build_shutdown_plan(&StartedState)`.
- `StartedState`: the accumulated record of everything that actually started this run — the typed
  replacement for the ~15 bash PID arrays (`DAEMON_SUPERVISOR_PID`, `CHARON_PIDS`,
  `VOLTE_STARTED_LINE_NETNS`, etc.), appended-to only on successful start, mirroring the existing
  append-on-success discipline (`STARTED_NETNS`'s own comment already states this invariant).

**Ordering invariants** (each a named test):
1. Every line's child-kill steps precede that line's PDN/namespace-teardown steps.
2. A VoLTE line's `volte-cleanup` step is scoped `RunInNetns` and precedes that namespace's
   `DeleteNetns`.
3. Any child that may block mid-AT-transaction (vowifi-usim-bridge holder, VoLTE
   register/bridge/carrier-agent) is torn down with `Signal::Kill`, never `Signal::Term`.

## Render inputs/outputs (`supervise::render`)

Pure functions, one per current heredoc:

- `render_strongswan_conf(idx, vici_socket, charon_log) -> String`
- `render_swanctl_epdg(params: SwanctlEpdgParams) -> String` where
  `SwanctlEpdgParams { imsi, mcc, mnc, epdg_ip, if_id, updown_script, src_addr: Option<String> }`
  — the `src_addr: None` branch omits the `local_addrs` line exactly as the current `sed -e
  "/local_addrs.*@SRC_ADDR@/d"` deletion does.
- `render_updown_script(idx, netns, tun_iface) -> String`
- `render_vpcd_reader_conf(port: u16) -> String`

Each has an `insta` snapshot per meaningfully-distinct input (including the `src_addr`
present/absent pair for `render_swanctl_epdg`).

## DaemonSupervisor (`supervise::daemon_supervisor`)

No new type — one function, `run_supervised(runner: &dyn CommandRunner, argv: &[&str], restart_delay: Duration) -> !`.
`restart_delay` is the existing 5s constant, named not inlined.
