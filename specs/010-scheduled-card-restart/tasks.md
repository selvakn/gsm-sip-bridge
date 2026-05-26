---

description: "Task list for feature 010 — Scheduled Card Auto-Restart"
---

# Tasks: Scheduled Card Auto-Restart

**Input**: Design documents in `/specs/010-scheduled-card-restart/`
**Prerequisites**: `plan.md`, `spec.md`, `research.md`, `data-model.md`, `contracts/config-schema.md`, `quickstart.md`

**Tests**: REQUIRED per Constitution Principle I (Integration-First Testing). Tests are interleaved with implementation tasks below.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies on each other)
- **[Story]**: `US1` / `US2` / `US3` map to the three user stories in `spec.md`; `FND` = foundational; `EDGE` = edge cases & clarifications; `POL` = polish
- All paths are repo-root-relative

---

## Phase 1: Setup

**Purpose**: One-shot dependency addition. No code changes yet.

- [ ] T001 Add `cron = "0.12"` and `rand = "0.8"` to `[dependencies]` in `gsm-sip-bridge/Cargo.toml`. Run `cargo check -p gsm-sip-bridge` to confirm both crates resolve and the workspace still builds.

**Checkpoint**: Dependencies present; nothing else changed.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Shared types, config plumbing, metrics, and active-call signal — required by every user story before scheduler logic can be added.

- [ ] T002 [FND] Config: extend `gsm-sip-bridge/src/config/mod.rs`:
  - Add `"scheduled_restart"` to the `TOP_LEVEL_SECTIONS` array.
  - Add a `SCHEDULED_RESTART_KEYS` array listing every field from `contracts/config-schema.md`.
  - Add `pub struct ScheduledRestartConfig` with the five fields (`enabled`, `cron`, `start_jitter_seconds`, `inter_card_gap_seconds`, `inter_card_gap_jitter_seconds`) and a `Default` impl matching the documented defaults.
  - Add a `parse_scheduled_restart(root)` function applying the validation table in `contracts/config-schema.md`. On any invalid value, log a WARN/ERROR via `tracing` and return `ScheduledRestartConfig { enabled: false, ..Default::default() }` so the daemon continues.
  - Add `pub scheduled_restart: ScheduledRestartConfig` to `AppConfig`; wire the parser into `load_config`.
  - Add `#[cfg(test)] mod tests` cases inside this file: (a) section omitted → defaults; (b) `enabled = false` → disabled; (c) custom cron applied; (d) invalid cron → disabled; (e) jitter > gap → disabled; (f) `start_jitter_seconds` out of range → disabled.

- [ ] T003 [P] [FND] Metrics: extend `gsm-sip-bridge/src/metrics/mod.rs` with `SCHEDULED_RESTART_TOTAL: Lazy<CounterVec>` named `gsm_sip_bridge_scheduled_restart_total`, labels `&["slot", "outcome"]`, help text from `contracts/config-schema.md`. Use the same `register_counter_vec!` pattern as the existing counters.

- [ ] T004 [FND] Active-call tracking: in `gsm-sip-bridge/src/modules/mod.rs`:
  - Add `has_active_call: bool` (default `false`) to the `SlotState` struct (and initialize it to `false` in every `SlotState` constructor site).
  - In `handle_bridge_event`, on `BridgeEvent::Ring { module_id, .. }` find the slot whose `module.id == module_id` and set `has_active_call = true`. On `BridgeEvent::Hangup { module_id }` set it back to `false`.
  - Add a simple test in `gsm-sip-bridge/tests/test_card_pool.rs` (or extend existing) only if a unit-testable seam exists; otherwise this is covered by Phase-3+ integration tests.

- [ ] T005 [FND] Scheduler module skeleton: create `gsm-sip-bridge/src/modules/scheduler.rs` containing only the public types from `data-model.md` (no logic yet):
  - `pub struct CycleState`, `pub enum CyclePhase`, `pub struct CurrentCard`, `pub enum AttemptType`, `pub struct CycleOutcome`, `pub enum Outcome`, `pub enum SkipReason`.
  - Derive `Debug` and (where appropriate) `Clone` / `PartialEq` for testability.
  - Add `pub mod scheduler;` to `gsm-sip-bridge/src/modules/mod.rs`.
  - Add a smoke `#[cfg(test)] mod tests` that just constructs an empty `CycleState` to confirm the types compile.

