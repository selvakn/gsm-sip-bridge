# Feature Specification: Honour what the far side actually offered in SDP

**Feature Branch**: `043-honour-sdp-negotiation`
**Created**: 2026-08-27
**Status**: Draft
**Input**: User description: "Batch 4 conformance fixes: SDP-01 (media lines other than the first audio stream are silently dropped instead of declined per RFC 3264 section 6), SDP-02 (direction attributes sendonly/recvonly/inactive are parsed nowhere and the answer always hardcodes sendrecv), SDP-03 (the m= line's transport profile token, e.g. RTP/AVP vs RTP/SAVP, is never checked). MT-05 (session timers advertised, never honoured) is resolved as a documentation/test-only item — post-MT-10 the inbound side no longer advertises timer support at all, and RFC 4028 section 9 explicitly permits a UAS to omit Session-Expires when it doesn't want the extension, which is exactly today's behavior and is spec-legal, not a bug."

## Why this exists

This bridge answers exactly one audio stream per call — an intentional,
stated design choice, not an oversight. The problem batch 4 addresses is
what happens around the edges of that choice: when an offer contains
something the bridge doesn't (and won't) act on — a second media section, a
stated direction, an unrecognized transport — the bridge today either
silently discards the information or answers as if the offer had said
something it didn't. Three consequences, found during the same
protocol-conformance review as batches 1-3
(`docs/plans/mt-conformance-findings.md`, batch 4):

- An offer with more than one media section (a second audio line, or a
  video/text/application section) has every section but one thrown away
  during parsing, with no trace it ever existed. The answer never
  acknowledges the other sections at all — not accepted, not declined,
  simply absent, which is not a valid answer to that offer.
- An offer that states a direction (`sendonly`, `recvonly`, `inactive`) is
  always answered `sendrecv` regardless — the answer doesn't describe what
  actually happens, it describes the bridge's default assumption.
- An offer's transport profile — the token that says whether the media is
  plain RTP, secure RTP, or something else — is never read. An offer this
  bridge cannot actually service that way is accepted as if it could be.

None of these change what the bridge actually does with media (it remains a
single-stream, plain-RTP, audio-only relay) — they change whether the SDP
answer it sends *honestly describes* that. This is the same principle
already applied in batches 2 and 3 (MT-07, MT-02, MT-10): say only what is
true about what this bridge can do, rather than accepting or ignoring
something in a way that misrepresents it.

A fourth finding, MT-05 (session timers advertised but never honoured), is
addressed by this feature but requires no behavior change: the earlier
MT-10 fix already stopped the inbound side from advertising session-timer
support at all, and the applicable specification (RFC 4028) explicitly
permits an endpoint to simply not use the extension. This feature confirms
and pins that as correct, intentional behavior rather than leaving it as an
apparently-still-open finding.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - An offer with more than one media section gets a real answer to all of it (Priority: P1)

A caller's device or network sends an offer containing the bridge's
supported audio section plus one or more other media sections (another
audio line, video, text, or anything else). The call proceeds normally on
the audio the bridge handles, and the answer explicitly and correctly
declines every other section, in the same order the offer listed them —
rather than answering as if those sections had never been offered at all.

**Why this priority**: An answer that omits sections the offer included is
not a valid SDP answer under RFC 3264 — a strict far-end implementation
could reasonably treat it as malformed and refuse to proceed, meaning this
is a correctness gap that can block call setup entirely for a compliant
peer, not just a cosmetic one.

**Independent Test**: Send an offer with two media sections — the audio
section this bridge supports, plus a second section of any other kind —
and confirm the call is answered normally on the audio, while the answer
also contains an explicit decline for the second section in its original
position. Confirm this is independent of direction and transport-profile
handling (User Stories 2 and 3): it holds even when the extra section and
the audio section both use ordinary values for those.

**Acceptance Scenarios**:

1. **Given** an offer with one audio section and one non-audio section,
   **When** the bridge answers, **Then** the audio section is negotiated
   normally and the answer also includes an explicit decline for the
   non-audio section, in the same relative order as the offer.
