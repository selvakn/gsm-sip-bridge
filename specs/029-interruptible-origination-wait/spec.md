# Feature Specification: Interruptible wait for outbound call origination

**Feature Branch**: `029-interruptible-origination-wait`
**Created**: 2026-08-07
**Status**: Draft
**Input**: User description: "address the gap identified and triaged at `docs/plans/dispatch-loop-interruptible-wait.md`"

## Context

Placing an outbound call over the carrier can legitimately take a long time —
the carrier may spend fifteen seconds acknowledging the request and another
minute ringing the destination before anyone picks up. Today, for that entire
window (up to roughly eighty seconds), **both halves of the bridge stop
listening to everything else**:

- The carrier-facing half stops watching for incoming calls and stops
  watching the connection to the telephone-facing half.
- The telephone-facing half stops watching the caller's own line, so it never
  notices that the person who dialled has hung up.

The consequence is not a crash or an error — it is silence. Nobody is told,
nothing is logged as a failure, and the observable damage lands on people
outside the system: a destination that keeps ringing for a call that has been
abandoned, and an incoming caller who hears nothing at all.

This is a documented, deliberately-deferred limitation, not a newly-found
defect: it is recorded in `docs/todo.md`, in the dispatch loop's own inline
`KNOWN LIMITATION` note, and in the cancel-the-pending-call routine's doc
comment. This feature closes it.

**Scope note discovered during specification**: the triage plan attributes the
whole gap to the carrier-facing half, and assumes the telephone-facing half
already emits a hangup signal that simply cannot be heard. It does not. While
an outbound attempt is in flight, the telephone-facing half is itself parked
in a blocking read and never inspects the caller's line state, so no hangup
signal is produced in the first place. **Both halves need the fix**; fixing
only the listening half would leave the primary user story unachievable.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A caller who gives up stops the call they started (Priority: P1)

Someone picks up a desk phone, dials an outside number through the bridge, and
hears ringback. After fifteen seconds nobody has answered, so they hang up and
get on with their day.

Today, hanging up tears down only the local leg. The carrier is never told, so
it goes on ringing the destination for as long as it is willing to — and if
the destination picks up, a call is connected, and billed, with nobody on the
originating end. The person who answers hears dead air from a caller who left
long ago.

**Why this priority**: This is the only part of the gap with consequences that
reach a third party who never interacted with the system at all — the person
being called. It also carries a direct billing cost, and it is the half that
the existing code comments single out as unreachable. Delivered alone, it is
already a complete, valuable fix.

**Independent Test**: Place an outbound call to a destination that does not
answer. Hang up the originating phone while it is still ringing. Confirm the
destination stops ringing promptly, and that the carrier-facing half recorded
sending a cancellation for that attempt.

**Acceptance Scenarios**:

1. **Given** an outbound attempt where the carrier has acknowledged the
   request and the destination is ringing, **When** the originating phone
   hangs up, **Then** the bridge cancels the attempt toward the carrier within
   seconds, and the destination stops ringing.
2. **Given** an outbound attempt where the carrier has not yet sent any
   response at all, **When** the originating phone hangs up, **Then** the
   bridge still cancels the attempt rather than waiting out its own timeout.
3. **Given** an attempt that the originating side abandoned, **When** the
   carrier nonetheless answers in the race window, **Then** the bridge hangs
   the answered leg up rather than leaving it connected to nothing — the same
   handling the existing self-timeout path already performs.
4. **Given** an outbound attempt that was abandoned by the caller, **When** the
   attempt finishes unwinding, **Then** the line is reported free and can
   accept the next call, and the attempt is recorded with an outcome that
   distinguishes "the caller gave up" from "the destination never answered".
5. **Given** an outbound attempt in the brief window after the carrier answered
   but before both legs are bridged, **When** the originating phone hangs up,
   **Then** the carrier leg is hung up rather than left connected.

---

### User Story 2 - Someone calling in during an outbound attempt is not left in silence (Priority: P2)

While the bridge is placing an outbound call, someone rings the line from
outside. Today their call is neither answered nor refused: the request sits
unread, and their own network gives up after about thirty seconds — well
inside the eighty-second window — so from their side the number simply did not
respond. No busy tone, no voicemail hand-off, no ring: nothing.

The line genuinely is busy, so the fix is not to answer them — it is to tell
them so, promptly, the same way the bridge already refuses a call that arrives
during an established one.

**Why this priority**: An incoming caller getting silence is a real and
user-visible failure, but it affects a narrower window than User Story 1 and
its resolution is bounded by a deliberate design rule (one call at a time per
line) rather than by the blocking wait alone. It is worth fixing, and worth
fixing second.

**Independent Test**: Start an outbound attempt to a number that will not
answer. From another phone, call the bridge's line during that window. Confirm
the incoming caller gets a busy signal within seconds, rather than silence
followed by their own network giving up.

**Acceptance Scenarios**:

1. **Given** an outbound attempt in progress, **When** an incoming call
   arrives, **Then** the caller gets a busy response within seconds — well
   inside their own network's transaction timeout — rather than silence.