**Checkpoint**: Workspace builds; new types reachable; no behavior change yet. `cargo test --workspace` green.

---

## Phase 3: User Story 1 — Nightly Auto-Restart of All Cards (Priority: P1) 🎯 MVP

**Goal**: At the configured cron time (default 1 AM ± 10 min jitter), iterate every known card in ascending slot order, restart each via the existing reboot path, with a 30 s ± 15 s gap between cards.

**Independent Test**: Drive a full cycle end-to-end with `tokio::time::pause` over multiple simulated cards and assert each is restarted in order, no two simultaneously, all reach Ready, cycle-complete summary correct.

- [ ] T006 [US1] Cron parsing in `gsm-sip-bridge/src/modules/scheduler.rs`:
  - `pub fn parse_cron_5field(expr: &str) -> Result<cron::Schedule, String>`: translates 5-field to 7-field (`"0 " + expr + " *"`) and parses via `cron::Schedule::from_str`. Returns the original `expr` in the error for log clarity.
  - `pub fn compute_next_scheduled_at(schedule: &cron::Schedule, after: chrono::DateTime<chrono::Local>) -> Option<chrono::DateTime<chrono::Local>>`: thin wrapper around `schedule.after(&after).next()`.
  - `#[cfg(test)] mod` covers: valid 5-field expression, invalid expression, `0 1 * * *` next-after a known timestamp lands on the expected next 1 AM, `*/5 * * * *` produces the next 5-min boundary.

- [ ] T007 [P] [US1] Jitter helpers in `gsm-sip-bridge/src/modules/scheduler.rs`:
  - `pub fn jitter_offset<R: rand::Rng>(rng: &mut R, max_seconds: u64) -> i64`: returns a uniform random integer in `[-max_seconds as i64, +max_seconds as i64]`; if `max_seconds == 0`, returns `0`.
  - `pub fn gap_with_jitter<R: rand::Rng>(rng: &mut R, base: u64, jitter: u64) -> std::time::Duration`: returns `Duration::from_secs((base as i64 + jitter_offset(rng, jitter)).max(0) as u64)`.
  - Tests: deterministic with `rand::rngs::StdRng::seed_from_u64(seed)`; verify range bounds across 1000 samples; verify `max_seconds == 0` always returns 0.

- [ ] T008 [US1] Core cycle FSM in `gsm-sip-bridge/src/modules/scheduler.rs`:
  - Define a trait `SlotView` (or just a closure parameter) that gives the FSM read-only access to per-slot lifecycle + `has_active_call`. Keep the FSM pure: it takes input snapshots, returns a list of `SchedulerAction` enum values (`SendReboot { slot }`, `RecordOutcome { slot, outcome }`, `Sleep { until }`, `Complete`).
  - `pub fn tick_scheduler(state: &mut CycleState, slot_view: &dyn SlotView, now: tokio::time::Instant, rng: &mut dyn rand::RngCore, gap_base: u64, gap_jitter: u64) -> Vec<SchedulerAction>`.
  - Implement: pop from `pending` if `current` is None and phase=Initial; transition to DeferredRetry then Complete; handle non-ready skip, active-call defer, success, timeout, GivenUp.
  - `#[cfg(test)] mod` (in-file, pure-function tests using a `MockSlotView`): single-card success, single-card non-ready skip, single-card active-call defer-then-success on retry, single-card active-call defer-then-still-active → skip, multi-card ordering, mixed outcomes, timeout path. **At least 8 distinct test cases.**

- [ ] T009 [US1] Event-loop integration in `gsm-sip-bridge/src/modules/mod.rs`:
  - Add fields to `CardPool`: `cycle: Option<CycleState>`, `next_scheduled_at: Option<tokio::time::Instant>`, `last_fired_tick: Option<chrono::DateTime<chrono::Local>>`, `cron_schedule: Option<cron::Schedule>`, `rng: rand::rngs::ThreadRng` (or `Box<dyn RngCore + Send>` if `Send` becomes an issue).
  - In `CardPool::new`, parse the cron expression via `scheduler::parse_cron_5field`. On failure, log warn and leave `cron_schedule = None` (effectively disabled). On success, compute initial `next_scheduled_at` and log the next-cycle line per `contracts/config-schema.md`. Also log the disabled message if `config.scheduled_restart.enabled == false`.
  - In `CardPool::run`'s `tokio::select!` deadline computation, fold `next_scheduled_at` and `cycle.as_ref().map(|c| c.next_action_at)` into `earliest_wakeup`.
  - On the sleep-tick branch (after the existing retry/rescan logic), call a new `self.advance_scheduler(now)` method that:
    1. If `cycle.is_none()` and `next_scheduled_at <= now` and scheduler enabled → start a new cycle (compute jittered start was already done at last computation; here we just construct `CycleState` and log cycle-start).
    2. If `cycle.is_some()` and `cycle.next_action_at <= now` → call `tick_scheduler`, apply each returned `SchedulerAction`.
    3. When phase becomes Complete → emit cycle-complete log, increment metrics, drop the cycle, recompute `next_scheduled_at`.