2. **Given** an offer with two audio sections, **When** the bridge answers,
   **Then** the first audio section is the one actually negotiated, and the
   second is explicitly declined — never silently replaced by the second
   without a trace of the first, and never negotiated on the second while
   ignoring the first.
3. **Given** an offer with only the one audio section this bridge already
   supports, **When** the bridge answers, **Then** the answer is unchanged
   from today's behavior — no decline lines appear for something that was
   never offered.

---

### User Story 2 - The answer says what the call will actually do, not a fixed default (Priority: P2)

An offer states that the sender will only send, only receive, or send
nothing at all (`sendonly`, `recvonly`, `inactive`) for the audio the
bridge handles. The answer reflects that honestly instead of always
claiming full two-way media regardless of what was actually offered.

**Why this priority**: This affects how a compliant peer interprets the
call's media state (for example, a caller placed on hold before the bridge
even answers) — a wrong claim here is a real interoperability defect, but
it's narrower in practical impact than User Story 1's answer-validity gap,
since no carrier here has been observed sending it, and it shares the same
underlying mechanism.

**Independent Test**: Send an offer whose audio section states each of
`sendonly`, `recvonly`, and `inactive` in turn, and confirm the answer's
own direction reflects the correct counterpart in each case, rather than
always the same fixed value. Separately confirm an offer stating ordinary
two-way media, or stating nothing at all, still gets today's answer.

**Acceptance Scenarios**:

1. **Given** an offer whose audio section is marked "send only," **When**
   the bridge answers, **Then** the answer's own direction reflects that it
   will only receive.
2. **Given** an offer whose audio section is marked "receive only,"
   **When** the bridge answers, **Then** the answer's own direction
   reflects that it will only send.
3. **Given** an offer whose audio section is marked "inactive," **When**
   the bridge answers, **Then** the answer's own direction reflects that
   as well.
4. **Given** an offer whose audio section states two-way media, or states
   no direction at all, **When** the bridge answers, **Then** the answer
   states two-way media, exactly as today.

---

### User Story 3 - An offer using a transport this bridge cannot service is refused honestly (Priority: P3)

An offer's audio section names a transport profile — the mechanism by
which the media itself would be protected or formatted — that this bridge
does not implement (for example, one requiring media encryption). The
bridge declines the request with a response indicating it cannot be
serviced, instead of accepting the offer and then relaying using a
transport the offer never actually asked for.

**Why this priority**: No carrier or device this bridge currently talks to
has been observed offering anything but the one transport it already
handles, so this has the least immediate impact of the three — but
accepting an offer's audio section while silently ignoring what it said
about the transport is a real correctness gap for any peer that does send
one, worth closing now that the same review already covers the other two.

**Independent Test**: Send an offer whose audio section names a transport
profile the bridge does not support, and confirm it is declined with a
response indicating the request cannot be serviced — distinct from an
ordinary successful answer, and distinct from the "no acceptable codec"
decline already given for an unsupported codec list.

**Acceptance Scenarios**:

1. **Given** an offer whose audio section names an unsupported transport,
   **When** the bridge processes it, **Then** the call is declined with a
   response indicating the request cannot be serviced, and no answer is
   produced as if the transport had matched.
2. **Given** an offer whose audio section names the transport this bridge
   already supports, **When** the bridge processes it, **Then** behavior is
   unchanged from today.

---

### Edge Cases

- **An offer with a non-audio section but no audio section at all.**
  Already out of scope for call setup today (the bridge requires an audio
  section to answer anything) — this feature does not change that; the
  non-audio section(s) are simply irrelevant to a call that can't be
  answered anyway.
- **An offer's audio section states an unsupported transport *and* the
  offer also has other media sections.** The transport check on the audio
  section takes precedence — the whole request is declined per User Story
  3, since the section this bridge would have negotiated isn't usable
  regardless of what the other sections say.
