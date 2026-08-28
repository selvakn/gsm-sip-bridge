# Feature Specification: Offerless Call Answering and Multi-Part SMS Reassembly

**Feature Branch**: `047-offerless-invite-sms-reassembly`
**Created**: 2026-08-28
**Status**: Draft
**Input**: User description: "in @docs/plans/mt-conformance-findings.md, prepare for - SDP-04 — offerless INVITE handling. and SMS-05 — concatenated SMS reassembly."

Two findings carried over from the terminating-side conformance review
(`docs/plans/mt-conformance-findings.md`, batch 6, deferred), bundled into one
spec because both were deferred for the same reason at the time: each needs
new state the system doesn't hold today, rather than a local fix to existing
logic. They are otherwise independent — a network-facing call-setup gap
(SDP-04) and a messaging-completeness gap (SMS-05) — and are written up as two
separately testable stories below.

## Clarifications

### Session 2026-08-28

- Q: How long should the system hold an incomplete multi-part text message (missing at least one part) before giving up on it and falling back to delivering the parts it did receive? → A: 3 minutes.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Answer a call that arrives with no media description (Priority: P1)

Today, when the network places a call to this line without stating up front
what kind of media it wants to send (an "offerless" call setup — legal under
the call-signaling standard, and used by some networks/devices as their
normal way of placing a call), the system cannot make sense of the request
and the call fails to connect. The caller experiences this exactly like a
line that doesn't answer at all — no ringing, or ringing that never resolves
into a live call — even though the line is otherwise working normally.

**Why this priority**: A call that can never connect is the most severe
possible failure this line can have — worse than a call with degraded audio
quality or a missing status report, because the two parties never get to
talk at all. Any network or device that uses this call-setup style makes the
line effectively unreachable from it.

**Independent Test**: Place a call to the line using a caller that is known
to omit its media description on the initial request. Confirm the call rings
and, once answered, carries two-way audio — the same outcome as an ordinary
call that does state its media up front.

**Acceptance Scenarios**:

1. **Given** the line is idle and registered, **When** a call arrives with no
   media description on the initial request, **Then** the line rings exactly
   as it would for an ordinary call, and the caller is not left with dead air
   or an immediate failure.
2. **Given** such a call has been answered, **When** the caller's device
   states what media it actually wants to send, **Then** the call proceeds
   with working two-way audio, negotiated the same way an ordinary call's
   media is negotiated today.
3. **Given** such a call has been answered, **When** the caller's device
   states media this line has no way to support (e.g., no audio format in
   common), **Then** the call is ended with a clear, explicit failure — not
   left connected with no audio, and not left ringing indefinitely.
4. **Given** an ordinary call that already states its media up front,
   **When** it arrives, **Then** it is handled exactly as it is today — this
   capability only changes what happens for the offerless case.

---

### User Story 2 - Deliver a multi-part text message as one complete message (Priority: P2)

Today, when a text message longer than one part arrives (the sender's device
having split it into several linked parts), each part is delivered
separately, labelled with its position ("part 2 of 3") but not combined —
so the message reads as several disconnected fragments instead of the single
message the sender actually typed and sent.

**Why this priority**: The message content still arrives today (each part is
delivered, just unassembled), so this is a completeness/readability gap
rather than a lost-communication failure — lower severity than User Story 1,
where nothing gets through at all.

**Independent Test**: Send a text message long enough that a sending device
splits it into multiple linked parts. Confirm the line ends up delivering one
complete message containing the full original text, in the correct order,
rather than separate fragments.

**Acceptance Scenarios**:

1. **Given** the line is idle, **When** all parts of a multi-part message
   arrive, regardless of the order they arrive in, **Then** the line
   delivers one complete message with the parts joined in their correct
   original order.
2. **Given** two different multi-part messages are in flight to the line at
   the same time (from the same sender or different senders), **When** their
   parts interleave with each other, **Then** each message is reassembled
   separately and correctly — parts from one message are never joined into
   the other.
