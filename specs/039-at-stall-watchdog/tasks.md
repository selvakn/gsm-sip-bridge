---

description: "Task list for 039-at-stall-watchdog"
---

# Tasks: Bounded modem I/O and stalled-line detection

**Input**: Design documents from `/specs/039-at-stall-watchdog/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/

**Tests**: Included and non-optional — Constitution I makes integration testing
NON-NEGOTIABLE, and the spec's success criteria are stated as verifiable outcomes.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story the task serves

## Path Conventions

Single Rust workspace. Source under `gsm-sip-bridge/src/`, integration tests under
`gsm-sip-bridge/tests/`.

## Commit discipline (Constitution II & III)

Every task group ends with `make format && make lint && make test` green, then one
focused commit. No commit may leave the tree red.

---

## Phase 1: Setup

- [ ] T001 Confirm the worktree builds clean before any change: `make format && make lint && make test`. Record the baseline so later failures are attributable.

**No dependency, tooling or scaffolding changes** — this feature deliberately adds no
new crate (plan.md, Technical Context).

---

## Phase 2: Foundational (blocking prerequisites)

**Purpose**: the progress primitive every monitored activity shares. Blocks US1 only.

- [ ] T002 [US1] Create `gsm-sip-bridge/src/ims/agent/watchdog.rs` with `Phase` (u8-backed, `Copy`, explicit `as_u8`/`from_u8` to avoid cast lints) and per-phase `budget()` per data-model.md §1.
- [ ] T003 [US1] Add `Progress` to the same module: `base: Instant`, `phase: AtomicU8`, `phase_started_ms: AtomicU64`, `busy: AtomicBool`, `label`. Operations `enter`/`leave`/`set_busy`/`snapshot`, plus RAII `PhaseGuard` returning to `Idle` on drop.
- [ ] T004 [US1] Add the pure decision function `stall_verdict(snapshot, previous, now, recovery_enabled, defer_ceiling) -> StallVerdict` implementing two-sample confirmation and call deferral (data-model.md §3). No I/O, no clock access — `now` is a parameter.
- [ ] T005 [US1] Unit-test T002–T004 with an injected clock: within budget → `Healthy`; one overrun → `Suspected`; two consecutive → `Confirmed`; busy → `Deferred`; past ceiling → `Forced`; `PhaseGuard` restores `Idle` on early return and on panic.
- [ ] T006 [US1] Add the budget-derivation test (FR-033): recompute each budget from the real constants (`MODEM_OPEN_MAX_WAIT`, `DEFAULT_TIMEOUT`, APDU count, SIP Timer B, sweep open retry) and assert every budget exceeds its worst legitimate case by ≥20%. This is the guard against a future timeout bump silently arming false restarts.

**Checkpoint**: `Progress` and the verdict logic are complete and tested in isolation.

---

## Phase 3: User Story 1 — A stuck line recovers by itself (P1) 🎯 MVP

**Goal**: a stalled line is detected and restarted automatically; repeated stalls
escalate through the existing ladder; the owner is alerted once.

**Independent test**: stall the AT port on a registered line; it must return to
answering calls with no human action, leaving a greppable marker.

- [ ] T007 [US1] Add the watchdog thread (named `ims-watchdog`) in `watchdog.rs`: samples every 5s, feeds `stall_verdict`, and on `Confirmed`/`Forced` logs the marker from contracts C3 with `activity`, `phase`, `stalled_secs`, `budget_secs`, `last_at_command`, then exits `70`. On `Deferred`, log the deferral marker once per episode. Treat thread-spawn failure at startup as fatal rather than silently degrading.
- [ ] T008 [US1] Track `last_at_command` on `AtCommander` (`Mutex<Option<(String, Instant)>>`, set in `send_command`) and expose it to the watchdog. Highest-value diagnostic; its absence is why the original incident needed live forensics.
- [ ] T009 [US1] Wire `Progress` into `gsm-sip-bridge/src/ims/agent/mod.rs`: construct it at the top of `run_inner` (before `derive_plmn`, which itself opens the modem and can wedge), thread it through `DispatchParams`/`LoopState` (never as a new positional argument — those structs exist to dodge `too_many_arguments`), and enter phases around each blocking region in `on_idle_tick`, including **before** `modem_lock.lock()`.
- [ ] T010 [US1] Keep `Progress.busy` in sync with the loop's existing `busy()` so deferral (FR-029) is driven by real call state.
- [ ] T011 [US1] Give the SMS sweep its own `Progress` in `gsm-sip-bridge/src/volte/sms.rs` (`run_modem_reader`/`sweep_modem_storage`) and register it with the watchdog. Post-merge this is the most frequent AT user (every 20s) and is currently unwatched.
- [ ] T012 [US1] [P] Do the same for the VoLTE carrier agent in `gsm-sip-bridge/src/volte/carrier_agent.rs` (FR-032 — the second bearer shares the defective paths).
- [ ] T013 [US1] Add `[vowifi].watchdog_recovery_enabled` (default `true`) to config (FR-034), and ensure that when false the stall is still detected, logged and surfaced — only the exit is suppressed (FR-035).
- [ ] T014 [US1] Extend `gsm-sip-bridge/src/supervise/sim_recovery.rs`: `has_at_stall(agent_log)` matching the C3 marker, and `AgentExitOutcome::AtStall` counted against the **same** incident counter as `CsimFailure`.
- [ ] T015 [US1] Make give-up non-terminal (FR-030): after escalation exhausts its remedies, keep retrying on a slow cadence bounded by SC-011's 30 minutes, alerting once per incident (FR-031).
- [ ] T016 [US1] Classify the AT stall in `gsm-sip-bridge/src/supervise/orchestrate.rs` (~line 1591) before the `Other` fallthrough, and extend `sim_alert_transition`'s wording.
- [ ] T017 [US1] Tests: `has_at_stall` pattern; three consecutive stalls trigger a SIM reset; a stall and a CSIM failure count toward one incident; give-up alerts once then slow-retries; recovery-disabled still reports.

**Checkpoint**: US1 is independently deployable and protects the live line on its own (FR-015). Commit here.

---

## Phase 4: User Story 2 — No modem operation can hang forever (P2)

**Goal**: every AT command completes or fails within its deadline; a timeout does not
poison the channel.

**Independent test**: point at a modem that never replies; every operation errors within
its deadline and the process stays responsive.

- [ ] T018 [US2] Create `gsm-sip-bridge/src/modules/at_worker.rs`: a thread owning the real `serialport` handle, receiving `Request::{Command, Resync, Shutdown}` over a channel and replying on a per-request channel. Owns the read buffer **across** commands.
- [ ] T019 [US2] Rework `gsm-sip-bridge/src/modules/at_commander.rs` to delegate to the worker while keeping every public signature identical (contracts C6). Caller waits with `recv_timeout`; on expiry mark the channel `Suspect` and return the existing `"AT command timeout"` error so current callers and greps keep working.
- [ ] T020 [US2] Implement drain-and-resync (FR-003): when the caller has abandoned a reply, the worker discards it and drains input to quiescence before the next command; a `Suspect` channel resyncs (drain + bare `AT`) before its next command.
- [ ] T021 [US2] Implement the abandoned-channel escalation (FR-036/FR-037): resync → reopen → mark `Dead` and signal the line for recovery. A `Dead` channel fails fast rather than queueing (FR-004).
- [ ] T022 [US2] Bound the response loop overall, not just per line read, and cap accumulated lines (256) and buffer (64 KiB) so an unterminated URC flood cannot spin or grow without limit (FR-006).
- [ ] T023 [US2] Replace the unbounded `port.flush()`/`tcdrain` with a deadline-bounded write path.
- [ ] T024 [US2] Create `gsm-sip-bridge/src/modules/modem_lock.rs`: `Mutex<bool> + Condvar` with `lock_timeout` via `wait_timeout_while`, ~20s derived from the post-merge hold shape (one `open_with_retry` ≤1.8s + one or two AT commands). Apply at all six sites (`volte/sms.rs` ×3, `carrier_agent.rs`, `ims/agent/mod.rs`). Timeout ⇒ ordinary failure with backoff, not an exit (FR-005).
- [ ] T025 [US2] Rewrite the now-false doc block at `at_commander.rs:123-148` (added by `d31ae2f`) which asserts `serialport` provides locking "for free" — still true of the port itself, but the surrounding contract has changed.
- [ ] T026 [US2] Tests over a **real pseudo-terminal** (Constitution I): a modem that never replies errors within its deadline; the **desync regression** (timeout → late reply → next command returns its own reply); unterminated flood is bounded; `Dead` channel fails fast; `modem_lock` contention times out rather than blocking.

**Checkpoint**: root cause removed. Commit.

---

## Phase 5: User Story 3 — Health surfaces tell the truth (P3)

- [ ] T027 [US3] Add `registration_expired` to `ServiceHealth` in `gsm-sip-bridge/src/ims/lifecycle.rs`; include it in `can_answer()`; insert "the registration has expired" second in `blocked_reason()`'s priority chain (contracts C1).
- [ ] T028 [US3] Convert `RegistrationStatus::health()` to `health_at(now)` in `gsm-sip-bridge/src/ims/mod.rs`, computing expiry from the existing `expires_at`; keep `health()` delegating so existing call sites are untouched.
- [ ] T029 [US3] [P] Add `gsm_sip_bridge_vowifi_registration_expires_in_seconds` and `gsm_sip_bridge_agent_dispatch_stall_seconds` across `metrics/{mod,ingest,server}.rs`, reporting the **absolute** expiry on the wire and computing the countdown at scrape time.
- [ ] T030 [US3] Gate the heartbeat in `gsm-sip-bridge/src/observability/reporter.rs` on `Progress`: skip the enqueue while stalled, so report age goes stale and the existing staleness path zeroes the VoWiFi gauges. Extract `should_heartbeat(progress, now)` as a pure, testable function.
- [ ] T031 [US3] Extend `gsm-sip-bridge/src/commands/healthcheck.rs`: upgrade `metrics_endpoint_ok` from a bare TCP connect to a real `GET /metrics` via a new `CommandRunner::http_get`, and add `RegistrationExpired` / `AgentStalled` faults. Keep `evaluate` pure over the body string. Land the `MockCommandRunner` impl in the same commit — `-D warnings` makes a missing one a hard failure.
- [ ] T032 [US3] [P] Render `expires_in: -8412s (LAPSED)` in `vowifi-status`/`volte-status` rather than a raw timestamp.
- [ ] T033 [US3] Tests: expiry outranks `gm_connection_up` in `blocked_reason`; `expires_at: None` is never expired; a canned `/metrics` body drives the healthcheck table-driven cases; `should_heartbeat` is false while stalled.

**Checkpoint**: an expired or stalled line can no longer look healthy. Commit.

---

## Phase 6: User Story 4 — Renewal timing follows the network (P4)

- [ ] T034 [US4] Move `granted_expires` from `volte/registration.rs:57` to `ims/mod.rs` beside `renewal_due`, and re-export from `volte::registration` so its existing tests and call site are untouched (`ims` must not depend on `volte`).
- [ ] T035 [US4] Add `renewal_headroom_for(granted, preferred) = preferred.min(granted / 2)` in `ims/mod.rs` (FR-024) — without this, honouring a short grant makes `renewal_due` permanently true.
- [ ] T036 [US4] Apply at the two `DEFAULT_EXPIRES` sites in `ims/agent/mod.rs` (initial status; post-renewal status), storing the granted value on `LoopState` so the `renewal_due` call uses the scaled headroom. Apply the same scaling in `volte/registration.rs`.
- [ ] T037 [US4] Tests: a 600s grant renews at 300s not 3300s; a 120s grant uses a 60s headroom and does not fire every poll; a missing `Expires` still uses 3600 and behaviour is unchanged.

**Checkpoint**: renewal is correct for any granted lifetime. Commit.

---

## Phase 7: User Story 5 — Stop leaking processes (P5)

- [ ] T038 [US5] Replace the keepalive body at `supervise/orchestrate.rs:1655-1671` with the existing `runner.tcp_connect_ok_in_netns` — removes the `bash`/`timeout` grandchild that orphaned ~120 processes/hour (FR-028).
- [ ] T039 [US5] Add an owned-pid registry to `RealCommandRunner` covering transient `run`/`run_in_netns` children, plus `owns_pid`.
- [ ] T040 [US5] Add the PID-1 reaper (gated on `std::process::id() == 1`, 1s interval): peek with `WNOWAIT`, back off if the head of the queue is owned, otherwise claim it — so it never steals an exit status from a `Child` handle (FR-027).
- [ ] T041 [US5] Tests: pure `should_claim(pid, owned)`; extend `assert_runner_conformance` with an `owns_pid` invariant so mock and real cannot drift; assert the keepalive issues no `bash`-bearing `run_in_netns` calls.

**Checkpoint**: process count flat. Commit.

---

## Phase 8: Polish & cross-cutting

- [ ] T042 Update `docs/operations.md` with the stall marker, the exit code, the new metrics, and how to preserve a wedged line for diagnosis.
- [ ] T043 Add the release-notes entry for 039.
- [ ] T044 Full gate: `make format && make lint && make test` plus `bash tools/count-unsafe.sh` (must be zero — the entire reason for the worker-thread design over driving the fd directly).
- [ ] T045 Hardware validation per quickstart.md: exercise every AT consumer, then fault-inject and confirm detect → exit 70 → restart → re-register, and that `docker ps` shows `unhealthy` while expired.

---

## Dependencies

- **Phase 2 (T002–T006)** blocks US1 only.
- **US1 (P1)** is independently shippable and is the priority: the live line is already
  running the build whose 20s sweep makes a stall likely (FR-015).
- **US2 (P2)** depends on nothing in US1, but T008 (`last_at_command`) touches
  `at_commander.rs`, so land US1 first to keep that file's US2 rewrite conflict-free.
- **US3, US4, US5** are mutually independent and independent of US1/US2, except T030,
  which consumes `Progress` from Phase 2.
- **Phase 8** last.

## Parallel opportunities

- T012 with T011 (different files).
- T029 and T032 with the rest of US3.
- US4 and US5 can proceed alongside US2 entirely — different files, no shared state.

## Implementation strategy

Ship US1 first and deploy it: it converts the outage class from hours to seconds for
*every* cause, known or not. Then US2 removes the root cause, and US3 ensures the next
unknown failure is diagnosable in one command instead of via kernel stacks.
