# Phase 0 Research: Scheduled Card Auto-Restart

**Feature**: 010-scheduled-card-restart | **Date**: 2026-05-26

The clarification phase (Q1–Q4 in `spec.md`) eliminated all functional ambiguities. The remaining items here are technology choices and integration patterns.

## Decision 1: Cron expression library

**Decision**: Use the `cron` crate (v0.12).

**Rationale**:
- Most-downloaded Rust cron parser; well-maintained; pure Rust; no `unsafe`.
- Supports `chrono::DateTime<Local>` directly via `Schedule::upcoming(Local)`, which matches our clarified requirement of **system local time** (Q2).
- Handles DST correctly via `chrono::Local` (skipped-hour and fall-back hour semantics inherited).

**Alternatives considered**:
- `saffron` — native 5-field cron, but smaller community, less recent activity.
- `croner` — newer, more configurable, but version 2.x adds API churn and we don't need extended syntax.
- Hand-rolled parser — rejected on Principle V grounds (cron edge cases like `*/5`, `MON-FRI`, `1-5,10` are easy to get wrong; not worth re-implementing).

**Interface**: User writes a standard 5-field expression in TOML, e.g. `cron = "0 1 * * *"`. The `cron` crate uses an extended 7-field internal form (seconds, minute, hour, dom, month, dow, year). We translate by prepending `"0 "` (seconds = 0) and appending `" *"` (year = any) before parsing. This translation is in `scheduler::parse_cron_5field`.

**Edge case behavior**:
- Invalid syntax → `Schedule::from_str` returns `Err` → we log a config error and disable the scheduler (FR-004 / FR-012).
- No upcoming occurrence (e.g., `0 25 * * *` — impossible hour) → `upcoming(Local).next()` returns `None` → we log and disable.
- DST "spring forward" — `upcoming` naturally skips the missing hour.
- DST "fall back" — `upcoming` returns each wall-clock time once.

## Decision 2: Randomness source for jitter

**Decision**: Use the `rand` crate (v0.8) with `rand::thread_rng()` and `Rng::gen_range`.

**Rationale**:
- Standard, widely-used; already transitively present in our dependency tree (via several crates per `Cargo.lock`). Promoting it to a direct dep is trivial.
- Cryptographic-quality randomness is not required — uniform distribution over a small integer range is sufficient.
- `thread_rng()` keeps the scheduler stateless (no `StdRng` field to thread through).

**Test seeding**: For deterministic tests, we use `StdRng::seed_from_u64(seed)` in the test code only; production code uses `thread_rng()`. The jitter calculation is factored into a pure function `jitter_offset(rng, max_seconds) -> i64` so it's trivial to unit-test with a seeded RNG.

**Alternatives considered**:
- `fastrand` — smaller, no_std-friendly. Equally fine, but `rand` is already in the tree and is the more idiomatic choice.
- Time-based pseudo-randomness (`Instant::now().subsec_nanos()`) — rejected: poor distribution, hard to test.

## Decision 3: Where the scheduler lives (architectural integration)

**Decision**: Embed scheduler state inside `CardPool` and drive it from the existing `tokio::select!` event loop in `CardPool::run`. No new task, no new channel.

**Rationale**:
- The pool's event loop already arbitrates between worker exits, retries, control commands, and the periodic USB rescan. Adding "scheduled-restart cycle progression" as one more case in `select!` is the smallest possible change.
- All shared state (slot lifecycle, active-call tracking) lives inside the loop's local `HashMap<u32, SlotState>`. A separate task would need a `Mutex` or message channel to access this safely — strictly more complexity (Principle V violation).
- Manual-restart concurrency (FR-014a / Q4) is naturally implemented because the same loop sees both the scheduler tick and the manual `ControlCmd::CardRestart`.

**How it integrates**:
- New field `CardPool::cycle: Option<CycleState>` for the in-progress cycle (None when idle).
- New field `CardPool::next_scheduled_at: Option<tokio::time::Instant>` for the next scheduled cycle start (None when disabled or until next cron tick is computed).
- The `select!` deadline arithmetic adds `next_scheduled_at` and `cycle.next_action_at` (if present) into the existing `earliest_wakeup` computation.
- On the sleep-tick branch, after handling retries and USB rescan, we call `tick_scheduler(&mut self, now)`, which is a pure mutator over scheduler state.

