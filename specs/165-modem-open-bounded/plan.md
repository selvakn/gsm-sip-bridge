# Implementation Plan: A Hung Modem No Longer Costs a Modem Line Its Registration

**Branch**: `165-modem-open-bounded` | **Date**: 2026-11-28 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `specs/165-modem-open-bounded/spec.md`

## Summary

#239, second part. Design decisions D1–D9 are in [research.md](research.md).

1. **Bound the device open** (`modules/at_commander.rs::open_session`).
   - `serialport::new(..).open()` runs on a short-lived thread `at-open-<port>`.
   - The caller waits `open_bound(timeout) = timeout + WORKER_GRACE`.
   - A result in time behaves exactly as today. No result in time fails with a new `PortOpenStalled("<port> did not open within N ms")`, which `is_port_out_of_step()` recognises and which reads the same; discovery's probes count it as a probe timeout, as they counted a hung open before.
2. **No new open of a port behind a given-up one.**
   - A process-wide map records a port only when a caller gave up on its open, and the open's thread removes its own entry when it finishes.
   - While the entry exists, opens of that port fail at once with `PortOutOfStep("an earlier open of <port> has not returned")`.
   - A port that opens after its caller left is closed by the thread before the entry is removed.
3. **No loop retries it.** The SMS sweep, `open_modem_waiting` and `vowifi-usim-bridge` already return `PortOutOfStep` at once. `try_reopen` gets the same early return.
4. **Budgets count the open.**
   - `PER_MESSAGE_PORT_BUDGET` goes 35 → 40 s.
   - `SWEEP_PASS_BUDGET` goes 72 → 80 s.
   - `Phase::SmsSweep` goes 90 → 100 s.
   - The renewal and startup derivations gain the open's bound and are checked by their existing tests.
5. **No watchdog policy change.** On a hung modem, a sweep pass now ends in about 7 s, or at once while an open is outstanding, so the watchdog has nothing to stop.

## Technical Context

**Language/Version**: Rust 1.99.1 (`rust-toolchain.toml`), crate `gsm-sip-bridge`
**Primary Dependencies**: existing only (`serialport` 4.10.1 behind `AtCommander`). No new crates
**Storage**: N/A
**Testing**: `cargo test` through `make test`.
- The open is tested through an internal `open_session_with(path, timeout, opener)`, with real threads and real blocking: an opener that blocks on a channel the test releases or never releases, and one that returns a `UnixStream` half as the port, whose peer sees EOF when the port is dropped.
- No mocks (constitution I).
- Live: the router drop of quickstart.md.

**Target Platform**: Linux, every process that opens a modem AT port (25 production sites, research F2)
**Project Type**: telecom bridge, a single binary with subcommands
**Performance Goals**: a healthy open costs one thread spawn more than today (tens of µs; SC-005)
**Constraints**:
- the error text read by classifiers keeps `AT command timeout` and never contains `AT+CSIM failed` (F4);
- no second way to open a port that skips `start_worker`'s sync (learnings: Concurrency & shared devices);
- card-reader lines unchanged.

**Scale/Scope**: one module changed (`at_commander.rs`), one constant table (`volte/sms.rs`, `watchdog.rs`), tests and docs

## Constitution Check

| Principle | Status |
|---|---|
| I. Integration-first testing | Pass. Real threads, channels, timeouts and `UnixStream` fds; no mocked boundary. The device itself is replaced by an opener closure, because a USB serial device whose `open(2)` never returns (a DTR control transfer the kernel cannot cancel) cannot be produced in CI; the closure is justified at its site |
| II. Green-on-commit | Pass. `make format`, `make lint`, `make test` before each commit |
| III. Frequent atomic commits | Planned as three commits: the bounded open with its tests; `try_reopen` and the budgets; docs and release notes |
| IV. Makefile-driven build | No new targets |
| V. Simplicity | One thread per open, one map, the existing error kind. The watchdog policy change from #239 is not built (spec, Assumptions; D8) |

Re-checked after design: no change.

## Blast Radius

Real `grep` output from `a2ef631`, paths relative to `gsm-sip-bridge/src`. Every file cited here is the same there as on `main` (`5334fa8`), which this branch was later rebased onto.

### State and invariants inventory

