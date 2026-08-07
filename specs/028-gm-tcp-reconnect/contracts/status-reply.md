# Contract: Status Reply & CLI Output

**Feature**: `028-gm-tcp-reconnect`

## `RegistrationStatusReply`

`control::protocol` — one new field:

```rust
/// Rendered Gm connection health. `#[serde(default)]` so a reply from an
/// older peer that omits it still parses (it then reads empty, and the CLI
/// prints "unknown" rather than claiming health it was not told about).
#[serde(default)]
pub gm_connection: String,
```

Rendered forms:

| State | String |
|---|---|
| `Up` | `up` |
| `Reconnecting { since, attempts }` | `reconnecting since 2026-08-07T10:14:03Z (attempt 2)` |
| `Failed { since }` | `failed since 2026-08-07T10:14:03Z` |
| absent (older peer) | CLI prints `unknown` |

A `String` rather than a typed enum, for the same reason `state` already is
one: the CLI prints it verbatim, and a rendered string keeps wire compatibility
trivial.

## `can_answer` / `blocked_reason`

`ServiceHealth::can_answer()` gains `&& self.gm_connection_up`.

`blocked_reason()` ordering — the connection check goes **after** `attached`
and `registered`, **before** `pbx_registered`:

```rust
if !self.attached            { "the network attachment is down" }
else if !self.registered     { "not registered" }
else if !self.gm_connection_up { "the carrier signaling connection is down" }
else if !self.pbx_registered { "the PBX registration is down" }
else if self.busy            { "a call is already in progress" }
else                         { None }
```

Both layers underneath the connection are reported first: a down attachment
explains a down connection, and surfacing the symptom over the cause sends an
operator to the wrong place.

**Behaviour change**: `can_answer` now reads `false` for the ~2–3s of a
successful reconnect. This is correct and intended — it is briefly true that a
call could not be answered — and follows `can_answer`'s stated doctrine that it
"must never be optimistic" (`lifecycle.rs:419-433`), because a card on this
path has no circuit-switched fallback.

## CLI output

Both printers gain one line, after `expires_at` and before `last_failure` —
alongside the registration facts it qualifies, and above the failure detail.

`vowifi-status` (`vowifi/mod.rs:~1890`):

```
    state: Registered
    registered_at: 2026-08-07T09:20:11Z
    expires_at: 2026-08-07T10:20:11Z
    gm_connection: reconnecting since 2026-08-07T10:14:03Z (attempt 2)
    last_failure: none
    can_answer: false
    blocked_reason: the carrier signaling connection is down
```

`volte-status` (`volte/bridge.rs:~535`): identical field, identical position.

Both are plain `println!` into the existing block; no format change to any
existing line, so anything parsing today's output keeps working.

## Test assertions

- A reply serialised without `gm_connection` (older peer) deserialises, and the
  CLI prints `unknown` rather than `up`.
- `can_answer` is `false` and `blocked_reason` names the connection when the
  connection is down and everything else is healthy.
- `blocked_reason` reports the *attachment* — not the connection — when both
  are down (ordering regression guard).
