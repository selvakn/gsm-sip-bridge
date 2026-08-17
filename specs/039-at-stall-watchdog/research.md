# Phase 0 Research: Bounded modem I/O and stalled-line detection

**Feature**: 039-at-stall-watchdog | **Date**: 2026-08-17

All Technical Context unknowns are resolved. No `NEEDS CLARIFICATION` remains — the
five clarification answers plus the incident forensics settled every open decision.

## R1. Why the existing 5s AT timeout does not bound anything

**Finding**: `serialport` 4.9.0's `TTYPort::read` (`posix/tty.rs:467`) is:

```rust
fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    if let Err(e) = super::poll::wait_read_fd(self.fd, self.timeout) { return Err(...); }
    nix::unistd::read(self.fd, buf).map_err(...)          // <-- unguarded, blocking
}
```

`open()` sets `O_NONBLOCK` (`tty.rs:127`) and then **deliberately clears it**
(`tty.rs:176`). So once `ppoll` reports readable, the following `read(2)` is a plain
blocking read on a tty with `VMIN=1 VTIME=0` — it sleeps until at least one byte
arrives, with no deadline. If the input buffer empties between the poll and the read,
the thread parks forever.

**Evidence** (captured live from the wedged process, `tmp/ims-stall-forensics-2026-08-16/`):
kernel stack `wait_woken ← n_tty_read ← tty_read ← vfs_read ← ksys_read`; syscall
`read(fd=11 → /dev/ttyUSB2, buf, 8192)`; `stty` confirming `min = 1; time = 0`. The
8192 length identifies the caller as `read_response`'s `BufReader` (default capacity).

**Decision**: treat the crate's timeout as advisory. Bound AT work from *outside* the
call rather than trusting the transport.

## R2. How to bound AT I/O — worker thread

**Decision**: `AtCommander` keeps its public API, but the real port moves to a worker
thread it owns. Each command is sent to the worker over a channel; the caller waits
with `recv_timeout`. A blocked worker can no longer block its caller.

**Rationale**: the alternatives were rejected by the owner for concrete reasons:

| Alternative | Rejected because |
|---|---|
| Re-apply `O_NONBLOCK` to serialport's fd and drive `poll`/`read` ourselves | `TTYPort` exposes only `AsRawFd`, not `AsFd`, so this needs `unsafe { BorrowedFd::borrow_raw }`; `tools/count-unsafe.sh` (wired into `make lint`) forbids it |
| Drop `serialport`, open the tty via `rustix` | Structurally the best fix, but reimplements termios/baud/flock and touches every AT consumer — the highest-risk change available, and rejected in favour of shipping safely |
| One documented `unsafe` exception | Smallest diff, but breaks a project-wide invariant for one call site |

**Consequence accepted in the spec**: a wedged worker leaks its thread, its port handle
and its `flock`. That is *why* FR-007's automatic recovery is mandatory rather than
optional — the two decisions are load-bearing for each other.

**Constitution V (simplicity)**: this adds a thread and a channel per open port. Justified
in Complexity Tracking — there is no way to bound an uncancellable blocking syscall from
within the calling thread.

## R3. Preventing response desynchronisation

**Finding**: `read_response` (`at_commander.rs:221`) constructs `BufReader::new(port)`
per command and drops it on return, discarding anything buffered past the terminating
`OK`. A reply that arrives after a timeout is therefore read as the *next* command's
response, and every command after that is off by one — permanently.

**Decision**: the worker owns the port across commands, so it also owns the read buffer.
On a caller timeout the worker's reply send fails (the caller dropped its receiver);
the worker then drains input to quiescence before accepting the next command.

**Alternative rejected**: correlating requests and replies with sequence numbers. AT has
no request identifier to correlate on, so this would mean inventing one out of band.

## R4. Recovering an abandoned channel (FR-036 / FR-037)

**Problem the clarification exposed**: a line that fast-fails every operation is *making
progress*, so lack-of-progress monitoring would never rescue it. Something else must.

**Decision**: a three-step escalation, cheapest first.

1. **Resync** — ask the worker to drain and round-trip a bare `AT`, with a short
   deadline. Succeeds whenever the worker was merely slow; costs nothing and avoids a
   restart entirely.
2. **Reopen** — if the worker does not answer the resync, attempt a fresh open of the
   port. Succeeds if the wedged worker has since finished and dropped the handle.
3. **Recover the line** — if the port cannot be reopened (the abandoned worker still
   holds the `flock`), the line is unusable in this process; restart it.

**Rationale**: step 1 is the common case and is free; step 2 is what FR-036 literally
asks for; step 3 is the guarantee. Going straight to restart (the simpler design) would
restart on every transient slow reply.

## R5. Watchdog budgets

**Decision**: budgets are *derived* from the sum of the timeouts of the operations each
phase performs, with ~25% margin, and a unit test recomputes the derivation from the
real constants and asserts the margin still holds.

**Rationale**: FR-033. Hand-set budgets rot silently — the failure mode is a future
timeout bump quietly turning the watchdog into a false-restart generator. Deriving them
and testing the derivation makes that a build failure instead.

