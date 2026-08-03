# Phase 1 Data Model: Outbound Calling

## Entities

### `OutboundCallRequest`

Created the moment an eligible INVITE is accepted (PBX trunk UAS, or a
phone's INVITE after the registrar's 302 redirect lands on `Account::local`).
Lives only for the duration of one attempt — never persisted.

| Field | Type | Notes |
|---|---|---|
| `destination` | `String` | Request-URI user part, byte-for-byte (FR-010) |
| `origin` | `Origin` | `Pbx` \| `SipServerPhone { aor: String }` — for logs/metrics only, never affects routing (FR-003) |
| `call_id` | `String` | The originating SIP dialog's Call-ID, for log correlation |

```rust
enum Origin {
    Pbx,
    SipServerPhone { aor: String },
}
```

### Line selection (FR-004, FR-007, FR-008, FR-009) — as actually implemented

**2026-08-03 revision**: the generic `CandidateLine`/`select_idle_line`
abstraction originally described here was never wired in and has been
deleted (`sip/outbound.rs` now only holds `OutboundOutcome` and
`validate_destination`). The two real paths each grew their own selection
logic against their own state representation instead:

- **Circuit-switched** (`modules::mod::handle_outbound_request`): iterates
  the daemon's in-process `SlotState`s in whatever order they're held, and
  claims the first slot whose `CardState == Idle`. No priority ordering, no
  path preference (FR-007) — same "first idle wins" rule as originally
  specified, just against `SlotState` rather than a shared `CandidateLine`
  read model.
- **VoWiFi/VoLTE** (`vowifi::mod::run_outbound_listener`): iterates its own
  `RuntimeLine`s the same way, claiming the first one that's registered and
  not already busy.

In both cases, "claim" is still provisional-and-self-correcting: a failed or
lost dial attempt leaves the line's next liveness/state report to reset it,
so nothing can wedge a line non-idle forever — same guarantee as originally
designed, just enforced locally in each path rather than via a shared
`idle()` method.

### Dispatching a claimed line (control protocol) — as actually implemented

**2026-08-03 revision**: the `PlaceCall`/`PlaceCallOutcome` wire structs and
the `control::line_server`/`line_client` synchronous per-agent listener
described here were never wired in either and have been deleted
(`control/protocol.rs`, `control/line_client.rs`, `control/line_server.rs`
no longer exist). The two real paths use different, already-existing
mechanisms instead, both same-process:

- **Circuit-switched**: `CardPool::handle_outbound_request` sends
  `ModuleCmd::Dial` straight into the selected modem's own command loop,
  replying via a `oneshot::Sender<Result<(), String>>` — the same pattern
  `SetMode`/`Reboot` use, but dispatched directly rather than over the
  control socket, since the SIP side and `CardPool` always live in the same
  daemon binary. **2026-08-03**: `ControlCmd::Dial` (the wire-visible
  variant this section used to describe, meant for a *different* process
  reaching a CS line over the control socket) has been deleted — its only
  real caller was `vowifi::mod`'s cross-process CS fallback, itself removed
  in an earlier review pass for lacking any cross-process audio bridge (see
  `contracts/control-cmd-dial.md`'s superseded banner). `ModuleCmd::Dial`
  itself is unaffected and still does the actual dialing.
- **VoWiFi/VoLTE**: `vowifi::control::ControlMessage::PlaceCall` (JSON over
  the existing Agent A ↔ Agent B TCP control channel — see
  `contracts/agent-outbound-protocol.md`), answered by `CallPlaced` /
  `CallRinging` (non-terminal) / `CallFailed`, mapped locally in
  `try_place_on_line`'s `PlaceCallOutcome` (`Placed` / `Unavailable` /
  `Committed` — note this is a same-named but distinct, VoWiFi-local enum
  from the deleted wire-protocol type above) to decide whether a
  network-failure is cheap to retry on another line or must abort per
  FR-009a.

### Outbound outcome categories (FR-015)

Matches inbound's existing outcome granularity:

| Outcome | Counted as |
|---|---|
| Destination answered | `placed` |
| No `CandidateLine` was idle | `refused_no_idle_line` |
| Destination number empty/malformed (FR-014) | `refused_invalid_destination` |
| Selected line failed before network placement (FR-009a) | `refused_network_failure` |
| Network rejected/busy/unreachable | `refused_network_failure` |
| Rang out unanswered | `unanswered` |

## State transitions

```text
INVITE accepted (PBX UAS, or phone via 302 redirect)
        │
        ▼
validate destination (FR-014) ──fail──▶ refused_invalid_destination
        │ ok
        ▼
select a line (FR-004/007, per-path logic above) ──none idle──▶ refused_no_idle_line
        │ found, claimed locally (provisional)
        ▼
dispatch (ModuleCmd::Dial, or ControlMessage::PlaceCall — per-path, see above)
        │
        ├─ line lost the race / not actually idle ──▶ refused_no_idle_line (FR-008)
        ├─ failed before carrier placement (Unavailable) ─▶ refused_network_failure, retry next line
        ├─ failed after carrier placement (Committed) ────▶ refused_network_failure or unanswered
        │                                                    (no retry, FR-009a)
        └─ Placed
              │
              ▼
        carrier progress relayed (FR-012, `CallRinging`) ── busy/rejected/unreachable ─▶ refused_network_failure
              │ answered
              ▼
        two-way audio, existing bridging/teardown (FR-013) ─▶ placed
```

**Naming note (revision 5, 2026-08-03)**: revision 4's cross-process
`PlaceCall`/`line_server` design (`contracts/line-command.md`) has now been
deleted outright rather than left unwired — see "Dispatching a claimed
line" above. The diagram now reflects the two real dispatch paths directly.
The VoWiFi/VoLTE `PlaceCallOutcome` used to decide the middle three branches
is a local, same-named-but-unrelated enum in `vowifi::mod`, not the deleted
wire-protocol struct.

## Validation rules

- **FR-014**: `destination` MUST be non-empty and contain only characters
  valid in a SIP `user` production subset actually reachable by `ATD`/IMS
  Request-URI (digits, `*`, `#`, `+`) — rejected before any `CandidateLine`
  is touched.
- **FR-008/FR-009a**: a `Busy` or `Failed` `PlaceCallOutcome` MUST NOT trigger
  a second `PlaceCall` to a different line for the same `OutboundCallRequest`
  — no automatic retry (resolved by clarification 2026-08-02).
- **FR-017**: with the feature disabled, the PBX-trunk UAS handler and the
  registrar's INVITE branch MUST take the pre-feature code path (accept
  nothing / `403`) byte-for-byte.