3. **Given** an ordinary, single-part message arrives, **When** it is
   received, **Then** it is delivered immediately, exactly as it is today —
   this capability only changes handling of messages that are split into
   parts.
4. **Given** a multi-part message has some but not all of its parts
   delivered, **When** 3 minutes pass with no further parts arriving,
   **Then** the line does not hold those parts forever — it gives up on
   that incomplete message rather than accumulating an unbounded backlog.

### Edge Cases

- What happens if the caller abandons an offerless call (hangs up or cancels)
  before ever stating what media it wants to send? The line must not treat
  the call as connected, and must not leave any resources behind as if it
  were still ringing or in progress.
- What happens if the caller's device, after being offered media by this
  line, never responds with its own media description at all? The line must
  eventually give up on the call rather than waiting indefinitely, the same
  way it already times out an ordinary call the far end never answers.
- What happens to the delivery acknowledgment owed to the network for each
  individual part of a multi-part message? Each part must still be
  acknowledged as it is received today, independent of whether or when the
  complete message is assembled — a sender that is waiting on delivery
  confirmation for a specific part must not be kept waiting on reassembly.
- What happens if a part of a multi-part message is received more than once
  (a retransmission)? It must not be treated as a new, additional part, and
  must not cause the reassembled message to contain duplicated text or ever
  count twice toward "all parts received."
- What happens if a part of a multi-part message never arrives at all (lost
  in transit)? The message is never delivered as "complete" for the missing
  part; per the timeout behavior above, the line eventually gives up on it
  rather than holding it forever.
- What happens if the multi-part message metadata itself is inconsistent or
  malformed (e.g., a claimed total part count of zero, or a part number
  higher than the claimed total)? The line must not crash or misbehave; at
  minimum it should fall back to today's per-part delivery for that message
  rather than lose it silently.

## Requirements *(mandatory)*

### Functional Requirements

**Offerless call answering (SDP-04)**

- **FR-001**: The system MUST recognize an inbound call request that carries
  no media description, and MUST NOT treat it as a malformed or unanswerable
  request the way it does today.
- **FR-002**: The system MUST respond to an offerless call request by
  proposing its own media — the same audio capabilities it would otherwise
  answer with — instead of declining the call or failing silently.
- **FR-003**: The system MUST let the call ring and be answered exactly as an
  ordinary (non-offerless) call does today, from the caller's perspective.
- **FR-004**: Once the caller's device states what media it actually wants to
  send, the system MUST use that to complete the call setup, resulting in
  working two-way audio when the two sides have a compatible audio format in
  common.
- **FR-005**: If the caller's device ultimately states media this system has
  no compatible audio format for, the system MUST end the call with an
  explicit, honest failure indication — never leave the call connected
  without working audio, and never leave it silently ringing forever.
- **FR-006**: If the caller's device never states its media at all within a
  bounded waiting period, the system MUST give up on the call rather than
  waiting indefinitely.
- **FR-007**: The system MUST continue to handle a call that already states
  its media on the initial request exactly as it does today — this
  capability is additive, not a change to the existing path.
- **FR-002a**: The media this system proposes for an offerless call MAY be
  limited to audio formats only — it is not required to also offer
  in-call touch-tone (DTMF) signaling or call-quality reporting on this
  specific path in this iteration. This is a deliberate, recorded scope
  cut (this system's existing mechanism for proposing its own media,
  reused here, predates and does not yet cover either of those two
  things), not a claim that they don't matter — tracked as a known
  residue, the same way an earlier, related gap (RTP-01's own scope cut
  for a different call path) was tracked rather than silently implied
  complete.

**Multi-part SMS reassembly (SMS-05)**

- **FR-008**: The system MUST recognize when a received text message is
  marked as one part of a larger, multi-part message, distinguishing it from
  an ordinary single-part message.
- **FR-009**: The system MUST hold the parts of a multi-part message it has
  received so far, until either every part has arrived or the message is
  given up on (FR-013).
- **FR-010**: Once every part of a multi-part message has been received, the
  system MUST combine them, in their correct original order, into one
  complete message, and deliver that single complete message rather than the
  individual parts.
