# Feature Specification: Complete the media contract on the relay legs

**Feature Branch**: `044-complete-media-contract`
**Created**: 2026-08-27
**Status**: Draft
**Input**: User description: "Batch 5 conformance fixes: RTP-03 (the pass-through relay forwards telephone-event under the wrong payload type), RTP-04 (no SSRC continuity check on receive), SDP-06's ptime half (the offer's own ptime is never read; the answer hardcodes 20ms regardless). RTP-01 (no RTCP despite declaring its bandwidth) and SDP-06's a=rtcp half are explicitly deferred to their own future feature — building real RTCP touches all three relay call sites and needs new state (octet counters, exposed SSRC, a per-call timer with socket access) well beyond this batch's scope, per user decision."

## Why this exists

Batches 1-4 fixed how this bridge negotiates and signals a call. This batch
looks at what happens to the media itself once a call is up, on the two
places media actually crosses a boundary: the plain byte-pump relay used
when both legs already speak the same codec, and the packetization the
answer states versus what actually happens. Three gaps, found during the
same protocol-conformance review (`docs/plans/mt-conformance-findings.md`,
batch 5):

- The pass-through relay forwards every RTP packet's payload type
  unchanged, on the (correct) assumption that both legs' *audio* codec was
  negotiated to match. But each leg's DTMF (`telephone-event`) payload type
  is chosen independently by that leg's own SDP answer — nothing keeps the
  two in sync, so a keypress relayed verbatim can arrive on a payload type
  the receiving leg never agreed DTMF would use.
- Nothing on the receive side of a relay notices if a stream's SSRC
  changes mid-call — a legitimate signal (per RFC 3550) that the source
  restarted, but currently invisible either way.
- An offer's own packetization interval (`ptime`) is never read at all.
  On investigation this turned out not to be the bug it first appeared:
  the offer's `ptime` describes what *the offer's own owner* intends to
  send, not a request for our answer to match — and since this bridge's
  own packetization is a fixed, codec-level constant, the answer's
  existing fixed value is already the honest thing to state. This finding
  is resolved by confirming that, not by echoing the offer's value (see
  `research.md` Decision 4 for the full reasoning).