- [ ] T010 [US1] Apply `SchedulerAction`s in `gsm-sip-bridge/src/modules/mod.rs`:
  - `SendReboot { slot }`: look up `slots[&slot]`; if `cmd_tx` is `Some`, send `ModuleCmd::Reboot`; otherwise send the reboot directly via `AtCommander::open` like `ControlCmd::CardRestart` does today; transition lifecycle to `Recovering`, retry_count=0, next_retry_at = now+10s — same shape as the existing manual-restart code path.
  - `RecordOutcome { slot, outcome }`: append a `CycleOutcome` to `cycle.outcomes`, emit per-card-outcome log, increment `SCHEDULED_RESTART_TOTAL` with the correct `outcome` label.
  - `Sleep { until }`: set `cycle.next_action_at = until`.
  - `Complete`: set `cycle.phase = Complete` so the next loop iteration tears down the cycle and recomputes `next_scheduled_at`.

- [ ] T011 [US1] Integration test: create `gsm-sip-bridge/tests/test_scheduled_cycle.rs`:
  - Use `tokio::time::pause` + `tokio::time::advance` to compress wall-clock waits to milliseconds.
  - Construct a minimal `CardPool` with 3 cards all in `Ready` state (extending `tests/common/pty.rs` if needed to spin up fake AT serial pairs that respond OK to `AT+CFUN=1,1`).
  - Set `cron = "*/1 * * * *"` (every minute) and `start_jitter_seconds = 0` for determinism; `inter_card_gap_seconds = 0`, `inter_card_gap_jitter_seconds = 0`.
  - Advance time past the next minute boundary; assert: cycle-start observed, slot 0 reboot first, then slot 1, then slot 2 (in order); each reaches Ready; cycle-complete observed with `succeeded=3`.
  - Capture log events using `tracing-subscriber::fmt::layer().with_test_writer()` (the project already does this pattern in other tests if present; otherwise use `tracing::subscriber::with_default` plus a `Vec<u8>` writer).

**Checkpoint**: User Story 1 done — a healthy 3-card setup gets a clean nightly restart cycle end-to-end. MVP ready.

---

## Phase 4: User Story 2 — Operator Configures Schedule and Jitter (Priority: P2)

**Goal**: Configurable via `[scheduled_restart]` in `config.toml`; sensible defaults when omitted; clear errors on misconfiguration without aborting the daemon.

**Independent Test**: Edit `[scheduled_restart]` (or omit it, or break it); restart; observe correct behavior in startup logs and subsequent cycle activity.

- [ ] T012 [P] [US2] Integration test in `gsm-sip-bridge/tests/test_scheduler.rs`: defaults applied when `[scheduled_restart]` section omitted (cron == default, jitter == defaults, scheduler enabled).

- [ ] T013 [P] [US2] Integration test: `enabled = false` → scheduler initialized but no cycle ever fires within a simulated 24 h advance; startup log contains `scheduled_restart disabled`.

- [ ] T014 [P] [US2] Integration test: invalid cron `"0 25 * * *"` → daemon starts, scheduler disabled with a WARN log, no cycle fires, **rest of bridge fully functional** (SIP registration still attempted, control socket still accepts commands).

- [ ] T015 [US2] Validation polish in `gsm-sip-bridge/src/config/mod.rs`:
  - `jitter > gap` returns disabled config + ERROR log naming both fields.
  - Out-of-range `start_jitter_seconds` (e.g., 999999) returns disabled config + ERROR.
  - Unknown keys under `[scheduled_restart]` emit the existing `warn_unknown_keys_in` warning (verify by extending the unit test in T002).

**Checkpoint**: All three US2 acceptance scenarios verified.

---

