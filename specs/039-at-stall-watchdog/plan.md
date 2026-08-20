# Implementation Plan: Bounded modem I/O and stalled-line detection

**Branch**: `039-at-stall-watchdog` | **Date**: 2026-08-17 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/039-at-stall-watchdog/spec.md`

## Summary

A live phone line was unreachable for 2h45m because a routine re-registration blocked
forever in `read(2)` on the modem's AT port, on the same thread that answers calls —
and every health surface reported it healthy throughout.

The fix has four independent layers, ordered so the live line is protected first:

1. **Detect and recover** a line that stops making progress (ships alone; the live line
   is already running the build whose 20s SMS sweep makes a stall far more likely).
2. **Bound** every AT operation by moving the port onto a worker thread the caller waits
   on with a deadline, so no modem fault can freeze a caller.
3. **Report honestly** — registration expiry and stalls become visible in the status
   command, the metrics and the container healthcheck.
4. **Fix renewal timing and process hygiene** — honour the registrar-granted lifetime
   (scaling the headroom with it), and stop leaking orphaned processes.

## Technical Context

**Language/Version**: Rust (edition per workspace; toolchain pinned in `rust-toolchain.toml`)
**Primary Dependencies**: existing only — `serialport` 4.9 (retained), `prometheus`, `chrono`, `tracing`. **No new dependencies.**
**Storage**: N/A (existing SQLite call/SMS store untouched)
**Testing**: `cargo test` via `make test`; real pseudo-terminals / socketpairs for AT-level tests (Constitution I)
**Target Platform**: Linux (aarch64 on Raspberry Pi in production, x86-64 in CI/dev), inside a privileged container
**Project Type**: Single Rust workspace — long-running daemon plus CLI subcommands
**Performance Goals**: not throughput-bound. Recovery latency: stall detected ≤60s for sweeps, ≤~6min worst case for renewals (SC-001, SC-002)
**Constraints**: zero `unsafe` (`tools/count-unsafe.sh` in `make lint`); no hand-rolled termios; `make lint` is workspace-wide over all targets with `-D warnings`; budgets derived, not configurable (FR-033)
**Scale/Scope**: 1–4 lines per host, one process per line; ~37 functional requirements across 5 independently deployable slices

## Constitution Check

*GATE: evaluated before Phase 0 and re-evaluated after Phase 1 design.*

| Principle | Assessment | Verdict |
|---|---|---|
| **I. Integration-First Testing** (NON-NEGOTIABLE) | AT bounding is tested over real pseudo-terminals/socketpairs — real fds with real blocking semantics — not scripted in-memory mocks. A "modem that never replies" is reproduced exactly. The watchdog is tested through its real sampling logic with an injected clock (time is the one thing we cannot wait on in a test suite). Escalation and reaper decisions are pure functions tested directly. | **PASS** |
| **II. Green-on-Commit** (NON-NEGOTIABLE) | `make format && make lint && make test` before every commit, per the repo's own pre-commit checklist. | **PASS** |
| **III. Frequent Atomic Commits** | One commit per task group; the five spec stories are independently deployable by construction, and the task list is ordered so each commit leaves the tree green. | **PASS** |
| **IV. Makefile-Driven Build** | No new entry points; uses existing `make format/lint/test`. | **PASS** |
| **V. Simplicity & Refactorability** | Two deliberate additions of machinery — a worker thread per AT port, and a watchdog thread. Both are justified in Complexity Tracking below; neither is speculative (each maps to an observed production failure). Everything else reuses existing components rather than adding layers. | **PASS with justification** |

**Reuse over new code** (Principle V in practice): the escalation ladder, Discord
alerting, netns TCP probe, staleness-driven gauge zeroing, and granted-`Expires` parsing
all already exist and are reused rather than reimplemented. See research.md R7–R10.

## Project Structure

### Documentation (this feature)

```text
specs/039-at-stall-watchdog/
├── plan.md              # This file
├── research.md          # Phase 0 — decisions, rationale, rejected alternatives
├── data-model.md        # Phase 1 — entities, states, transitions
├── quickstart.md        # Phase 1 — how to verify, including fault injection
├── contracts/           # Phase 1 — observable contracts (CLI, metrics, log markers, exit codes)
├── checklists/
│   └── requirements.md  # Spec quality checklist
└── tasks.md             # Phase 2 (/speckit-tasks)
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/
├── modules/
│   ├── at_commander.rs        # MODIFY: port moves to a worker thread; deadline, drain, resync
│   ├── at_worker.rs           # NEW: the thread that owns the port and serves commands
│   └── modem_lock.rs          # NEW: deadline-bounded modem lock (replaces bare Mutex)
├── ims/
│   ├── mod.rs                 # MODIFY: granted_expires + renewal_headroom_for; health_at
│   ├── lifecycle.rs           # MODIFY: ServiceHealth.registration_expired + blocked_reason
│   └── agent/
│       ├── mod.rs             # MODIFY: phase instrumentation, watchdog wiring, granted expiry
│       └── watchdog.rs        # NEW: Progress, Phase, budgets, sampling, trip decision
├── volte/
│   ├── registration.rs        # MODIFY: re-export granted_expires; apply headroom scaling
│   ├── carrier_agent.rs       # MODIFY: phase instrumentation (second bearer, FR-032)
│   └── sms.rs                 # MODIFY: sweep gets its own Progress; bounded modem lock
├── observability/
│   └── reporter.rs            # MODIFY: gate heartbeat on progress
├── metrics/
│   ├── mod.rs, ingest.rs, server.rs   # MODIFY: expiry + stall gauges
├── commands/
│   └── healthcheck.rs         # MODIFY: fail on expired registration / stalled agent
└── supervise/
    ├── orchestrate.rs         # MODIFY: keepalive without `timeout`; classify AT stall; reaper
    ├── sim_recovery.rs        # MODIFY: has_at_stall; AtStall outcome; slow-retry after give-up
    └── runner.rs              # MODIFY: owned-pid registry; http_get for healthcheck

