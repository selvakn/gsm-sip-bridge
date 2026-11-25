# Data Model: The Supervisor Stops Retrying in a Tight Loop While a Card-Reader SIM Is Out of Reach

All state is in memory, lives as long as the supervisor process, and exists only for card-reader (`pcsc_reader`) lines. Nothing is persisted. Paths are relative to `gsm-sip-bridge/src`.

## Shared signals (D1)

| Item | Where | Value | Pinned by |
|---|---|---|---|
| `CARD_UNREACHABLE_MARKER` | `ims/agent/mod.rs` | `the SIM card is out of reach; exiting` | `sim_recovery` test that `has_card_unreachable` matches the marker as the agent logs it |
| `EXIT_CARD_UNREACHABLE` | `vowifi/plmn.rs` | `3` | test that `card_check` maps exit 3 to `Out`, and `plmn::run`'s mapping test |

## `CardVerdict` (`supervise/card_outage.rs`)

`Present` | `Out` | `Unknown`. It is what one `card_check` found:

- exit 0 → `Present`;
- exit `EXIT_CARD_UNREACHABLE` → `Out`;
- anything else, including `timeout`'s 124 and a spawn error → `Unknown`.

`Unknown` never starts or ends an outage.

## `CardOutage` (`supervise/card_outage.rs`), one per card-reader line, `Arc<Mutex<_>>`

| Field | Meaning |
|---|---|
| `since: Option<(Instant, SystemTime)>` | When the outage began. `None` means there is no outage |
| `last_check: Option<Instant>` | When a check last ran, from either thread. Gates `check_due` |
| `agent_delay: Duration` | The next agent restart delay: `NORMAL_RESTART_DELAY`, then ×2 up to `CARD_OUTAGE_MAX_BACKOFF` |
| `tunnel_interval: Duration` | `CARD_TUNNEL_INITIAL_INTERVAL`, then ×2 up to the cap |
| `tunnel_next_at: Option<Instant>` | When the next paced initiation may go. `None` means as soon as the outage starts (no attempt has been made under it yet) |

Methods (pure; every method takes `now`):

| Method | Effect | Returns |
|---|---|---|
| `on_out(now)` | Starts an outage if there is none, with all delays at their initial values | `Some(Began)` on a transition |
| `on_present(now)` / `on_registered(now)` | Ends the outage and resets every delay | `Some(Ended { held_for })` on a transition |
| `record(verdict, now)` | Sets `last_check`, then calls `on_out` or `on_present`. `Unknown` changes nothing else | the transition, if any |
| `check_due(now)` | `last_check` is `None`, or at least `CARD_CHECK_INTERVAL` ago | `bool` |
| `next_agent_delay()` | The current `agent_delay`, then doubles it | `Duration` |
| `tunnel_attempt_due(now)` | The outage is on, and `tunnel_next_at` is `None` or past | `bool` |
| `tunnel_attempted(now)` | `tunnel_next_at = now + tunnel_interval`, then doubles `tunnel_interval` | - |
| `is_out()` | `since.is_some()` | `bool` |

Invariants:

- It exists only for card-reader lines. Modem and VoLTE lines never construct one.
- At most one check runs per `CARD_CHECK_INTERVAL` per line, whichever thread asks.
- `Began` and `Ended` alternate strictly, so there is one log line each and one alert-state change each.
- Every path out of an outage resets every delay. A later outage starts from the initial values (latching-state class).

## `AgentExitOutcome` (`supervise/sim_recovery.rs`): what changes

| Variant | Recognised by | `observe` | Restart delay |
|---|---|---|---|
| `AtStall` | `STALL_MARKER` | as today | as today |
| `CsimFailure` | `AT+CSIM failed` | as today | as today |
| **`CardUnreachable`** (new) | `CARD_UNREACHABLE_MARKER`, card-reader lines only | `Action::None`, counters untouched | `CardOutage::next_agent_delay()`, waited in 5 s steps with checks; early on `Present` or shutdown |
| `Other` | anything else | as today (resets) | as today (5 s) |

Classification order: `AtStall`, `CsimFailure`, `CardUnreachable`, `Other`.

## `CardView` (`supervise/line_supervisor.rs`), built before each tick

| Field | Meaning |
|---|---|
| `pcsc_reader: bool` | `false` makes the card gate a no-op (modem lines, and every existing test via `CardView::none()`) |
| `outage: bool` | `CardOutage::is_out()` when the view was built |
| `attempt_due: bool` | `CardOutage::tunnel_attempt_due(now)` |

Passed with a `check: &dyn Fn() -> CardVerdict`, called at most once per tick, and only when `pcsc_reader && !outage` and a SIM-needing recovery is about to be issued.

## `SteadyOutcome::CardHeld` (new variant)

`CardHeld { skipped: DegradeReason, initiated: bool }`. A SIM-needing recovery was held because the line's card is out of reach. `initiated` is true on the tick that made the paced initiation for a missing IKE_SA.

- `outcome_restarts_agent(CardHeld)` is false.
- `UnderlayTracker::after(CardHeld)` changes no uplink state.
- After the tick, the loop calls `CardOutage::record(Out)` when the gate's own check found the card out (`Began` is logged then), and `tunnel_attempted` when `initiated`.

## `needs_the_sim(reason)` (`supervise/line_supervisor.rs`)

Exhaustive, with no wildcard.

| `DegradeReason` | Needs the SIM | Card-gate rule |
|---|---|---|
| `IkeSaLost` (IkeSaMissing), `IkeSaNegotiationStuck` | yes | paced initiation (no IKE_SA to lose) |
| `PcscfPoolExhausted`, `PcscfUnreachable`, `ChildSaStuck`, `ChildSaRekeyStuck`, `TunVanished` | yes | held while the card is out (would tear down an established IKE_SA) |
| `ProcessDied`, `ViciBroken` | no (shared charon; every line's missing IKE_SA is then gated on its own) | not gated |
| `ChildSaMissing`, `TunUnavailable`, `DefaultRouteMissing`, `PcscfChanged` | no (no new IKE negotiation) | not gated |

Pinned: `needs_the_sim(r) == recovery_renegotiates(r) && !matches!(r, ProcessDied | ViciBroken)` for every variant.

## `AgentState.card_hold` (`control/protocol.rs`)

`Option<bool>`, with `#[serde(default, skip_serializing_if = "Option::is_none")]`. Only the supervisor sets it: `Some(true)` on `Began`, `Some(false)` on `Ended`. The daemon keeps it as `last_card_hold` on the supervisor's record (`metrics/ingest.rs`), as it keeps `last_uplink`.

## State machine (one card-reader line)

```text
                         card-unreachable agent exit, or gate check → Out
   ┌────────── normal ──────────────────────────────────────────────┐
   │ agent: restart in 5 s                                          │
   │ tunnel: recoveries as today (a gate check before one that      │
   │         needs the SIM)                                         │
   └────────────────────────────────────────────────────────────────┘
                 │ Began → log once, card_hold=true
                 ▼
   ┌────────── outage ──────────────────────────────────────────────┐
   │ check every 5 s (either thread, shared)                        │
   │ agent: restart after 5,10,20,…,120 s, early on Present         │
   │ tunnel: missing IKE_SA → one initiation per 60,120,120… s      │
   │         rebuild of an established IKE_SA → held                │
   │ loop ticks every 5 s                                           │
   └────────────────────────────────────────────────────────────────┘
                 │ check → Present, or the agent registers
                 │ Ended → log once with duration, card_hold=false,
                 │ every delay reset
                 ▼
               normal
```