**Alternatives considered**:
- Standalone tokio task with an `mpsc` channel into the pool — rejected per Principle V (added a channel that's never used outside this feature).
- A separate per-cycle task spawned on each tick — rejected: harder to coordinate manual-restart concurrency and shutdown.

## Decision 4: Detecting "active SIP call" on a slot

**Decision**: Track active-call status per slot inside `CardPool` by listening to the existing `BridgeEvent::Ring` and `BridgeEvent::Hangup` events.

**Rationale**:
- The worker thread (`run_module_loop`) already emits `Ring` when answering and `Hangup` when the GSM side disconnects. The pool already handles these in `handle_bridge_event` for SIP bridging.
- Adding a `has_active_call: bool` flag on `SlotState` and toggling it in `handle_bridge_event` is two lines of code and exposes the signal to the scheduler trivially.
- No new AT command query, no new channel.

**Alternatives considered**:
- Query the worker for its `CardState` via a new `ModuleCmd::QueryState` round-trip — rejected: adds a synchronous request/response on every scheduled card's turn (≥1 round-trip * 8 cards * every cycle); unnecessarily noisy.
- Inspect `pjsua_safe::is_sip_peer_disconnected()` — rejected: that returns SIP-side state only; can be stale during teardown; not slot-keyed.

## Decision 5: Per-card restart success detection

**Decision**: After issuing `ModuleCmd::Reboot` to a slot, the scheduler waits until that slot's `lifecycle` transitions back to `Ready` (or to `GivenUp`, or until a timeout) before advancing to the next card.

**Rationale**:
- The existing pool already drives the recovery loop: when a worker exits (because the reboot AT command was sent), `tasks.join_next()` returns; pool sets `lifecycle = Recovering` and a `next_retry_at` deadline ~10 s out; the retry-tick eventually calls `try_init_module` which succeeds and transitions back to `Ready` with a fresh worker.
- The scheduler simply polls `slots[&slot].lifecycle` on each event-loop wakeup (which already happens frequently because of the retry deadlines). No new signaling needed.
- We add a per-card timeout of **60 seconds** (configurable internally; not user-tunable in v1 — keeps schema simple; can be promoted later if needed). If a slot has not reached `Ready` within 60 s of the reboot, the scheduler records the slot as `failed` (timeout) and advances.

**Why 60 s and not a smaller number**: The existing reboot retry path waits 10 s before first retry; first `try_init_module` itself takes ~5–10 s (AT probe + IMEI + phone-number + network-type queries plus stored-mode application). So a healthy reboot+reinit is ~15–20 s; 60 s gives comfortable headroom for slow modems without dragging out the cycle.

**Alternatives considered**:
- Block on a `oneshot` from the worker — rejected: requires plumbing through the worker; the existing reboot path doesn't reply to the issuer.
- Use a shorter timeout and rely on the user's max-cycle expectation — rejected: too brittle; spurious failures during normal slow re-init would be misleading.

## Decision 6: Cycle-state structure

**Decision**: `CycleState` holds the cycle identifier, scheduled tick time, actual jittered start time, the **initial queue** of slots to process in order, a **deferred queue** for slots that had an active call on first attempt, per-card outcomes, the currently in-flight slot (if any), a `current_card_deadline` (per-card 60 s timeout), and a `next_action_at` deadline for the gap between cards. See `data-model.md` for the exact field list.

**Rationale**: Concrete state up-front prevents the cycle-state machine from drifting. Each loop iteration that enters `tick_scheduler` runs at most one transition: start-current-card, complete-current-card, advance-to-next, complete-cycle. Easy to test as a pure function.

## Decision 7: Cycle identifier scheme

**Decision**: 64-bit `u64`, generated as the Unix-second timestamp of the cycle's cron-tick time. Two cycles within the same second are not possible (we drop overlapping triggers per FR-014). The identifier appears in every structured log entry for the cycle to enable grep/filter.

**Alternatives considered**:
- UUIDv4 — overkill; not needed for log correlation across processes.
- Monotonic counter — fine, but tying it to the cron tick makes it self-describing in logs.

## Decision 8: Metrics shape

**Decision**: One new counter:

```text
gsm_sip_bridge_scheduled_restart_total{slot, outcome}
```

with `outcome ∈ {success, failed, deferred-recovered, skipped-non-ready, skipped-active-call, skipped-already-restarted-by-manual, timed-out}`.

**Rationale**:
- One counter, one set of labels — matches the existing minimalist metrics design in `metrics/mod.rs`.
- The `outcome` label cardinality is small (≤7 values × ≤8 slots = ≤56 time series), well within Prometheus best practices.

**Alternatives considered**:
- Separate counters per outcome (`scheduled_restart_success_total`, `scheduled_restart_failed_total`, …) — rejected: cardinality is fine; one counter with a label is simpler and standard.
- A histogram of per-card restart duration — out of scope for v1; can be added in a follow-up without breaking compatibility.

## Decision 9: Behavior on zero cards present

**Decision**: If the slots map is empty at the moment a cycle starts, the cycle immediately emits a cycle-start log, a cycle-complete log with zero entries, and the metrics counter is not incremented (no slot label to attach).

**Rationale**: Pure no-op semantics; no error. Operators benefit from seeing the cycle ran (next scheduled tick is logged in the cycle-complete summary).

## Decision 10: Cron evaluation cadence

**Decision**: On startup and after every cycle-complete (or cycle-skipped) event, compute the *next* upcoming occurrence via `Schedule::upcoming(Local).next()` and add the start jitter to it. Store that as `next_scheduled_at` (an `Instant`). The event loop's `sleep_until` picks it up automatically.

**Rationale**: Compute once, sleep until that time, then repeat. No polling, no per-second wakeup.

**System time changes** (NTP correction, manual `date` change):
- A small adjustment (±60 s, per FR-005) is absorbed by `sleep_until` semantics — `Instant` is monotonic so it's robust to wall-clock jumps.
- A large backward jump that *re-creates* an already-fired tick: guarded by tracking `last_fired_tick: Option<DateTime<Local>>` and comparing to the next-upcoming candidate. If `next_upcoming <= last_fired_tick`, advance to the *next* one after `last_fired_tick`.
- A large forward jump: the next-upcoming check naturally fires once at the next valid time, not repeatedly.