A fourth finding from the same review batch, RTP-01 (no RTCP sent or
received despite its bandwidth being declared), and the half of a fifth
finding (SDP-06's `a=rtcp` explicit-port attribute) that exists only to
support RTP-01, are explicitly **out of scope** for this feature. Building
real RTCP needs new call-wide state this bridge doesn't have anywhere
today (send-side octet counts, a stable exposed SSRC, a per-call timer
with access to the live RTP socket, and a synchronous teardown hook to
send a final packet) across all three places media is relayed — a
materially larger and riskier undertaking than anything else in this
batch, deferred to its own dedicated feature by explicit decision.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A DTMF keypress on a pass-through call reaches the far side recognizably (Priority: P1)

A call where both legs negotiated the same audio codec (so the relay
forwards payloads unchanged) is in progress, and a keypress arrives from
one side. It must be recognizable as DTMF to the leg that receives it,
even when that leg negotiated a different payload-type number for DTMF
than the sending leg did.

**Why this priority**: A keypress that fails to register is an immediate,
noticeable failure for the person using it (an IVR menu, a voicemail PIN)
— this bridge's own batch-1 fix (RTP-02) already established that DTMF has
to actually work on a transcoded call; this closes the same gap on the
pass-through path, which was untouched by that fix.

**Independent Test**: With a pass-through call up (both legs on the same
audio codec) where the two legs negotiated *different* DTMF payload-type
numbers, send a keypress from one leg and confirm it arrives on the
receiving leg's own negotiated DTMF payload type, not the sender's.
Separately, confirm the audio payload itself is forwarded exactly as
before — this change touches only how a DTMF packet's payload type is
labeled, nothing else about the relay.

**Acceptance Scenarios**:

1. **Given** a pass-through call where the two legs negotiated different
   DTMF payload-type numbers, **When** a keypress is relayed from one leg
   to the other, **Then** it arrives labeled with the *receiving* leg's own
   negotiated DTMF payload type.
2. **Given** a pass-through call where the two legs happen to negotiate the
   *same* DTMF payload-type number, **When** a keypress is relayed,
   **Then** it is forwarded exactly as today (no relabeling needed, none
   applied).
3. **Given** an ordinary audio packet (not DTMF) on a pass-through call,
   **When** it is relayed, **Then** it is forwarded byte-for-byte
   unchanged, exactly as today.

---

### User Story 2 - A source restart mid-call is visible, not silently absorbed (Priority: P2)

A stream's SSRC changes partway through a call — a legitimate signal that
the sending side restarted its RTP source. Today nothing notices; the
relay keeps forwarding regardless, with no way to later tell that it
happened.

**Why this priority**: This is an observability gap, not a call-breaking
one — nothing in this bridge's own relay logic currently depends on SSRC
continuity to function, so nothing is *broken* by its absence today. It's
worth closing because a silent, unlogged source change is exactly the kind
of thing worth being able to see after the fact (a real restart, a
misbehaving far end, or something less benign), and no legitimate call
should be disrupted just to gain that visibility.

**Independent Test**: With a call in progress, change the SSRC on one
relayed stream mid-call (simulating a source restart) and confirm it's
recorded/logged as an SSRC change, while audio continues to flow
uninterrupted through the relay exactly as before.

**Acceptance Scenarios**:

1. **Given** a call in progress, **When** a relayed stream's SSRC changes,
   **Then** the change is recorded, identifying which leg it was seen on.
2. **Given** a call in progress, **When** a relayed stream's SSRC changes,
   **Then** the relay continues forwarding that stream's packets without
   interruption — an SSRC change is never treated as a reason to drop
   packets or end the call.
3. **Given** a call whose SSRC never changes (the ordinary case), **When**
   it runs to completion, **Then** nothing about its behavior changes from
   today.

---

### User Story 3 - The answer's stated packetization stays honest regardless of what was offered (Priority: P3)

An offer states its own packetization interval (`ptime`) — a description
of what *the offer's own owner* intends to send, not a request for what
our answer should claim. This bridge's own packetization is fixed
(a stable, codec-level constant); the answer must keep stating that true
value regardless of what any given offer's `ptime` says, rather than
echoing a value that would misrepresent our own actual behavior.

**Why this priority**: No carrier or device this bridge talks to has been
observed stating a non-default packetization, so this has the least
immediate impact of the three. On investigation (see this feature's
`research.md`, Decision 4), the original framing of this finding — echo
the offer's value — turned out to be the wrong fix: this bridge's
packetization does not vary per call, so echoing an offer's stated value
would make the answer say something untrue whenever that value differs
from what actually happens. The correct outcome is confirming today's
fixed statement stays fixed, not changing it.

**Independent Test**: Send an offer stating a packetization interval other
than this bridge's own fixed value, and confirm the answer still states
its own true value, unaffected by what the offer requested.

**Acceptance Scenarios**:

1. **Given** an offer stating a packetization interval different from this
   bridge's own, **When** the bridge answers, **Then** the answer states
   its own true value, not the offer's.
2. **Given** an offer stating no packetization interval at all, **When**
   the bridge answers, **Then** the answer states the existing value,
   unchanged from today.

---

### Edge Cases

- **A transcoded call (not pass-through).** User Story 1 applies only to
  the pass-through relay path — batch 1's RTP-02 fix already ensures the
  transcoding path forwards DTMF to the far leg's own negotiated payload
  type correctly; this feature does not change that path.
- **An SSRC change on the very first packet of a call.** There is no prior
  value to compare against; this is simply the stream's starting SSRC, not
  a change, and must not be recorded as one.
- **An offer's `ptime` value that is unusually large or small.** Not
  relevant: the answer states this bridge's own true value regardless of
  what the offer's `ptime` says, so nothing about the offer's specific
  value changes the outcome.

## Requirements *(mandatory)*

### Functional Requirements

#### DTMF payload-type correctness on the pass-through relay

- **FR-001**: The pass-through relay MUST relabel a relayed DTMF packet's
  payload type to the receiving leg's own negotiated DTMF payload type
  when the two legs' negotiated DTMF payload types differ.
- **FR-002**: The pass-through relay MUST NOT alter the payload type of a
  packet that is not DTMF, or of a DTMF packet when both legs' negotiated
  DTMF payload types already match.
- **FR-003**: A leg that did not negotiate a DTMF payload type at all MUST
  NOT have DTMF relabeling applied toward it (there is nothing to relabel
  to); behavior for that direction is unchanged from today.

#### SSRC visibility

- **FR-004**: The system MUST record when a relayed stream's SSRC changes
  mid-call, identifying which leg the change was observed on.
- **FR-005**: The system MUST NOT drop packets, end the call, or otherwise
  interrupt media flow solely because a stream's SSRC changed.
- **FR-006**: The first packet observed for a stream MUST NOT itself be
  recorded as an SSRC change.

#### Packetization interval

- **FR-007**: The system MUST state its own true, fixed packetization
  interval in the answer regardless of what any given offer's own
  packetization interval says — the answer's value describes this
  bridge's own behavior, not the offer's.
- **FR-008**: An offer that states no packetization interval at all MUST
  continue to receive the same value in the answer, unchanged from today.

### Key Entities

- **Pass-through relay**: The relay path used when both legs already
  share the same audio codec, which forwards RTP payloads without
  decoding/re-encoding them.
- **DTMF payload type**: The payload-type number a leg's own SDP answer
  assigned to `telephone-event`, independently chosen per leg.
- **SSRC**: The synchronization source identifier carried on every RTP
  packet, expected to stay constant for one continuous stream.
- **Packetization interval (`ptime`)**: What an offer or answer states
  about how much audio, in milliseconds, each RTP packet is expected to
  carry.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A DTMF keypress relayed on a pass-through call always
  arrives on the receiving leg's own negotiated DTMF payload type, across
  every combination of matching and differing negotiated DTMF payload
  types.
- **SC-002**: An SSRC change on any relayed stream is always recorded,
  identifying the affected leg, with zero packets dropped as a result.
- **SC-003**: An answer's stated packetization interval always matches
  this bridge's own true, fixed value, regardless of what any given offer
  stated — 100% of the time, across offers with and without their own
  `ptime`.
- **SC-004**: An ordinary pass-through call (matching DTMF payload types,
  no SSRC change, no explicit `ptime`) continues to behave identically to
  today, with zero regression across existing relay test coverage.

## Assumptions

- RTP-01 (RTCP) and the `a=rtcp` half of SDP-06 are out of scope for this
  feature, deferred to their own dedicated future feature by explicit
  decision — building real RTCP requires call-wide state (send-side octet
  counts, an exposed/stable SSRC, a per-call timer with live socket
  access, a synchronous teardown hook) that doesn't exist anywhere in this
  codebase today, and touches all three relay call sites (inbound,
  outbound, veth) across both relay implementations — a materially larger
  and riskier change than the rest of this batch.
- SSRC continuity is handled as an observability improvement only in this
  feature (recorded, not enforced) — nothing in this bridge's relay logic
  today depends on SSRC continuity to function correctly, so there is no
  existing behavior to "fix" beyond making a change visible.
- No carrier or device this bridge currently operates against has been
  observed negotiating different DTMF payload-type numbers on its two
  legs, changing SSRC mid-call, or requesting a non-default `ptime` — all
  three are correctness/interoperability fixes for gaps that haven't
  caused a live incident yet, matching the posture already taken for
  earlier batches' least-observed findings.
