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

### `CandidateLine`

The SIP-owning process's view of one configured line, built from the
existing `AgentReport` liveness/state stream (no new state source — this is
a read model over data already reported).

| Field | Type | Notes |
|---|---|---|
| `id` | `LineId` | Existing per-line identity (modem slot, or agent `(kind, module_id)`) |
| `path` | `CarrierPath` | `CircuitSwitched` \| `VoWifi` \| `Volte` |
| `registered` | `bool` | From existing liveness tracking |
| `busy` | `bool` | Existing inbound-call busy state, now also set by an in-flight outbound attempt on the same line |
| `recovering` | `bool` | Existing self-healing/backoff state |

`idle(&self) -> bool { self.registered && !self.busy && !self.recovering }`
— this is the sole definition of "idle" (FR-005); no additional outbound-
specific eligibility rule exists.

### Line selection (FR-004, FR-007, FR-008, FR-009)

No priority ordering, no path preference (FR-007, resolved by
clarification): iterate `CandidateLine`s in whatever order the SIP-owning
process already holds them (a `Vec`/map populated by existing discovery), and
claim the first `idle()` one. "Claim" means: atomically mark it non-idle in
the SIP-owning process's own view *before* issuing `PlaceCall` (R-003 race
handling) — this local mark is provisional and is corrected by the line's
next `AgentReport` regardless of outcome, so a crashed or lost `PlaceCall`
cannot wedge a line non-idle forever.

### `PlaceCall` command / `PlaceCallOutcome` (control protocol)

Sent over the new synchronous per-agent listener (`control::line_server`,
research.md R-003), not the existing `AgentReport` channel.

```rust
struct PlaceCall {
    destination: String,
}

enum PlaceCallOutcome {
    Placed,                 // dial-out leg established; media bridging proceeds
    Busy,                   // line was not actually idle (lost the local race)
    Failed { reason: String },
}
```

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
select CandidateLine (FR-004/007) ──none idle──▶ refused_no_idle_line
        │ found, claimed locally (provisional)
        ▼
PlaceCall → line_server (same process or cross-process)
        │
        ├─ Busy ─────────────────────────▶ refused_no_idle_line (FR-008)
        ├─ Failed ───────────────────────▶ refused_network_failure (no retry, FR-009a)
        └─ Placed
              │
              ▼
        mobile network progress relayed (FR-012) ── busy/rejected/unreachable ─▶ refused_network_failure
              │ answered
              ▼
        two-way audio, existing bridging/teardown (FR-013) ─▶ placed
```

**Naming note (revision 4)**: the `PlaceCall`/`line_server` step above
describes the original, since-superseded cross-process design
(`contracts/line-command.md`, likely unneeded — `ControlCmd::Dial` already
covers the cross-process CS case). The unrelated, actually-implemented
`ControlMessage::PlaceCall` (`contracts/agent-outbound-protocol.md`) is the
VoWiFi/VoLTE dispatch path, Agent B → Agent A — see that contract for its
own state transitions, which mirror the shape above with `Placed`/`Failed`
replaced by `CallPlaced`/`CallFailed` and no `Busy` case (a line already
selected via `select_idle_line` before either dispatch path is chosen).

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
