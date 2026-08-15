# Phase 0 Research: Early Media Relay for Outbound Calls

No `NEEDS CLARIFICATION` markers remain in the Technical Context — this is
a protocol/state-machine extension inside an existing, well-understood
codebase, not a new-technology adoption. This document instead records the
design decisions made while translating the spec into an implementation
approach, and the alternatives rejected.

## R1: Reuse `pair_calls`-before-answer instead of a new early-media path

**Decision**: Extend the outbound flow to call the same
`Endpoint::pair_calls` primitive the *inbound* flow already uses, just
earlier — as soon as the carrier's first SDP-bearing provisional arrives,
not only once the carrier sends `200 OK`.

**Rationale**: `bridge_call` (`vowifi/mod.rs`) already proves this works:
for an inbound call, it places Agent B's PBX-facing leg and its veth leg,
pairs them, and only *then* waits for the PBX extension to actually answer
(`wait_for_pbx_answer`) — PJSIP's conference bridge is fully capable of
carrying audio between two call legs while one of them is still in an
early/ringing dialog. Reusing this primitive means the outbound direction
needs no new PJSIP surface, only a new *trigger point* for calling it.

**Alternatives considered**:
- A parallel, early-media-specific audio path (e.g. a raw RTP forwarder
  outside the conference bridge, torn down and replaced once the real
  answer arrives). Rejected: this is a second mechanism to maintain
  alongside the existing bridged-call path, and the swap-over from one
  path to the other is exactly the kind of seam that would produce the
  audible gap the spec's SC-005 rules out. It also duplicates logic
  `pair_calls` already provides for free.
- Buffering/replaying the early audio once the call is later answered
  (rather than relaying live). Rejected outright: it defeats the point —
  the caller needs to hear the carrier's message *while it's happening*,
  not afterward.

## R2: Trigger on *any* SDP-bearing provisional, not on the `P-Early-Media` header

**Decision**: Treat the first provisional response (`180`–`183`) whose
body parses as SDP as "early media available," regardless of whether the
carrier's `P-Early-Media` header (RFC 5009) is present. Already codified
as FR-008/the spec's Assumptions.

**Rationale**: The header is optional and carrier-specific; many networks
that send genuinely audible pre-answer audio don't set it. Gating on it
would silently under-deliver the feature's core value (Story 1) for any
carrier that omits it, for no compensating benefit — the SDP body itself
is the only signal actually required to relay audio.

**Alternatives considered**: Gate strictly on the header, falling back to
today's silent behavior otherwise. Rejected per FR-008.

## R3: No direction-gating (RFC 5009 `sendonly`/`recvonly` enforcement)

**Decision**: Keep the call two-way throughout, exactly as today — per
FR-009, resolved during specification. No new SDP-direction parsing or
uplink suppression.

**Rationale**: No existing precedent in this codebase for asymmetric
media handling (`sdp.rs` only ever emits `a=sendrecv`); building it would
be new infrastructure with no clear benefit for this bridge's actual
carriers, versus the simplicity of leaving audio direction untouched
(Constitution Principle V).

## R4: State machine placement — extend `OriginationStep`, don't add a new step variant

**Decision**: Agent A's early-media bookkeeping (the veth listener
receiver, the "already paired" flag, the connected RTP socket) lives
alongside the existing `AwaitingCarrier`/`provisional_answer` state in
`PendingOrigination`, not as a new `OriginationStep` variant. `on_carrier_response`
stays in `AwaitingCarrier` until the real final response; only the
*content* of what happens on a provisional response changes.

**Rationale**: The call is still, transaction-wise, awaiting the carrier's
final response — nothing about the INVITE transaction's state changes,
only side effects (RTP connect, veth spawn, Agent B notification) move
earlier. Introducing a new top-level step would force every match arm
across `origination.rs` (cancel handling, timeout handling, tick
advancement) to learn a state that isn't really a new phase of the
transaction — needless branching for Principle V.

## R5: Dedup at the real `200 OK` via a flag, not by removing the old path

**Decision**: `finish_origination` keeps its existing full setup path
(RTP connect, veth spawn, `CallPlaced`) intact for carriers that never
send early media (straight `100`/`180` with no SDP → `200 OK`), and adds a
branch that skips re-doing work already done when early media fired
first.

**Rationale**: Zero risk to the large existing carrier population this
was already tested against (research note in `origination.rs` documents
several carrier-specific fixes already living in this exact code path).
Additive, not rewritten.

## R6: Teardown reuses `Call::hangup()` unchanged; only the *reachability* is new

**Decision**: No new PJSIP-facing teardown code. `Call::hangup()` already
handles a call in any state (`pjsua_call_hangup` is state-agnostic per its
existing doc comment in `pjsua-safe/src/call.rs`). What's new is a code
path that can *reach* the local leg and veth leg from the new
early-paired-but-not-yet-`CallPlaced` state, both when the caller hangs up
(Agent B's local `call` state) and when the carrier fails (Agent A's
`fail()`/`AwaitingCancel` path, which today sends nothing to Agent B
before `CallPlaced` because there was nothing to tear down).

**Rationale**: Matches the contract-delta pattern `specs/029` already
established for a structurally similar problem (making `CallEnded` legal
earlier in the attempt phase than it used to be) — extend *when* an
existing message/primitive applies, don't invent new teardown mechanics.
