# Feature Specification: Match in-dialog SIP requests to the call they name

**Feature Branch**: `042-dialog-transaction-identity`
**Created**: 2026-08-26
**Status**: Draft
**Input**: User description: "Batch 3 conformance fixes: MT-01 (no server transaction layer — retransmission, ACK tracking), MT-02 (a re-INVITE is treated as a second call and refused 486), MT-08 (in-dialog requests are not matched to a dialog — BYE tears down whichever call is active, regardless of Call-ID)."

## Why this exists

This bridge answers exactly one call at a time per phone line. That single-call
design is deliberate and stays as-is. The problem is what happens once that one
call is in progress: every request that arrives while it's up is acted on
because *a* call is active, never because *this* request actually names that
call. Three consequences of that, found during a protocol-conformance review
against RFC 3261 (`docs/plans/mt-conformance-findings.md`, batch 3):

- A `BYE` ends whichever call is currently up, without checking whether it
  actually belongs to the dialog the `BYE` names. A `BYE` that arrives late, or
  that names a stale or unrelated call, still hangs up the live one.
- A second `INVITE` for the call already in progress — a network retransmission
  of the original offer, or a legitimate mid-call re-invitation — is
  indistinguishable from a brand-new second call, and gets refused "busy,"
  which is not what's actually happening.
- Nothing acknowledges a `CANCEL` for a call that's already been answered, and
  nothing checks that an `ACK` actually confirms the call it claims to.

These are all one gap: the bridge tracks *that* a call is active, not *which*
call a given request is about. Closing it does not change the one-call-per-line
behavior — it makes the bridge check identity before acting, the way a
compliant SIP endpoint is expected to (RFC 3261 §12.2.2).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A stray hangup can't end the wrong call (Priority: P1)

A call is in progress. An unrelated `BYE` reaches the line — a retransmission
of an old, already-ended call's hangup, a message meant for a call that never
completed, or anything else naming a call other than the one actually up. The
active call is untouched; only a `BYE` that actually names it ends it.

**Why this priority**: This is the one with real, immediate consequences for
the person on the call right now — a live conversation getting silently cut by
a signal that was never about it. It's independent of the other two: fixing it
requires nothing from either.

**Independent Test**: With a call in progress, send a `BYE` naming a different
(or no longer valid) call, and confirm the live call keeps running unaffected
and the stray `BYE` is refused. Separately, confirm a `BYE` that does name the
live call still ends it exactly as before.

**Acceptance Scenarios**:

1. **Given** a call in progress, **When** a `BYE` arrives naming that same
   call, **Then** the call ends normally, reported as ended by the caller.
2. **Given** a call in progress, **When** a `BYE` arrives naming a different
   call, **Then** the active call is unaffected and the `BYE` is refused as
   naming a call the bridge has no record of.
3. **Given** no call in progress, **When** a `BYE` arrives, **Then** it is
   refused as naming a call the bridge has no record of, rather than answered
   as if a call had existed.

---

### User Story 2 - Repeated or late signaling doesn't cause double effects (Priority: P2)

Networks retransmit. An `INVITE` the bridge already answered, or is already
ringing out on, can legitimately arrive again — and a `CANCEL` can arrive after
the call it was meant to stop has already been answered. None of these should
be treated as something new: a repeated offer gets the same answer it already
got, and a late cancellation gets an explicit acknowledgement instead of being
dropped or mistaken for a request about a nonexistent call.

**Why this priority**: This is about robustness under ordinary network
conditions (UDP duplication, timing races) rather than a single dramatic
failure, and it shares its underlying mechanism with User Story 1 and 3 rather
than needing its own.

**Independent Test**: While a call is ringing, resend the same offer and
confirm the caller gets the same ringing indication, not a fresh ring attempt.
Once the call is answered, resend the original offer again and confirm the
caller gets the same answer already given, not a second call attempt. Send a
`CANCEL` for a call that has already been answered and confirm it gets an
explicit reply rather than being dropped or refused as if it named nothing at
all.

**Acceptance Scenarios**:

1. **Given** a call still ringing, **When** the same offer arrives again,
   **Then** the caller receives the same ringing response already given, and
   ringing is not restarted.
2. **Given** a call already answered, **When** the same offer that started it
   arrives again, **Then** the caller receives the same answer already given,
   and no second call attempt is made.
3. **Given** a call already answered, **When** a `CANCEL` naming that call
   arrives, **Then** it receives an explicit acknowledgement tied to that
   call's identity, distinct from the refusal given to a `CANCEL` for a call
   that never existed.
4. **Given** a call in progress, **When** an acknowledgement (`ACK`) arrives
   naming a different call, **Then** it is not treated as confirming the
   active call.

---

### User Story 3 - A mid-call re-invitation is declined honestly, not refused as busy (Priority: P3)

A caller's network (or a future PBX/carrier) sends a second offer for the call
already in progress — a legitimate protocol pattern used for session refresh or
changing call parameters mid-call. The bridge does not support acting on it, but
it must say so honestly: the line is not busy, it simply can't renegotiate a
call already in progress.

**Why this priority**: No carrier this bridge currently talks to has been
observed doing this, so it has the least immediate impact of the three — but
it's a real interoperability gap for any compliant peer that does, and it's
worth fixing now that the same mechanism already exists for User Stories 1
and 2.

**Independent Test**: With a call in progress, send a second offer using the
same call identity but describing it as a new request (not a repeat of the
original), and confirm the response says the change can't be honored, distinct
from the "line is busy" response a genuinely separate second call would get.

**Acceptance Scenarios**:

1. **Given** a call in progress, **When** a genuinely new offer arrives for
   that same call (not a retransmission of the original), **Then** it is
   declined with a response that means "this can't be done," not "the line is
   busy."
2. **Given** a call in progress, **When** an entirely separate, unrelated call
   attempt arrives, **Then** it is still refused as busy exactly as today —
   this behavior is unchanged.

---

### Edge Cases

- **A call the bridge placed itself, not one it answered.** If a request later
  arrives naming that call's identity, there is no earlier answer for it to be
  a retransmission of — it must be handled as a mid-call change attempt (User
  Story 3's outcome), never mistaken for a repeat of something that was never
  sent to the bridge in the first place.
- **A duplicate `BYE` for a call that already just ended.** Once a call has
  ended, a second `BYE` naming it must not attempt to end it again or crash;
  it is refused the same way a `BYE` for any other unrecognized call is.
  (Some SIP transports also naturally suppress exact duplicates before they
  reach this handling — this requirement covers what happens if one gets
  through.)
- **Retransmission arriving before the call has rung or been declined at
  all** (a narrow timing window during initial call setup). Best-effort only:
  this spec does not require a response to be resent in that window if none
  has been sent yet, since a normal SIP peer stops retransmitting on any
  response, including the earliest ones this bridge gives.
- **No call active at all**, for any of the three request types (`BYE`,
  `CANCEL`, re-offer) — every one of them must be refused as naming something
  that doesn't exist, never silently accepted or acted on.

## Requirements *(mandatory)*

### Functional Requirements

#### Recognizing which call a request is about

- **FR-001**: The system MUST determine, for every `BYE`, `CANCEL`, `ACK`, and
  in-dialog `INVITE` it receives, whether the request names the call currently
  active on that line, using the call identifier the request carries.
- **FR-002**: The system MUST NOT end, answer, or otherwise act on behalf of
  the active call in response to a request that does not name it.

#### Ending a call

- **FR-003**: A `BYE` that names the active call MUST end that call, reported
  as ended by the far side, exactly as today.
- **FR-004**: A `BYE` that names a call other than the active one, or that
  arrives when no call is active, MUST be refused as naming a call the system
  has no record of, and MUST NOT affect any call that is active.

#### Repeated and late signaling

- **FR-005**: An `INVITE` that repeats the exact offer the system already
  answered for the active call MUST receive the same final answer already
  given, not be evaluated as a new request.
- **FR-006**: An `INVITE` that repeats the exact offer the system is currently
  ringing on MUST receive the same ringing response already given, without
  restarting the ringing process.
- **FR-007**: A `CANCEL` that names a call the system has already given a
  final answer to MUST receive an explicit acknowledgement scoped to that
  call's identity, even though it can no longer change the call's outcome.
- **FR-008**: A `CANCEL` that names a call the system has no record of MUST be
  refused as naming something that doesn't exist.
- **FR-009**: An `ACK` that does not name the active call MUST NOT be treated
  as confirming it.

#### Mid-call re-invitation

- **FR-010**: An `INVITE` that names the active call but is not a repeat of an
  offer already answered MUST be declined with a response indicating the
  change cannot be honored — distinct from the response given to a genuinely
  separate, unrelated second call.
- **FR-011**: An `INVITE` naming a call the system placed itself MUST always
  be treated as a change attempt under FR-010, never as a repeat of an earlier
  answer, since no such answer exists for a call the system originated.
- **FR-012**: A genuinely separate call attempt — one that does not name the
  active call at all — MUST continue to be refused as busy, unchanged from
  today's behavior.

### Key Entities

- **Active call**: The single call, if any, currently occupying a given phone
  line — either one the bridge answered or one it placed. Identified by the
  call identifier carried on every request that belongs to its dialog.
- **In-dialog request**: A `BYE`, `CANCEL`, `ACK`, or subsequent `INVITE` that
  claims to belong to an existing call's dialog, as opposed to an `INVITE`
  proposing a brand-new one.
- **Repeated offer**: An `INVITE` that names the active call and matches the
  exact request the system already responded to (ringing or answering) for
  that call, as opposed to a new offer on the same call.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A `BYE` naming a call other than the one in progress never ends
  the wrong call — 100% of the time across repeated trials with mismatched
  and missing call identities.
- **SC-002**: A mid-call re-invitation on the call in progress is never
  answered with the same response given to a genuinely separate, unrelated
  call attempt.
- **SC-003**: A retransmitted request arriving while a call is ringing or
  immediately after it's answered produces one observable outcome (one ring
  indication, one answer, one call attempt on the far side of the bridge) —
  never a duplicate.
- **SC-004**: A `CANCEL` arriving after a call has been answered always
  receives a response distinguishable from the response given to a `CANCEL`
  naming a call that never existed.

## Assumptions

- The bridge continues to handle exactly one call at a time per phone line;
  this feature does not add support for multiple concurrent calls on one
  line, and "the active call" remains a single, unambiguous thing to check a
  request's identity against.
- No carrier or PBX this bridge currently operates against has been observed
  sending a genuine mid-call re-invitation; User Story 3 is a
  correctness/interoperability fix for a case that hasn't caused a live
  incident yet, not a response to one.
- Actually renegotiating a call already in progress (changing its media,
  putting it on hold, refreshing its session) is out of scope for this
  feature. The requirement is to respond to an attempt honestly, not to
  support it.
- A request arriving before the bridge has sent any response at all for a
  brand-new call (the earliest moment of call setup) is not required to be
  specifically handled as a retransmission by this feature — see Edge Cases.