2. **Given** an incoming call refused as busy during an outbound attempt,
   **When** that outbound attempt fails moments later and frees the line,
   **Then** the already-refused call is not revived; the caller redials.
3. **Given** an outbound attempt in progress, **When** an incoming call is
   refused because the line is busy, **Then** the refusal is recorded in the
   same way an ordinary busy refusal is, so the metric does not silently
   under-count.
4. **Given** an incoming caller who is refused, **When** they compare it to
   being refused during an established call, **Then** the two are
   indistinguishable from their side.

---

### User Story 3 - Operators can see that abandonment is being handled (Priority: P3)

An operator reviewing call records or dashboards can tell how an outbound
attempt ended: connected, rejected by the carrier, rang out unanswered, or
abandoned by the caller before anyone picked up.

**Why this priority**: Pure observability. It does not change what happens on
the wire, but without it the fix in User Story 1 is invisible — abandoned
attempts would be indistinguishable from ordinary failures, and a regression
would go unnoticed.

**Independent Test**: Abandon an outbound attempt mid-ring and confirm the
resulting record and counters name that outcome distinctly from a carrier
timeout or a carrier rejection.

**Acceptance Scenarios**:

1. **Given** a caller-abandoned outbound attempt, **When** the operator
   inspects recent call records, **Then** the outcome is distinguishable from
   "destination never answered" and from "carrier rejected".
2. **Given** any outbound attempt, **When** it ends by any route, **Then**
   exactly one outcome is recorded for it — abandonment does not produce a
   duplicate or a missing record.

---

### Edge Cases

- **The caller hangs up in the last moment before connection.** The abandonment
  arrives after the carrier answered but before both legs are joined. The
  carrier leg must be hung up, not leaked; the outcome recorded once.
- **The caller hangs up and the destination answers simultaneously.** Both a
  cancellation and an answer are in flight. The existing race handling —
  acknowledge, then immediately hang up — must apply here exactly as it does
  for a self-initiated timeout.
- **The connection between the two halves drops mid-attempt.** A lost
  connection is indistinguishable from a hangup in its consequence: the
  originating side is gone. The attempt must be abandoned, not left running to
  its full timeout.
- **The caller hangs up before the carrier has been contacted at all.** No
  cancellation is owed to the carrier; the attempt must simply not be started,
  or be unwound without contacting the carrier.
- **The destination answers normally.** The added interruption checks must not
  cut short a slow-but-legitimate setup — a carrier gap of eighteen seconds
  between progress signals has been observed live and must still complete.
- **Repeated or duplicate hangup signals.** A second abandonment signal for an
  attempt already being unwound must not send a second cancellation or record
  a second outcome.
- **An outbound attempt is abandoned while more lines remain untried.** The
  bridge must stop trying further lines — the caller is gone, so ringing a
  destination from a second line would recreate the exact problem being fixed.
- **Health and upkeep duties during the window.** Registration renewal and the
  carrier-connection keepalive must not be starved or falsely triggered by the
  added polling; renewal has ample headroom today and must keep it.

## Requirements *(mandatory)*

### Functional Requirements

**Detecting abandonment (telephone-facing half)**

- **FR-001**: While an outbound attempt is in flight, the telephone-facing half
  MUST continue to observe the originating call's state, rather than blocking
  exclusively on a reply from the carrier-facing half.
- **FR-002**: The telephone-facing half MUST detect that the originating call
  has ended within a small, bounded interval consistent with the polling
  cadence it already uses for an established call.
- **FR-003**: On detecting that the originating call has ended, the
  telephone-facing half MUST signal abandonment to the carrier-facing half for
  that specific attempt.
- **FR-004**: On abandonment, the telephone-facing half MUST stop the
  line-by-line retry sequence for that request and not attempt any further
  line.

**Acting on abandonment (carrier-facing half)**

- **FR-005**: While waiting for the carrier to respond to an outbound request,
  the carrier-facing half MUST periodically check for an abandonment signal
  without waiting out its full timeout.
- **FR-006**: The interval between those checks MUST be bounded and no longer
  than the existing per-read interval already used on the carrier connection.
- **FR-007**: On receiving an abandonment signal, the carrier-facing half MUST
  cancel the pending request toward the carrier, using the same cancellation
  path — including its handling of a late answer racing the cancellation —
  that a self-initiated timeout already uses.
- **FR-008**: The same interruptibility MUST apply to the wait for the local
  leg that follows the carrier answering, not only to the wait for the carrier
  itself.
- **FR-009**: An abandoned attempt MUST unwind through the existing failure
  path, leaving no connected carrier leg, no running media relay, and no line
  marked busy.
- **FR-010**: The abandonment signal MUST be matched to the attempt it names; a
  signal referring to a different or already-finished attempt MUST be ignored
  rather than cancelling the wrong call.

**Not blocking the rest of the loop**

- **FR-011**: An incoming call arriving during an outbound attempt MUST be
  noticed and refused as busy while the attempt is still in flight, well inside
  the originating network's transaction timeout — not left unanswered until the
  attempt finishes. It MUST NOT be held pending the outbound attempt's outcome,
  even though the line may free up moments later.