- **FR-011**: The system MUST correctly tell apart parts belonging to
  different multi-part messages that are in flight at the same time,
  including two such messages from the same sender, so that parts are never
  joined into the wrong message.
- **FR-012**: The system MUST continue to send the network the same
  per-part delivery acknowledgment it sends today for each part, independent
  of whether or when that part's message is fully reassembled.
- **FR-013**: The system MUST give up on an incomplete multi-part message
  after 3 minutes with no further parts arriving, so that a message missing
  its last part does not consume resources indefinitely.
- **FR-014**: The system MUST NOT create a duplicate entry, nor double-count
  toward completion, when a part it has already received arrives again.
- **FR-015**: The system MUST continue to deliver an ordinary, single-part
  message immediately, exactly as it does today — this capability is
  additive to the existing single-part path.
- **FR-016**: When a multi-part message's own metadata is inconsistent or
  malformed in a way that makes correct reassembly impossible, the system
  MUST fall back to delivering what it has (today's per-part behavior)
  rather than losing the content silently.

### Key Entities *(include if feature involves data)*

- **Pending Inbound Call**: A call this line has begun ringing/answering
  before knowing what media the caller actually wants to send. Tracks the
  call's identity and the media this line has proposed, until either the
  caller's own media description arrives (letting the call complete) or the
  call is abandoned/times out.
- **Multi-Part Message Buffer**: The set of parts received so far for one
  particular multi-part text message, identified by the combination of
  sender, the message's own part-grouping reference, and its claimed total
  part count. Tracks which part positions have arrived and how long the
  buffer has been waiting, until it either completes (all parts present),
  is given up on (FR-013), or is superseded by a new message reusing the
  same identifying combination.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A call placed with no upfront media description completes and
  carries two-way audio at the same success rate as an equivalent ordinary
  call, where the caller's device is otherwise capable of a compatible audio
  format.
- **SC-002**: An offerless call that cannot ultimately negotiate a compatible
  audio format ends with an explicit failure signal 100% of the time — never
  a silent hang or an indefinitely ringing/connected-but-silent call.
- **SC-003**: 100% of multi-part text messages whose every part is
  successfully received are delivered as a single, complete, correctly
  ordered message rather than as separate unlabelled fragments.
- **SC-004**: An incomplete multi-part message (missing one or more parts) is
  cleared from the system within 3 minutes of its last-received part rather
  than persisting indefinitely, and never blocks delivery of any unrelated
  message in the meantime.
- **SC-005**: Neither capability changes the observed behavior of the
  existing, already-working cases (an ordinary call that states its media
  up front; an ordinary single-part text message) — both continue to work
  exactly as they do before this feature ships.

## Assumptions

- This line handles one call at a time (its existing architecture), so only
  one offerless call can be pending at once — no new concurrency behavior is
  required beyond what call handling already provides today.
- A message reasonably held while waiting on the remaining parts of a
  multi-part text, and a call reasonably held while waiting on the far end's
  media description, do not need to be preserved across a restart of this
  system — consistent with the fact that no other in-progress call or
  registration state survives a restart today either. A restart mid-wait may
  lose that specific pending call or partially-received message.
- The exact bounded wait time used for "how long to hold an unanswered
  offerless call" is an implementation choice, not fixed by this
  specification — it should be long enough to cover normal network/device
  delay and short enough that it doesn't meaningfully change how quickly a
  genuinely stuck call is noticed and cleared. (The equivalent value for an
  incomplete multi-part message is fixed at 3 minutes — see FR-013/SC-004.)
- Reassembled multi-part messages are delivered to the same destination and
  by the same means as an ordinary single-part message is today — this
  feature changes what is delivered (one complete message instead of several
  fragments), not where or how it is delivered.
- A multi-part message that never completes (per FR-013/FR-016) is not
  silently discarded outright — today's existing per-part delivery remains
  the fallback, preserving at least partial visibility into content that
  can't be fully reassembled.
