# Data Model: Inbound Call Bridging over the Host-Side LTE Registration

**Feature**: `017-volte-inbound-bridge` | **Date**: 2026-07-22

State is in-memory and service-scoped, except call and message history, which
use the **existing** store — no new schema. Entities marked **(exists)** are
already in the tree and are reused or extended rather than replaced, which is
what keeps FR-019 and SC-008 true.

---

## BridgedCall **NEW**

One incoming call and its two legs.

| Field | Type | Notes |
|---|---|---|
| `call_id` | string | Correlates both legs and the history record |
| `caller` | E.164 + display name | Presented onward to the telephone system (FR-003). Both are supplied by the network — verified in the probe |
| `stage` | `CallStage` | Where it got to; the basis of FR-016 |
| `carrier_leg` | leg state | The network side |
| `pbx_leg` | leg state | The telephone-system side |
| `ended_by` | `Caller` \| `Pbx` \| `AttachmentLost` \| `RegistrationLost` | FR-004, FR-011 |
| `media` | `MediaReport` **(exists)** | Reused from feature 016 — carries the one-way verdict |
| `started_at` / `duration` | timestamps | History |

### CallStage

```
Offered ──accept──> Answering ──pbx rings──> PbxRinging ──pbx answers──> Bridged
   │                    │                        │                          │
   │                    └── cannot answer ──┐    └── pbx never answers ──┐   │
   └── rejected (busy) ────────────────────┴───────────────────────────┴──> Ended
```

**Validation rules**

- A second call offered while any call is past `Offered` is rejected as busy
  (FR-006); it never becomes a `BridgedCall`.
- `Bridged` is the only stage that can produce a successful outcome.
- A call reaching `Bridged` whose `media.verdict` is not both-ways is a
  **failure** (FR-017), not a success — carried forward from feature 016, where
  the same rule caught a real defect.
- `ended_by` must always be set; "the call ended" without a reason is what makes
  an operator re-run a failure to learn anything.

---

## ServiceRegistration **(exists — `ims::RegistrationStatus`)**

The single long-lived registration serving both liveness and calls.

| Field | Notes |
|---|---|
| `state` | **(exists)** `Unregistered` / `Registering` / `Registered` / `Renewing` / `Failed` — the shared vocabulary FR-018 requires |
| `registered_at` / `expires_at` | **(exists)** Drives renewal |
| `last_failure` | **(exists)** Reported in status |
| `renewal_deferred` | **NEW** — true while a call is in progress (FR-009) |

**Validation rules**

- Renewal MUST NOT run while a call is active (FR-009). Already implemented on
  the Wi-Fi path, with the reason recorded at the site: renewing mid-call tears
  down the transport the call's own ending still needs.
- **Re-attachment MUST NOT run mid-call either.** This is the new hazard: the
  carrier tears the attachment down roughly every two hours and the registration
  loop re-attaches automatically. The existing deferral covers renewal only.
- A call is allowed to outlive its registration (spec Assumptions): dropping a
  live conversation to satisfy a timer is worse than a registration lapsing
  slightly late.

---

## InboundMessage **NEW**

A text message, from either delivery route.

| Field | Type | Notes |
|---|---|---|
| `route` | `OverRegistration` \| `ThroughModem` | Which route delivered it (FR-036) |
| `sender` | E.164 | |
| `body` | text | |
| `dedupe_key` | derived | Sender + timestamp + body, so a retransmission is recognised (FR-027, FR-037) |
| `recorded` / `forwarded` | bool | Recorded even when forwarding fails (FR-029) |

### Route convergence

```
over the registration ─┐
                       ├──> dedupe ──> record ──> forward ──> acknowledge/clear
through the modem  ────┘
```

**Validation rules**

- Recorded and forwarded **exactly once** regardless of route (FR-037).
- Acknowledged to the network only *after* recording, so a crash mid-handling
  makes the network retry rather than the message vanish (FR-026).
- A modem-delivered message is cleared from modem storage only after recording,
  for the same reason (FR-036).
- Recording must succeed even when forwarding fails; losing a message because a
  downstream service was down is the worst available outcome (FR-029).

---

## ServiceHealth **NEW (assembled from existing parts)**

What a live status query answers (FR-014, FR-033).

| Field | Notes |
|---|---|
| `registration` | `ServiceRegistration` above |
| `active_call` | The current `BridgedCall`, if any — **live**, which is why a published snapshot cannot serve this |
| `attachment` | Whether the network attachment is up and routable |
| `can_answer` | Derived: registered **and** attached **and** no call in progress (SC-009) |
| `recent_calls` | Last N outcomes (FR-015) |

**Validation rule**: `can_answer` must be false whenever the service could not
in fact answer. Exclusive card assignment removes the fallback (FR-034), so a
wrong answer here means silently missed calls.

---

## CardAssignment **(exists — extended)**

| Value | Meaning |
|---|---|
| `CircuitSwitched` | **Default.** Today's behaviour |
| `WifiCalling` | **(exists)** |
| `HostSideCellular` | **NEW** — this service |

**Validation rules**

- Exactly one subsystem per card (FR-034). The "modem claimed by both
  subsystems" hazard is already documented in discovery, with a live symptom.
- Absent selection means `CircuitSwitched` (FR-024) — the feature is opt-in and
  changes nothing until asked.

---

## Relationships

```
CardAssignment ──decides──> whether the service runs for a card
                                   │
                                   ▼
                        ServiceRegistration ──held by──> the service
                              │        │
              ┌───────────────┘        └────────────────┐
              ▼                                          ▼
        BridgedCall ──produces──> MediaReport      InboundMessage
              │                    (exists)         (either route)
              └──────────┬───────────────────────────────┘
                         ▼
                   ServiceHealth  ──answers──> the live status query
```

One registration feeds both calls and messages — which is exactly why renewal
and re-attachment must yield to a call in progress.