## Phase 5: User Story 3 — Visibility into Scheduled Restart Activity (Priority: P3)

**Goal**: Structured logs and Prometheus counters give operators full visibility into every scheduled cycle.

**Independent Test**: After a cycle runs, log-grep produces a complete trail (one cycle-start, N per-card-outcome, one cycle-complete) and the `gsm_sip_bridge_scheduled_restart_total` counter has incremented for every processed card.

- [ ] T016 [US3] Structured logs in `gsm-sip-bridge/src/modules/mod.rs` and `scheduler.rs`:
  - cycle-start: `tracing::info!(cycle_id, cron_tick = %ct, actual_start = %now_local, n_slots, pending_slots = ?ids, "scheduled_restart cycle-start")`.
  - per-card-start: `tracing::info!(cycle_id, slot, attempt = %attempt, "scheduled_restart per-card-start")`.
  - per-card-outcome: level depends on outcome (`info` for success, `warn` for failed/timed-out, `debug` for skipped/deferred); include `duration_ms`.
  - cycle-complete: `tracing::info!(cycle_id, total, succeeded, failed, deferred_recovered, skipped, duration_ms, next_cycle_at = ?, "scheduled_restart cycle-complete")`.

- [ ] T017 [US3] Metrics emission: at every `RecordOutcome`, call `metrics::SCHEDULED_RESTART_TOTAL.with_label_values(&[&slot.to_string(), outcome_label]).inc()`. `outcome_label` strings exactly per `contracts/config-schema.md` (`success`, `failed`, `deferred-recovered`, `skipped-non-ready`, `skipped-active-call`, `skipped-already-restarted-by-manual`, `timed-out`).

- [ ] T018 [US3] Integration test in `gsm-sip-bridge/tests/test_scheduled_cycle.rs`: after the T011 end-to-end cycle, assert:
  - Logs contain exactly one `cycle-start`, exactly N `per-card-outcome`, exactly one `cycle-complete`.
  - The cycle-complete summary fields match the actual outcomes.
  - For each slot in the cycle, the metric `gsm_sip_bridge_scheduled_restart_total{slot="N",outcome="success"}` has incremented by 1. (Use `metrics::REGISTRY.gather()` to read the live counter family from inside the test.)

**Checkpoint**: Operator can fully reconstruct a cycle from logs + metrics.

---

## Phase 6: Edge Cases & Clarifications

**Purpose**: Implement and verify the four behaviors fixed in `## Clarifications` of `spec.md`, plus FR-013/014/015 edge cases.

- [ ] T019 [EDGE] Active-call deferral path (Q1): in `tick_scheduler`, when `slot_view.has_active_call(slot)` returns true during Initial phase, push the slot to `cycle.deferred` and emit `Outcome::Deferred { reason: "active call" }`. Move to next pending slot immediately (no gap wait between a defer and the next initial pop — gaps only apply after a real restart).

- [ ] T020 [EDGE] Deferred-retry phase: when pending is empty and deferred is non-empty, transition `phase = DeferredRetry` and process `deferred` the same way, except on still-active-call → `Outcome::Skipped { reason: SkipReason::ActiveCall }` (terminal for this cycle). Integration test exercising the full defer→succeed-on-retry path and the defer→still-active-→skipped path.

- [ ] T021 [EDGE] Manual-restart concurrency (Q4): extend `handle_control_cmd`'s `ControlCmd::CardRestart` arm:
  - If `self.cycle.is_some()`:
    - If `cycle.current.map(|c| c.slot) == Some(slot)` → reply `Err("slot N is currently being restarted by the scheduled cycle (cycle id=ID)")`; return early without rebooting.
    - Else if `cycle.pending.contains(slot) || cycle.deferred.contains(slot)` → remove from queues, append a `CycleOutcome { outcome: AlreadyRestartedByManual }` to `cycle.outcomes`, then fall through to the existing manual-restart code.
    - Else (slot already processed) → fall through to existing manual-restart code.
  - Integration test covering all three sub-cases (pending → marked, current → rejected, processed → proceeds).

- [ ] T022 [EDGE] Cycle-overlap protection (FR-014): in `advance_scheduler`, if a cron tick fires while `self.cycle.is_some()`, emit a WARN log `scheduled_restart cycle-trigger-dropped previous_cycle_id=X new_tick=…` and update `last_fired_tick` so the dropped tick is not re-evaluated. Integration test: force two very close ticks (e.g., `*/1` cron with `tokio::time::advance`) while the first cycle is still in flight; assert second is dropped.