- **A subsequent request that would change the direction or add media
  sections mid-call (a re-INVITE).** Out of scope here — mid-call
  renegotiation of any kind is already declined honestly by the prior
  batch (MT-02), and this feature only concerns the initial offer/answer.
- **Session-timer headers on an inbound request (MT-05).** No offer or
  header content changes this bridge's behavior: it already never
  advertises the extension and never claims support for it, which the
  applicable specification treats as a fully valid, unremarkable outcome —
  this feature adds a check confirming that stays true, not a new
  behavior.

## Requirements *(mandatory)*

### Functional Requirements

#### Multiple media sections

- **FR-001**: The system MUST negotiate exactly the one audio section it
  already supports when an offer contains more than one media section,
  selecting the first audio section present.
- **FR-002**: The system MUST explicitly decline every media section in
  the offer other than the one it negotiates — including a second audio
  section — rather than omitting it from the answer entirely.
- **FR-003**: Declined sections MUST appear in the answer in the same
  relative order as they appeared in the offer, alongside the one
  negotiated section.
- **FR-004**: An offer containing only the one audio section this bridge
  already supports MUST be answered exactly as it is today, with no
  decline lines added.

#### Direction

- **FR-005**: The system MUST read the offered audio section's stated
  direction (send-only, receive-only, inactive, or two-way/unstated) and
  reflect the correct corresponding direction in its own answer, rather
  than always answering two-way.
- **FR-006**: This requirement governs only what the answer states about
  direction; it does not require the system to change how it actually
  sends or receives media based on that value.

#### Transport profile

- **FR-007**: The system MUST check the offered audio section's transport
  profile and decline the request, with a response indicating it cannot be
  serviced, when that transport is not one this bridge implements.
- **FR-008**: An offer using the transport this bridge already implements
  MUST be processed exactly as it is today.

#### Session timers (MT-05)

- **FR-009**: The system MUST continue to never claim session-timer
  support in a response to an inbound request, and MUST NOT begin doing so
  as a side effect of this feature's other changes.

### Key Entities

- **Media section**: One `m=`-delimited block of a session description —
  the audio one this bridge negotiates, and any other kind or duplicate
  the offer includes.
- **Direction**: What a media section states about which way media will
  flow for it — two-way, one of the two one-way states, or neither
  direction.
- **Transport profile**: What a media section states about the underlying
  transport its media uses — the one this bridge implements, or anything
  else.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An offer with multiple media sections always receives an
  answer with a matching entry for every section the offer had — never
  fewer — 100% of the time across offers with zero, one, and multiple
  extra sections.
- **SC-002**: An answer's stated direction always matches the correct
  counterpart of what the offer's audio section actually stated, across
  all four direction states.
- **SC-003**: An offer naming an unsupported transport is never answered
  as if negotiation succeeded — it always receives a decline distinct from
  a successful answer.
- **SC-004**: An ordinary offer — one audio section, two-way, the
  supported transport — continues to be answered identically to today's
  behavior, with zero regression across the existing call-setup test
  coverage.

## Assumptions

- This bridge remains a single-audio-stream relay; this feature does not
  add support for actually carrying a second media stream (audio, video,
  or otherwise) — declining extra sections honestly is the correct
  outcome, not an interim step toward relaying them.
- Reflecting a one-way or inactive direction in the answer is a signaling
  correctness fix only; this feature does not require the relay to
  actually suppress sending or receiving media to match it, since no
  carrier here has been observed depending on that behavior and building
  it is a materially larger change.
- No carrier or device this bridge currently operates against has been
  observed sending more than one media section, a non-default direction,
  or an unsupported transport profile on an initial offer — these are
  correctness/interoperability fixes for gaps that haven't caused a live
  incident yet, matching the same posture already taken for MT-02's
  mid-call renegotiation handling.
- MT-05 requires no behavior change; this feature only adds coverage
  confirming the already-correct, already-shipped state (post-MT-10)
  stays that way.