| Changed item | Readers / writers (grep) | Copies at construction | Invariants and comments | Decision |
|---|---|---|---|---|
| `AtCommander::open_session` (now bounded) | `grep -n open_session modules/at_commander.rs`: `:249` (`open_with_timeout`), `:317` (definition), `:602` (`try_reopen`) | none | `open_with_timeout` doc `:228`–`:236`: "a `read(2)` that never comes back therefore strands that worker … until the process exits" | The open itself now strands its own thread the same way. The doc gains one paragraph for the open. Signature unchanged; `open_session_with` is the injectable core, `open_session` passes the `serialport` opener |
| New process-wide outstanding-open map | only `open_session_with` and its thread | none | data-model.md invariants | New `static`, private to `at_commander.rs` |
| `BridgeError::is_port_out_of_step` (now also true for the new `PortOpenStalled`) | `grep -rn is_port_out_of_step src`: `volte/sms.rs:912`, `ims/mod.rs:1175`, `vowifi/usim_bridge.rs:216` (stop retrying), tests | - | Doc: "a modem port that failed its sync on open (#237)" | Widened on purpose: both mean the modem is not answering. Doc updated |
| Discovery probes' open failures (`modules/discovery/probe.rs:144`, `sim.rs:89`) | `select_at_capable_port` quarantine (`record_at_timeout` / `record_at_responded`), `record_sim_timeout` / `record_sim_responded` | - | `ProbeOutcome::NotAtCapable` doc: "the open itself failed cleanly … resets the consecutive-timeout streak" | A stalled open (`is_port_open_stalled`) maps to `TimedOut` / the SIM timeout path, as a hung open did when `run_bounded` gave up on it; other open failures unchanged |
| `try_reopen` loop (`:598`–`:617`) | `REOPEN_ATTEMPTS` `:598`, `:601`, `:608` | - | Comment `:607`: "Expected while the abandoned worker still holds the flock" | Add `Err(e) if e.is_port_out_of_step()` → warn and stop (D6). The busy-port retry stays |
| `WORKER_GRACE` (`:56`) | `:302` (sync), `:470` (command wait), `:568` (resync), test `:1438` | - | Doc `:50`–`:55`: the outer wait "exists only to" catch a worker that never answers | Reused by `open_bound`. Not changed |
| `PER_MESSAGE_PORT_BUDGET` 35 → 40 s | `volte/sms.rs:946`, `:1027`, `:1132`, tests `:1367`, `:1380`, `:1388`, `:1398` | - | Doc `:943`–`:945`: "20 + 1.8 + 5 + 5 = 31.8 s" | Doc and the test's `open_worst` add `DEFAULT_OPEN_BOUND`: 38.8 s |
| `SWEEP_PASS_BUDGET` 72 → 80 s | `:941`, `:963`, tests `:1306`–`:1309`, `:1380` | `PassBudget::start` copies it into `deadline` per pass | Doc `:939`–`:940`: "72 s since the open's sync raised the per-message budget (#237)" | Updated with the new reason |
| `Phase::SmsSweep` budget 90 → 100 s | `ims/agent/watchdog.rs:220`; read by `sms.rs` tests `:1286`, `:1302`, `:1339` | - | `watchdog.rs:217`–`:219`: "A sweep re-opens the port per message; this bounds one pass" | Value only; the margin test (≥ 20 %) stays |
| Renewal / startup derivations | `watchdog.rs` tests `derived_renewal_worst_case` (`:670`), `startup_budget_covers_…` (`:721`) | - | `:671`–`:675` "Opening the modem … then the open's sync" | Add `DEFAULT_OPEN_BOUND` once per open. Budgets unchanged: renewal 440 s against 540 s (22.7 %), startup 477 s against 600 s (25.8 %) |

**Implicit contracts**:
- **Error text.** `PortOutOfStep` displays `discovery error: AT command timeout: …` (`error.rs:48`). That is read as a timeout by `modules/worker.rs:253`, `:829` and `map_registration_error`, and is not read as a SIM fault by `sim_recovery::has_csim_failure`. The open's own errors (`failed to open serial …: <serialport error>`, `:325`) are unchanged, so the scripted texts in `ims/mod.rs:2384`–`:2501` still apply.
- **Retry semantics.** Every open-retry loop stops on `is_port_out_of_step()` (research F3). The SMS sweep's `OPEN_RETRY_ATTEMPTS` and sleeps, `MODEM_OPEN_MAX_WAIT` and `usim_bridge`'s backoff are not touched.
- **Lock order.** The outstanding-open mutex is never held while opening, sending or dropping a port. It is taken alone, briefly, by the caller and by the open's thread. No other lock is taken under it.

### Concurrency and ownership map

