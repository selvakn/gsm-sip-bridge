# Data Model: Scheduled Card Auto-Restart

**Feature**: 010-scheduled-card-restart | **Date**: 2026-05-26

This feature introduces **no persistent storage** changes. All state is in-memory inside `CardPool`, and the cycle's logical artifacts are emitted exclusively as structured log entries and Prometheus counter increments.

## Configuration: `ScheduledRestartConfig`

Loaded once at startup from the `[scheduled_restart]` TOML section. Held inside `AppConfig`.

```rust
pub struct ScheduledRestartConfig {
    pub enabled: bool,                      // default: true
    pub cron: String,                       // default: "0 1 * * *"
    pub start_jitter_seconds: u64,          // default: 600
    pub inter_card_gap_seconds: u64,        // default: 30
    pub inter_card_gap_jitter_seconds: u64, // default: 15
}
```

**Validation rules** (enforced in `config::parse_scheduled_restart`):
- `cron` must parse via `cron::Schedule::from_str` after translating from 5-field to 7-field form (`"0 " + cron + " *"`). Invalid → log config error and return a disabled config (`enabled = false`); do not abort startup (FR-004).
- `start_jitter_seconds` ∈ `0..=86400` (cap at one day to prevent absurd values).
- `inter_card_gap_seconds` ∈ `0..=3600`.
- `inter_card_gap_jitter_seconds` ∈ `0..=3600` and MUST be ≤ `inter_card_gap_seconds` (so the gap never goes negative).

## In-Memory Cycle State: `CycleState`

Constructed at each cycle-start moment; dropped at cycle-end. Lives inside `CardPool` as `Option<CycleState>`.

```rust
pub struct CycleState {
    pub id: u64,                                   // Unix-second timestamp of cron tick
    pub cron_tick: chrono::DateTime<chrono::Local>,
    pub started_at: tokio::time::Instant,          // actual jittered start time
    pub phase: CyclePhase,
    pub pending: std::collections::VecDeque<u32>,  // initial-pass queue (ascending slot order)
    pub deferred: std::collections::VecDeque<u32>, // deferred-due-to-active-call queue
    pub current: Option<CurrentCard>,              // slot currently being restarted
    pub next_action_at: tokio::time::Instant,      // when tick_scheduler should next act
    pub outcomes: Vec<CycleOutcome>,               // record of every attempt in this cycle
}

pub enum CyclePhase {
    Initial,         // working through `pending`
    DeferredRetry,   // working through `deferred`
    Complete,        // cycle finished; awaiting cleanup by event loop
}

pub struct CurrentCard {
    pub slot: u32,
    pub attempt: AttemptType,  // Initial or DeferredRetry
    pub started_at: tokio::time::Instant,
    pub deadline: tokio::time::Instant,  // started_at + per-card timeout (60 s)
}

pub enum AttemptType { Initial, DeferredRetry }

pub struct CycleOutcome {
    pub slot: u32,
    pub attempt: AttemptType,
    pub outcome: Outcome,
    pub duration: std::time::Duration,
}

pub enum Outcome {
    Success,
    Failed { reason: String },
    Deferred { reason: String },                 // recorded on first attempt; later retried
    Skipped { reason: SkipReason },              // terminal for that slot in this cycle
    TimedOut,
    AlreadyRestartedByManual,                    // operator pre-empted via CLI
}

pub enum SkipReason {
    NonReady(String),                            // string contains the lifecycle state
    ActiveCall,                                  // only used after deferred-retry also has active call
    SlotDisappeared,                             // slot removed mid-cycle (hot-unplug)
}
```

## State Transitions: `CycleState` Machine

The scheduler is driven by `tick_scheduler(&mut CardPool, now: Instant)`, called from the existing event loop whenever `now >= cycle.next_action_at` (or whenever the cycle is created).

```text
[no cycle]
   |
   |  now >= next_scheduled_at && !overlap
   v
[Initial]----------------------------------------+
   |                                              |
   | current=None, pending=non-empty              |
   |                                              |
   |  pop next slot N                             |
   |    | check slot N state:                     |
   |    |   non-ready  → record Skipped(NonReady), advance immediately
   |    |   active-call → push N into deferred, record Outcome::Deferred, advance immediately
   |    |   ready+idle → set current=N, send ModuleCmd::Reboot, set deadline=now+60s
   |                                              |
   | current=Some(N)                              |
   |   wait until slot N is Ready (success)       |
   |     OR GivenUp (failure)                     |
   |     OR deadline reached (timeout)            |
   |   → record outcome, clear current,           |
   |     set next_action_at = now + gap+jitter    |
   |                                              |
   | current=None, pending=empty                  |
   |   if deferred=empty → phase=Complete         |
   |   else              → phase=DeferredRetry    |
   v                                              |
[DeferredRetry]                                   |
   |                                              |
   | same per-card flow as Initial, but           |
   |   on active-call this time → Skip(ActiveCall)|
   | deferred=empty → phase=Complete              |
   v                                              |
[Complete]                                        |
   | emit cycle-complete log + metrics            |
   | drop CycleState; recompute next_scheduled_at |
   +-----------------------------------------> [no cycle]
```

## Slot-Side State (additions to existing `SlotState`)

The existing `modules::SlotState` struct (in `gsm-sip-bridge/src/modules/mod.rs`) gains one field:

```rust
pub struct SlotState {
    // ... existing fields ...
    pub has_active_call: bool,   // NEW — set on BridgeEvent::Ring, cleared on Hangup
}
```

Mutations:
- `CardPool::handle_bridge_event(BridgeEvent::Ring { module_id, .. })` → find slot by module_id, set `has_active_call = true`.
- `CardPool::handle_bridge_event(BridgeEvent::Hangup { module_id })` → find slot, set `has_active_call = false`.

## Manual-Restart Concurrency: `cycle.manual_skip_set`

When `ControlCmd::CardRestart { slot }` arrives during an active cycle:

| Cycle state for slot | Action |
|----------------------|--------|
| `slot` is in `pending` or `deferred` | Remove it; record `CycleOutcome { outcome: AlreadyRestartedByManual }`; proceed with the normal manual-restart path. |
| `slot == current.slot` | Reject the manual command with error `"slot N is currently being restarted by the scheduled cycle (cycle id={id})"`; CLI exits non-zero. |
| `slot` already in `outcomes` (already processed) | Proceed normally — manual restart runs as if no cycle existed. |
| No cycle active | Proceed normally (existing FR-013 flow). |

## Time Anchors

Two distinct time domains:
- `tokio::time::Instant` (monotonic) — used for all deadlines (cycle start, per-card timeout, inter-card gap). Robust to wall-clock jumps.
- `chrono::DateTime<chrono::Local>` — used only for computing the next cron occurrence and for logging the human-readable cron-tick time.

Conversion happens once per cycle: when we determine the next cron-tick `DateTime<Local>`, we compute `delta = tick - Local::now()`, then `next_scheduled_at = Instant::now() + delta`.

## Persistence

**None.** Cycle state and scheduler state are entirely in-memory. A process restart resets everything; the next cycle fires on the next future cron occurrence (FR-015).

## Concurrency

- Only one cycle may be active at a time (`Option<CycleState>` enforces this at the type level).
- All cycle mutations happen inside the `CardPool::run` async loop on a single tokio task — no `Mutex`, no `Arc`.
- The scheduler interacts with module workers exclusively through the existing `cmd_tx: crossbeam_channel::Sender<ModuleCmd>` per-slot channel — same path the manual `card restart` already uses.