Inputs to the renewal derivation (all current constants):
`MODEM_OPEN_MAX_WAIT` 30s + EF_DIR/SELECT APDU walk (≤34 APDUs × `DEFAULT_TIMEOUT` 5s)
+ 2 × SIP Timer B 32s + Gm SA install (20 × `ip xfrm`) ≈ 284s → budget 360s.
The SMS sweep's own open retry (`OPEN_RETRY_ATTEMPTS` 4 × `OPEN_RETRY_BASE_DELAY` 300ms
linear ≈ 1.8s) feeds the sweep budget.

**Two-sample confirmation**: sample every 5s and act only on two consecutive overruns of
the same phase instance. Costs 5s against a 360s budget and removes single-sample
artefacts.

**Monotonic clock only** (`Instant`, never `SystemTime`) so an NTP step cannot be
mistaken for a stall (FR-014).

## R6. Why recovery is process exit

**Decision**: on a confirmed stall, log one machine-greppable marker and exit non-zero.

**Rationale**: a thread blocked in `read(2)` cannot be cancelled in safe Rust, and it
holds the port. Only process death releases it. The supervisor
(`supervise/orchestrate.rs:1574`) already restarts an exited agent within 5s, and
agents are one process per line, so the blast radius is one line.

**Two properties make the marker survivable**: logging is synchronous to stderr
(`observability/logging.rs`, no non-blocking writer), and the supervisor redirects agent
stderr to `/tmp/ims-agent-{idx}.out` — the exact file it reads back after an exit to
classify the failure.

## R7. Reusing the existing escalation

**Finding**: `supervise/sim_recovery.rs` already implements exactly the needed ladder —
`has_csim_failure` greps the agent log, `IncidentCounters::observe` counts strikes,
`Action::ResetSim` drives `AT+CFUN=0 → 1`, and repeated resets end in give-up plus a
Discord alert via `sim_alert_transition`.

**Decision**: add `has_at_stall` alongside `has_csim_failure` and count an AT stall
against the **same** incident counter. An unresponsive AT channel and a failing
`AT+CSIM` are one physical fault with one remedy.

**Post-give-up behaviour (FR-030)**: give-up becomes a slow-retry state rather than a
terminal one, so a line whose hardware recovers returns to service unattended, with the
alert still fired only once per incident (FR-031).

## R8. Making the stall visible

**Finding**: the Prometheus heartbeat is emitted by `observability-reporter-*`, a thread
independent of the dispatch loop, which re-reports cached state
(`observability/reporter.rs:156-162`). That is why `agent_up` stayed 1 for three hours.

**Decision**: gate the heartbeat on progress — skip the enqueue while the shared
`Progress` reports stalled. Report age then crosses `staleness_threshold`, and
`metrics/server.rs:33-59` *already* zeroes the VoWiFi gauges for a stale agent.

**Rationale**: one conditional reuses machinery that exists and is already tested, rather
than adding a parallel liveness path.

## R9. Registrar-granted lifetime

**Finding**: `granted_expires` already exists and is correct at
`volte/registration.rs:57`, with tests. `ims` must not depend on `volte` — the dependency
runs the other way (`volte/registration.rs:277` calls `ims::renewal_due`).

**Decision**: move it to `ims/mod.rs` beside `renewal_due` and re-export from
`volte::registration` so existing callers and tests are untouched.

**Latent bug it exposes**: honouring a short grant against the fixed 300s
`RENEWAL_HEADROOM` makes `renewal_due` permanently true — the agent would re-register on
every idle poll forever. Hence `renewal_headroom_for(granted, preferred) =
preferred.min(granted / 2)` (FR-024), applied to both bearers.

## R10. Orphan reaping without breaking the supervisor

**Trap**: a naive `waitpid(-1, WNOHANG)` loop in PID 1 steals exit statuses from the
`std::process::Child` handles `supervise` relies on, making `is_alive` report false for
live children and `Command::output()` fail with `ECHILD`.

**Decision**: peek with `WNOWAIT` (non-destructive), and claim a pid only if the
supervisor does not own it; otherwise leave it for its owner. Requires an owned-pid
registry in `RealCommandRunner` covering transient `output()` children.

**Source removal first**: the keepalive at `orchestrate.rs:1655-1671` shells out to
`timeout 3 bash -c '>/dev/tcp/…'` every 30s and orphans the inner process. Replacing it
with the existing `runner.tcp_connect_ok_in_netns` removes the known source; the reaper
is then defence in depth. 456 keepalive cycles in 3.8h ≈ the 462 orphans observed.

## R11. Testing approach under Constitution I

Constitution I requires real components, with mocks only where a real component is
impractical and justified in a comment at the mock site.

**Decision**: exercise the bounded AT path over a **real pseudo-terminal or socketpair**
rather than a scripted in-memory mock. Both are real OS file descriptors with real
blocking semantics, so the deadline, drain and resync logic is genuinely exercised — and
a fake modem that simply never writes reproduces the production fault exactly.

The existing scripted transports remain for callers that only assert AT *grammar*; they
are pre-existing and out of scope for this change.