- **FR-012**: The refusal MUST be the same busy refusal the bridge already
  issues when a call arrives while another call is active, so an inbound caller
  cannot tell "busy with a call" from "busy placing a call".
- **FR-013**: Refusals issued under FR-011 MUST be recorded through the same
  counters and records as an ordinary busy refusal.

**Preserving current behaviour**

- **FR-014**: A successful outbound call MUST connect exactly as it does today,
  including the case where the carrier takes tens of seconds between progress
  signals.
- **FR-015**: The existing timeout values governing how long the bridge waits
  for the carrier MUST NOT be changed by this feature.
- **FR-016**: The fix MUST NOT introduce concurrent writers to the carrier
  connection; the single-owner, cooperative-polling structure the bridge uses
  today MUST be preserved.
- **FR-017**: Registration renewal and the carrier-connection keepalive MUST
  continue to operate correctly across an outbound attempt, whether it succeeds
  or is abandoned.

**Reporting**

- **FR-018**: A caller-abandoned attempt MUST be recorded with an outcome
  distinguishable from a carrier timeout and from a carrier rejection.
- **FR-019**: Every outbound attempt MUST produce exactly one recorded outcome,
  including abandoned ones.

**Documentation**

- **FR-020**: The three places that currently document this gap as a known,
  unfixed limitation — the pending-items list, the dispatch loop's inline note,
  and the cancellation routine's doc comment — MUST be updated to describe the
  behaviour that now exists, including whatever residual limitation remains.

### Key Entities

- **Outbound attempt**: One request to reach a destination over one line, from
  the moment the telephone-facing half commits to it until exactly one outcome
  is recorded. Identified by a call identifier shared between both halves.
- **Abandonment signal**: A message from the telephone-facing half to the
  carrier-facing half meaning "the person who started this attempt is gone;
  stop". Names the attempt it refers to.
- **Attempt outcome**: The single recorded result of an attempt — connected,
  rejected by the carrier, unanswered, abandoned by the caller, or failed for a
  local reason.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: When a caller hangs up while the destination is ringing, the
  destination stops ringing within 10 seconds, in 100% of attempts — measured
  today as "never, until the carrier itself gives up".
- **SC-002**: Across 20 deliberately-abandoned attempts, zero result in a
  carrier call that connects with nobody on the originating side.
- **SC-003**: A line abandoned mid-attempt is available for the next call
  within 10 seconds of the hangup, versus up to 80 seconds today.
- **SC-004**: Someone calling in during an outbound attempt hears a busy signal
  within 10 seconds, in 100% of attempts, versus roughly 30 seconds of silence
  ending in their own network giving up, today.
- **SC-005**: Every outbound attempt, including abandoned ones, appears exactly
  once in call records with an outcome that names how it ended; abandoned
  attempts are distinguishable from unanswered ones.
- **SC-006**: No regression in successful calls: outbound calls still connect,
  including a case where the carrier leaves a gap of at least 18 seconds
  between progress signals.
- **SC-007**: No regression in upkeep: registration stays valid and the carrier
  connection stays healthy across a full-length outbound attempt, whether it
  succeeds or is abandoned.

## Assumptions

- **One call at a time per line stays the rule.** This feature does not enable
  handling an incoming and an outgoing call simultaneously on the same line.
  Making that possible is a substantially larger redesign and is out of scope.
- **An outbound attempt in flight counts as busy.** An incoming call during
  that window is refused outright rather than held in case the attempt fails
  and frees the line moments later (decided 2026-08-07). Holding it would
  rescue the short-failure case, but at the cost of a held request with its own
  deadline and staleness handling — and the caller waits either way. Refusing
  promptly is honest and matches how every other busy case behaves.
- **The existing timeout values are correct and stay unchanged.** The fifteen-
  second, sixty-second and five-second waits were each tuned against live
  carrier behaviour; this feature makes them interruptible, not shorter.
- **Roughly five seconds is a good enough detection granularity** on the
  carrier-facing side, bounded by the read interval already used on that
  connection. Shortening that interval affects every read on the connection and
  is a separate, riskier change, out of scope here.
- **The telephone-facing side can poll much faster** — it already polls an
  established call several times a second — so end-to-end detection is expected
  to be dominated by the carrier-facing side's interval.
- **A lost connection between the two halves means abandonment.** There is no
  attempt to distinguish a deliberate hangup from a dropped connection; both
  mean the originating side is gone.
- **Registration renewal is unaffected.** It has roughly five minutes of
  headroom against an eighty-second window and is not part of this gap.
- **Verification is primarily automated.** Live hardware confirmation is
  desirable but the acceptance scenarios are expected to be reproducible with
  the project's existing test doubles for the carrier and the telephone side.

## Out of Scope

- Handling an incoming call and an outgoing call concurrently on one line.
- Changing any of the carrier-facing timeout values.
- Reducing the underlying per-read interval on the carrier connection.
- Circuit-switched (non-VoWiFi/VoLTE) outbound calling, which does not use this
  origination path.
