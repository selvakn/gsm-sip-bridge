# Data Model: A Card-Reader Line Rides Out an Unreachable SIM

All state is in-process and lives as long as the agent. Nothing is persisted. Paths are relative to `gsm-sip-bridge/src`.

## `BridgeError::CardUnreachable(String)` (`error.rs`)

| Aspect | Value |
|---|---|
| Display | `IMS error: SIM card unreachable: {0}` |
| Built by | `modules::pcsc_card::pcsc_failure(context, e)` for the mapped `pcsc::Error` variants (research D1), and the two final `PcscTransport::connect` errors |
| Recognised by | `BridgeError::is_card_unreachable()`. Nothing matches its text. |
| Never built for | modem transports (`AtCommander`), the card's own answers (`usim.rs`), config errors in `open_card` |

## `CardTiming` (`ims/registration_auth.rs`)

| Field | Production value | Used for |
|---|---|---|
| `retry_window` | `CARD_RETRY_WINDOW` = 30 s | how long the worker keeps putting the same challenge to an unreachable card |
| `retry_interval` | `CARD_RETRY_INTERVAL` = 2 s | the pause between attempts |
| `attempt_bound` | `CARD_ATTEMPT_BOUND` = 15 s | the loop's slack past the window before abandoning the worker; also the bound on one local check |
| `check_initial` | `RETRY_INITIAL_BACKOFF` = 5 s | the first local check after entering the waiting state |
| `check_max` | `CARD_CHECK_MAX_BACKOFF` = 60 s | the cap on the check backoff (step: `next_backoff`, ×2) |

It is held in `ReauthEnv.timing`. Tests use `CardTiming::fast()` (milliseconds). It is not operator-configurable.

## `SimWorker<T>` (`ims/registration_auth.rs`)

A generalisation of today's `Reauthentication`: one thread that only talks to the SIM.

| Field | Meaning |
|---|---|
| `result: Receiver<BridgeResult<T>>` | the thread's single answer |
| `cancelled: Arc<AtomicBool>` | set on drop; checked between attempts; late answers are discarded |
| `done: Arc<AtomicBool>` | set by a drop guard when the thread ends, including on panic. Shared with `AwaitingCard.previous` for FR-015. |
| `deadline: Instant` | `started + WORK_TIMEOUT` (modem), or `started + retry_window + attempt_bound` (reader challenge), or `started + attempt_bound` (check) |

Methods: `poll()`, `overdue(now)`, `cancel()`, `finished_flag() -> Arc<AtomicBool>`.

- `Reauthentication = SimWorker<Answer>`, built by `Reauthentication::spawn(job, open, timing: Option<CardTiming>)`. `Some` means a card-reader line.
- `CardCheck = SimWorker<()>`, built by `CardCheck::spawn(cfg, open, bound)`.

## `AwaitingCard` (`ims/agent/registration_refresh.rs`)

`LoopState.awaiting_card: Option<AwaitingCard>`. `None` on every line until a card-reader challenge gives up. It never exists on a modem line.

| Field | Meaning |
|---|---|
| `since: Instant` | when waiting began (for `waited_secs` in the log) |
| `check: Option<CardCheck>` | the check in flight, if any |
| `next_check_at: Instant` | when the next check may start |
| `backoff: Duration` | the current check interval (`check_initial` → ×2 → `check_max`) |
| `previous: Option<Arc<AtomicBool>>` | the `done` flag of the last abandoned worker (the challenge worker, or a hung check). No check starts until it is set. |

### State machine (one card-reader line)

```text
                    401 to an in-place refresh
                                 │
                                 ▼
 ┌────────────────────── Challenged (worker retries the same challenge) ──────────────────────┐
 │ answer / AUTS            → as today (Answer / Resync REGISTER)                             │
 │ card error (not unreachable) → fall_back_to_full (as today)                               │
 │ CardUnreachable after window, or worker overdue                                            │
 │                          → AwaitingCard { previous: worker.done }   last_refresh=awaiting-card
 └────────────────────────────────────────────────────────────────────────────────────────────┘
                                 │
                                 ▼
 ┌──────────────────────────── AwaitingCard (each pass, in order) ────────────────────────────┐
 │ 1. force_renewal || refresh_fallback      → None (that path owns the line)                 │
 │ 2. !busy && (lapsed || remaining < 60 s)  → None, force_renewal = true                     │
 │ 3. check answered Ok                      → None, send one in-place REGISTER this pass     │
 │    check CardUnreachable                  → schedule next check (backoff ×2, cap 60 s)     │
 │    check other error                      → None, fall_back_to_full                        │
 │    check past its bound                   → previous = check.done, schedule next check     │
 │ 4. no check, due, previous done           → start CardCheck                                │
 └────────────────────────────────────────────────────────────────────────────────────────────┘
   Also cleared: before every full renewal (on_idle_tick), and on LoopState drop.
```

Invariants:

- `challenged.is_some()` and `awaiting_card.is_some()` are never true at once. Entering the waiting state consumes `challenged`. A challenge can only start from a REGISTER, and none is sent while waiting.
- While `awaiting_card.is_some()`, `registration_refresh` is `None` and no REGISTER goes out.
- At most one `SimWorker` that has not finished exists per line, counting abandoned ones (FR-015).

## Renewal ladder (`ims/agent/mod.rs`): what changes

| Field | Card-unreachable failure on a reader line | Every other failure |
|---|---|---|
| `next_renewal_attempt` | `now + backoff` | as today |
| `backoff` | `next_backoff(backoff, RETRY_MAX_BACKOFF)` | as today |
| `consecutive_renewal_failures` | **unchanged** | `+1`, `Err` at 6 (as today) |
| P-CSCF pool marks | **none** | as today (`on_renewal_failure`) |
| status `state` / `last_failure` / `set_registered(false)` | as today (`last_failure` text carries `SIM card unreachable`) | as today |

## Status (`LoopState.last_refresh`)

New value: `awaiting-card`, set when the waiting state is entered. Existing values: `ok`, `rejected <status>`, `timeout`, `error`.
