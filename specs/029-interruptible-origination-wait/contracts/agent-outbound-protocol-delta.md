# Contract delta: outbound attempt phase

Amends `specs/025-outbound-calling/contracts/agent-outbound-protocol.md`. Only
the differences are stated here; everything not mentioned is unchanged.

**Wire format: unchanged.** No new `ControlMessage` variant, no field changes.
The delta is entirely in *when* an existing message may be sent and how it must
be interpreted.

---

## 1. `CallEnded` is now legal during the attempt phase

**Before**: `CallEnded` was exchanged only once a call was established —
after `CallPlaced` (outbound) or `CallAnswered` (inbound).

**After**: Agent B MAY send `CallEnded` at any point after `PlaceCall`,
including before `CallPlaced` or `CallFailed` has been received.

```json
{"CallEnded": {"call_id": "out-42", "reason": "caller_hangup"}}
```

Meaning in this phase: *the party that asked for this call is gone; stop
placing it.*

### Agent A obligations on receipt

1. **Match `call_id`** against the attempt in flight. A mismatch MUST be logged
   and ignored — never acted on (FR-010).
2. If the carrier INVITE is still pending: send `CANCEL` for it, then reply
   `CallFailed{call_id, reason: "caller_hangup: ..."}`.
3. If the carrier already answered but the legs are not yet bridged: send `BYE`
   for the answered leg, then reply `CallFailed`.
4. Exactly one `CallPlaced` **or** `CallFailed` is sent per `PlaceCall`,
   including on this path (FR-019).

### Agent B obligations after sending

- MUST NOT try the next line for this request (FR-004).
- MUST NOT wait for `CallFailed` before tearing down its own legs; the reply is
  informational at this point. It is still sent so Agent A's accounting stays
  symmetric.

---

## 2. Progress table — one added row

| Agent A sends | Agent B does | Phase |
|---|---|---|
| `CallAttempting` | switch to the long wait | unchanged |
| `CallRinging` | `answer(180)` — caller hears ringback | unchanged |
| `CallPlaced` | bridge the legs; enter the active-call loop | unchanged |
| `CallFailed` | answer the phone leg with a mapped status; may try the next line | unchanged |
| **(receives `CallEnded` from B)** | **—** | **NEW: attempt phase** |

---

## 3. Timing contract

**Unchanged**: `PLACE_CALL_TIMEOUT` (3s, up to `CallAttempting`),
`CALL_ATTEMPT_TIMEOUT` (90s, up to the terminal reply),
`OUTBOUND_INVITE_TIMEOUT` (15s), `OUTBOUND_RING_TIMEOUT` (60s),
`VETH_INVITE_TIMEOUT` (5s).

**Reinterpreted**: `CALL_ATTEMPT_TIMEOUT` becomes an *overall deadline* on
Agent B rather than a socket read timeout. Its value and its meaning to the
protocol are the same; only the mechanism changes (the socket now uses a short
poll timeout so the caller's line can be watched — see research R7).

**New guarantee**: from the caller's leg reaching `Disconnected` to `CANCEL`
leaving Agent A toward the carrier, ~200ms typical (research R8). Spec SC-001
requires ≤10s.

---

## 4. New: line-busy semantics during an attempt

An in-flight outbound attempt now occupies the line for admission purposes.

- An inbound carrier `INVITE` arriving during an attempt receives
  `486 Busy Here`, promptly, and is counted as an ordinary busy refusal
  (FR-011, FR-012, FR-013).
- A second `PlaceCall` arriving during an attempt receives
  `CallFailed{reason: "busy"}` — unchanged behaviour, but now reached promptly
  rather than after the attempt finishes.

Rationale is recorded in the spec's Assumptions: the alternative — holding an
inbound call in case the attempt fails and frees the line — was considered and
rejected.

---

## 5. Metrics

`OutboundAttemptOutcome` gains `CallerAbandoned` (`"caller_abandoned"`).

Distinguishes, for the first time, "the caller gave up" from `Unanswered` ("the
destination never picked up") and from `RefusedNetworkFailure`. Both agents ship
together, so the added variant needs no version negotiation — noted because the
enum crosses a process boundary to the daemon's metrics exporter.
