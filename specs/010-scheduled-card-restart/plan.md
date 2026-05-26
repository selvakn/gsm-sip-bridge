# Implementation Plan: Scheduled Card Auto-Restart

**Branch**: `010-scheduled-card-restart` | **Date**: 2026-05-26 | **Spec**: [./spec.md](./spec.md)
**Input**: Feature specification from `specs/010-scheduled-card-restart/spec.md`

## Summary

Add a nightly (cron-driven) preventive-restart scheduler to the running `gsm-sip-bridge` daemon. At a configurable cron tick (default `0 1 * * *` — 1 AM local time, ±10 minute start jitter), the daemon iterates each known slot in ascending order, restarts it via the existing manual-restart code path (`ModuleCmd::Reboot` → `AT+CFUN=1,1` → re-init), waits a randomized inter-card gap (default 30 s ± 15 s) between cards, and emits structured logs + Prometheus counters for the cycle. Cards with an active SIP call are deferred to the end of the cycle's queue (per clarification); cards in non-ready states are skipped. A manual `card restart` issued mid-cycle is honored immediately and the scheduler records that slot as `already-restarted-by-manual`. Implementation is additive to the existing Rust workspace — no new processes, no new binary, no schema change.

## Technical Context

**Language/Version**: Rust stable (pinned by `rust-toolchain.toml`); same MSRV as v5.x.

**Primary Dependencies** (existing crates reused; only two new):
- `cron = "0.12"` — **NEW** — standard cron-expression parser/evaluator. Used to compute next-occurrence timestamps from a user-supplied 5-field expression (we translate to its 7-field internal form).
- `rand = "0.8"` — **NEW** — uniform random jitter for cycle start and inter-card gap.
- `tokio` — existing — drives the event loop, computes `sleep_until` deadlines for scheduled cycles.
- `chrono` — existing — local-time arithmetic for cron tick computation.
- `serde` — existing — `[scheduled_restart]` TOML deserialization.
- `tracing` — existing — structured log entries (cycle-start, per-card, cycle-complete).
- `prometheus` — existing — new counter `gsm_sip_bridge_scheduled_restart_total{slot, outcome}`.

**Storage**: None. The scheduler is stateless across process restarts (per FR-015, no catch-up). All cycle state lives in memory inside `CardPool` for the duration of a cycle.

**Testing**: `cargo test --workspace` integration tests. Unit tests for cron parsing, jitter range, and cycle-state-machine transitions. End-to-end tests use a programmable `Clock` trait (real `tokio::time::Instant` in production; `tokio::time::pause` + advance in tests) so we can simulate a full cycle in milliseconds without real wall-clock waits.

**Target Platform**: Linux (Debian/Ubuntu, x86\_64 and aarch64) — same as existing daemon.

**Project Type**: Cargo workspace binary daemon — single `gsm-sip-bridge` crate gains a new module.

**Performance Goals**:
- Cycle start fires within ±5 seconds of the computed (cron-tick + jitter) target (SC-004 indirectly).
- A complete 8-card cycle finishes within 10 minutes under normal modem response (SC-002).
- Scheduler overhead when idle: zero CPU between cycles (only a `sleep_until` future is pending).

**Constraints**:
- Zero new `unsafe` blocks in `gsm-sip-bridge` crate.
- Stay within the existing single-threaded-event-loop architecture in `CardPool::run` — no new background tasks for the scheduler; it lives in the same `tokio::select!` loop.
- Invalid cron expression at startup MUST NOT prevent the daemon from running; it disables the scheduler with a logged error (FR-004).
- No persistent state: missed cycles are not made up.

**Scale/Scope**: Up to 8 slots (inherited). At most one cycle runs at a time (FR-014). Deferred-retry queue is bounded by slot count.

## Constitution Check

*Gate: must pass before Phase 0. Re-checked after Phase 1.*

### I. Integration-First Testing — PASS
- Cycle progression tested end-to-end through the real `CardPool::run` event loop with `tokio::time::pause`. Cron parsing tested against the real `cron` crate. Jitter ranges tested with a seeded `StdRng`.
- No new mocks introduced. The existing PTY-based modem stub handles the AT command side; the existing in-memory store handles persistence (no persistence is added here).

### II. Green-on-Commit — PASS (process gate)
- Every task ends with `cargo fmt && make lint && cargo test --workspace` green before commit, per `CLAUDE.md` pre-commit checklist.

### III. Frequent Atomic Commits — PASS
- Tasks sized one-commit-each: dependency add, config parser, cron module, jitter module, cycle state machine, event-loop integration, manual-restart concurrency, metrics, integration tests, docs.

### IV. Makefile-Driven Build — PASS
- No new Makefile targets required; all operations remain under `make build`, `make test`, `make lint`, `make run`.

### V. Simplicity & Refactorability — PASS
- The scheduler is one struct (`CycleState`) + one function (`tick_scheduler`) inserted into the existing event loop. No new tasks, no new channels, no new background threads. Re-uses the existing `ModuleCmd::Reboot` path for the actual modem reset — no duplicate restart code.

## Project Structure

### Documentation (this feature)

```text
specs/010-scheduled-card-restart/
├── plan.md              ← this file
├── research.md          ← Phase 0 output
├── data-model.md        ← Phase 1 output
├── contracts/
│   └── config-schema.md ← Phase 1 output (TOML schema for [scheduled_restart])
├── quickstart.md        ← Phase 1 output
└── tasks.md             ← Phase 2 output (/speckit.tasks)
```

### Source Code Changes (all in `gsm-sip-bridge/`)

```text
gsm-sip-bridge/Cargo.toml          MODIFY — add `cron = "0.12"`, `rand = "0.8"`
gsm-sip-bridge/src/
├── config/
│   └── mod.rs                     MODIFY — add ScheduledRestartConfig + parse_scheduled_restart;
│                                            extend AppConfig; defaults match spec FR-002
├── modules/
│   ├── mod.rs (CardPool)          MODIFY — embed scheduler state in CardPool, extend the
│   │                                       tokio::select! loop with scheduler tick handling,
│   │                                       extend handle_control_cmd for manual-restart concurrency
│   ├── scheduler.rs               NEW    — CycleState, CyclePhase, CycleOutcome, cron evaluator,
│   │                                       jitter helpers, tick_scheduler() pure-function entry
│   └── card.rs                    (no change)
├── metrics/
│   └── mod.rs                     MODIFY — add SCHEDULED_RESTART_TOTAL counter (labels: slot, outcome)
└── (tests/)
    ├── test_scheduler.rs          NEW    — unit tests for cron parsing, jitter, cycle FSM
    └── test_scheduled_cycle.rs    NEW    — integration test driving CardPool with tokio::time::pause
                                            through a full cycle end-to-end
```

**Structure Decision**: Single Rust binary crate (`gsm-sip-bridge`). All scheduler logic lives in one new module (`modules/scheduler.rs`) plus surgical edits in `modules/mod.rs` (event-loop integration) and `config/mod.rs` (TOML schema). No new sub-crate, no new binary, no new background task. This matches Principle V (Simplicity) and the existing pattern established by feature 009.

## Complexity Tracking

> No Constitution violations — table omitted.