| Object | Created by | Owned / held by | Released on success | On timeout (caller gave up) | On shutdown |
|---|---|---|---|---|---|
| `at-open-<port>` thread | `open_session_with` | itself (detached) | exits after sending | keeps running until the device open returns, then closes what it got and exits | dies with the process |
| Handoff slot (`OpenSlot`) | `open_session_with` | caller and thread, each an `Arc` | caller takes `Delivered` under the slot's mutex | caller marks `Abandoned` and adds the map entry under the same mutex; the thread then closes what the open returns on its own thread, and only then removes the entry (D4, D5) | n/a |
| The opened port (fd, `TIOCEXCL`, `flock`) | the thread | caller's `Session` → worker, as today | as today (worker drops it) | dropped by the thread **before** it removes its map entry (D5) | process exit |
| Map entry | caller, on timeout, under the handoff mutex, only if the open is still waiting | the map | n/a | removed by the open's thread, matching its id, after it closed what it got | process exit |
| `ModemLock` guard held across the open by the SMS sweep and the renewal | existing callers | caller's stack frame | as today | the caller now returns within the bound and drops it. Before, a hung open held it indefinitely | as today |

### Where else does this logic run?

- **Other openers.** Every opener goes through `open_with_timeout` → `open_session` (research F2), and `try_reopen` through `open_session` (`:602`). No caller-side change other than `try_reopen`'s early return.
- **Other processes.** `vowifi-usim-bridge`, `vowifi-plmn`, discovery and the CS pool each have their own map. "No new open behind a given-up one" is per process, which is where the leak would be.
- **The dispatch loop.** It never opens the port itself: AKA runs on `SimWorker`, the sweep on its own thread, and startup is before the loop. No dispatch-loop timing changes.

### Situations checklist

| Situation | Behaviour |
|---|---|
| Idle, modem hung | Sweep pass: lock, one open that fails after ≤ 7 s, `modem SMS sweep failed …`. Later passes fail at once while the open is outstanding. Registration untouched |
| In a call | VoWiFi media does not use the modem; the sweep skips the signal poll during a call as today |
| Ringing / origination pending | No modem use on these paths |
| Challenged refresh during the hang | The challenge's open fails in ≤ 7 s (or at once), `open_modem_waiting` does not retry `PortOutOfStep`, so the refresh falls back to a full registration, which fails the same way and is retried on the renewal backoff. Same outcome as today minus the watchdog |
| Full registration / agent start during the hang | Fails within the bound. Startup exits and the supervisor restarts the agent (5 s), as for a missing modem today |
| Device removed during the hang | The stuck open returns an error, the thread removes the entry, and later opens fail at once with `No such file or directory` |
| Device answers again | The stuck open completes; the thread closes the port and removes the entry. The next open syncs as usual (#237), which discards anything stale |
| Error and rollback | No state to roll back; the port is never handed out after a timeout |
| Shutdown | A stranded open thread does not hold up exit (detached) |
| Limits vs the existing path | The bound is the same `timeout + WORKER_GRACE` as the sync's (`:302`). Budgets are recomputed with it (F5) |
| Blocks the dispatch loop | No |
| Docs and repo rules | `docs/operations.md` (stall section: what a hung modem now looks like); `docs/observability.md` (the two new failure messages in the sweep's warning); `RELEASE_NOTES.md` under `## Unreleased` with every moved budget; the learnings doc gains one line under Concurrency & shared devices: the open is bounded too |

## Project Structure

### Documentation (this feature)

```text
specs/165-modem-open-bounded/
├── spec.md
├── plan.md              # this file
├── research.md          # F1–F5, D1–D9
├── data-model.md        # the outstanding-open map and its transitions
├── quickstart.md        # tests, deploy, live router drop, kernel stack
├── checklists/requirements.md
└── tasks.md             # /speckit-tasks
```

No `contracts/`: nothing is exposed to other systems. Log messages are listed in data-model.md and quickstart.md.

### Source Code

```text
gsm-sip-bridge/src/
├── modules/at_commander.rs   # open_session_with, open_bound, DEFAULT_OPEN_BOUND, outstanding-open map, try_reopen early return, tests
├── volte/sms.rs              # PER_MESSAGE_PORT_BUDGET, SWEEP_PASS_BUDGET, derivation tests
└── ims/agent/watchdog.rs     # Phase::SmsSweep budget, renewal/startup derivations
docs/operations.md, docs/observability.md, docs/greptile-review-learnings.md, RELEASE_NOTES.md
```

## Complexity Tracking

No constitution violations.