- [ ] T023 [EDGE] Per-card 60 s timeout: when `cycle.current.deadline <= now` and the slot is not yet Ready/GivenUp, record `Outcome::TimedOut` and advance. Integration test using a fake slot that never returns to Ready (the test-side worker holds its lifecycle in Recovering indefinitely); assert outcome is `timed-out` after exactly the configured timeout window.

- [ ] T024 [EDGE] No-catch-up on startup (FR-015): integration test that starts the daemon at simulated local-time 03:00 with `cron = "0 1 * * *"`, advances time forward, and asserts that no cycle fires until the *next* 01:00 — not immediately on startup. Implemented automatically because we use `Schedule::after(now)` which naturally yields the next future occurrence; the test simply locks this behavior in.

**Checkpoint**: All Edge Cases section bullets in `spec.md` covered by code or test.

---

## Phase 7: Polish

- [ ] T025 [P] [POL] Update `config.toml.example`: add a commented `[scheduled_restart]` section showing each field with its default value and a one-line comment, matching `contracts/config-schema.md`.

- [ ] T026 [P] [POL] Update `README.md`: add a short paragraph under the existing "Features" / "Operations" section linking to `specs/010-scheduled-card-restart/quickstart.md`. Do not duplicate content from quickstart.

- [ ] T027 [POL] Final guardrails: run the pre-commit checklist sequence from `CLAUDE.md`:
  - `cargo fmt --all`
  - `make lint` (rustfmt check + clippy `-D warnings` + cargo-deny + unsafe ratio)
  - `cargo test --workspace`
  - All three must succeed before considering the feature done.

---

## Dependencies & Execution Order

### Phase ordering

- **Phase 1 (Setup)** — must complete before anything else.
- **Phase 2 (Foundational)** — must complete before Phase 3. T003 [P] runs in parallel with T002/T004/T005 (different file).
- **Phase 3 (US1)** — T006 and T007 [P] are independent of each other but both block T008. T008 blocks T009. T009 blocks T010. T010 blocks T011.
- **Phase 4 (US2)** — T012/T013/T014 [P] (independent test files / different scenarios). T015 sequential after T002.
- **Phase 5 (US3)** — T016, T017 sequential (both edit scheduler/CardPool integration); T018 after both.
- **Phase 6 (Edge)** — T019 → T020 (defer logic). T021–T024 can run in any order after Phase 3 is done.
- **Phase 7 (Polish)** — depends on Phases 3–6 completing.

### Parallel Opportunities

- T003 ‖ {T002, T004, T005} (different file: `metrics/mod.rs`).
- T006 ‖ T007 (different functions in the same new file, but independent — can be authored together or split).
- All Phase-4 [P] tests are independent scenarios.
- T025 ‖ T026.

### Within Each User Story

- Pure-function tests come *with* the implementation in the same task (in-file `#[cfg(test)] mod tests`) per existing project pattern.
- Integration tests follow implementation in the next task.

---

## Implementation Strategy

### MVP Path (US1 only)

T001 → T002 → T003 → T004 → T005 → T006 → T007 → T008 → T009 → T010 → T011. After T011, the feature is a working MVP.

### Full Feature

Continue with Phase 4 → Phase 5 → Phase 6 → Phase 7. Each phase is independently mergeable (Constitution Principle III).

### Verification Per Phase

- Phase 2: `cargo build -p gsm-sip-bridge && cargo test -p gsm-sip-bridge` green.
- Phase 3: `cargo test --test test_scheduled_cycle` green.
- Phase 4: `cargo test --test test_scheduler` green (file added in this phase).
- Phase 5: same test files cover metrics + log assertions.
- Phase 6: all `EDGE` task tests green.
- Phase 7: `make lint && cargo test --workspace` green.

---

## Notes

- [P] tasks touch different files or independent scenarios — safe to run in parallel.
- All tests are integration-style or in-file pure-function tests; no new mocks (Constitution Principle I).
- Per CLAUDE.md, commit only after each task's tests pass.
- Avoid creating a new background tokio task for the scheduler — fold it into `CardPool::run`'s existing `select!` (per `research.md` Decision 3).
- The `cron` crate's API uses a 7-field internal expression; `parse_cron_5field` is the only place that does the 5→7 translation. All other code uses `cron::Schedule` directly.