gsm-sip-bridge/tests/          # integration tests (real ptys/socketpairs)
```

**Structure Decision**: existing single-workspace layout, unchanged. Two new modules
(`at_worker.rs`, `watchdog.rs`) and one small new module (`modem_lock.rs`); everything
else is modification of existing files. No new crate, no new dependency, no new binary.

## Phase 1 Design Summary

Full detail in `data-model.md` and `contracts/`. The load-bearing decisions:

- **`AtCommander` keeps its exact public API.** 30+ call sites across `ims`, `volte`,
  `modules`, `vowifi` and `commands` are untouched; only the internals change. This is
  what makes a change with this blast radius reviewable.
- **A stall is recovered by process exit**, because a thread blocked in `read(2)` cannot
  be cancelled and holds the port. One process per line bounds the blast radius.
- **Abandoned channels escalate cheaply first**: resync → reopen → recover (R4), so a
  merely-slow modem never costs a restart.
- **Budgets are derived and test-pinned**, never configured (FR-033).
- **Recovery is deferrable but not indefinitely**: deferred during a call, forced past a
  ceiling (FR-029), because a stalled loop cannot observe the call ending.

## Complexity Tracking

> Constitution V requires written justification for added machinery.

| Violation | Why Needed | Simpler Alternative Rejected Because |
|---|---|---|
| A worker thread + channel per open AT port | A blocking `read(2)` on a tty cannot be cancelled or bounded from the calling thread in safe Rust; the only way to give a caller a deadline is to put the blocking call somewhere else | Trusting `serialport`'s timeout is what caused the outage (R1). Driving the fd directly needs `unsafe`, forbidden by `make lint`. Replacing `serialport` with `rustix` is a larger, riskier change the owner explicitly declined |
| A watchdog thread per agent | Every in-process signal is produced by threads that keep running while the loop is wedged, so nothing inside the process can notice; and the supervisor only sees process exit | An external prober would need a new IPC surface and would still not distinguish "stalled" from "busy". Gating the existing heartbeat on progress (R8) reuses tested machinery, but still needs *something* tracking progress — that is the watchdog |
| An owned-pid registry in `RealCommandRunner` | PID 1 must reap orphans without stealing exit statuses the supervisor's `Child` handles depend on (R10) | A bare `waitpid(-1, WNOHANG)` loop is simpler but actively breaks `is_alive` and `Command::output()` |

## Post-Design Constitution Re-Check

Re-evaluated after Phase 1: **PASS**. The design adds no abstraction beyond the three
justified items above, introduces no new dependency, keeps the widest-blast-radius
component (`AtCommander`) API-compatible, and is testable through real file descriptors
rather than mocks. The five spec stories remain independently shippable, satisfying both
Principle III and FR-015.
