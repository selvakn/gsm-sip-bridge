# Feature Specification: Honour locally-confirmable SDP QoS preconditions

**Feature Branch**: `048-sdp-qos-preconditions`
**Created**: 2026-08-28
**Status**: Draft
**Input**: User description: "MT-06: SIP/SDP QoS preconditions (RFC 3312) are not implemented. Today, any inbound INVITE with `Require: precondition` gets an unconditional 420 Bad Extension from the existing MT-03 gate — spec-legal but means any carrier that actually requires preconditions cannot get through at all. This bridge has no real network-level QoS resource reservation to make on its own local segment (media is a local veth relay; RTP starts flowing as soon as the answer is sent), so its local-segment QoS status is always trivially already met. A `local`-only precondition can be confirmed unilaterally and immediately; an `e2e` (end-to-end) precondition genuinely requires hearing the caller's own segment status back via UPDATE or 100rel, neither of which this codebase has built, and building them would be a materially larger, currently unjustified subsystem for a scenario no carrier here has ever been observed producing. Goal: support `local`/unqualified preconditions inline, correctly continue declining what cannot be honestly confirmed without peer coordination."

## Why this exists

MT-03's existing gate refuses `Require: precondition` the same way it
refuses any other `Require:` tag this bridge doesn't implement — a blanket
`420 Bad Extension`, sent before the SDP is even parsed. That is spec-legal
(RFC 3261 §8.2.2.3), but it is also indiscriminate: it treats every shape
of precondition offer identically, including the shapes this bridge is
fully capable of honouring honestly.

RFC 3312 preconditions describe a *segment* (`local`, `remote`, or `e2e`)
and a *strength* (`mandatory`, `optional`, `none`, `failure`, `unknown`).
The segment is what changes the answer here: this bridge's own local
segment is a media relay with no reservation delay — by the time it can
answer at all, its local QoS status is already whatever the offer asked
for. Confirming that costs nothing and requires no coordination with the
far end. An `e2e` segment is different in kind, not degree: honestly
confirming it means finding out whether the *caller's* segment is also
ready, which this bridge has no channel to learn today (UPDATE is an
unimplemented method; reliable provisional responses were deliberately
never built, per MT-04/MT-05's resolution). Claiming an `e2e` segment is
confirmed without that channel would be a false statement in the SDP
answer, not merely an unsupported feature — worse than today's honest
`420`.

So the fix here is the same shape as batches 4 and 6 (SDP-01/02/03,
MT-11/12/13): make the bridge's SIP/SDP responses honestly describe what
it actually does, no more and no less. For preconditions that only name
segments this bridge can truthfully confirm on its own, stop refusing the
call and answer with accurate status. For preconditions that name a
segment this bridge cannot truthfully confirm, keep declining — but only
those, not every offer that merely mentions preconditions at all.

## Clarifications

### Session 2026-08-28

- Q: For an SDP offer with an e2e-segment precondition at `mandatory` strength — which this bridge cannot honestly confirm since it has no UPDATE/100rel channel to learn the caller's own segment status — how should it respond? → A: Decline the call (today's 420-style refusal), since claiming e2e confirmation would be a false statement in the SDP answer.
- Q: Should this feature require a live hardware round (a real carrier call actually exercising `Require: precondition`) before its PR is considered ready, the same gate batches 1-8 used? → A: Yes, but scoped to what's actually reachable — see follow-up.
- Q: Given no carrier can be made to send `Require: precondition` (only Jio itself can trigger `agent::inbound::handle_invite`, and it's never been observed sending this header), what should the hardware-pass gate mean for this feature? → A: Regression-only — deploy to the real Pi and confirm an ordinary real inbound call (no preconditions) still connects and bridges cleanly. The precondition-handling logic itself is verified by unit/fixture tests only, since no real traffic can reach it; this is a proven-no-regression gate, not a proven-new-behavior-works-live gate.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A precondition on this bridge's own segment is honoured instead of refused (Priority: P1)

A carrier sends an inbound INVITE with `Require: precondition` and an SDP
offer whose `a=des:qos` line(s) name the `remote` status type — RFC 3312
§4 defines `local`/`remote` relative to whoever generated the SDP, so a
line the *offerer* (caller) labels `remote` is asking about the far end's
segment, which from the offerer's point of view is this bridge's own
segment; the tag inverts to `local` in this bridge's answer (RFC 3312
§5.2). Today this call is refused outright with `420` before the SDP is
even read. After this feature, the bridge accepts the `Require`, reads the
desired precondition, and answers with `a=curr`/`a=conf` lines (using the
inverted, answer-relative `local` tag) truthfully reporting its own
segment as already met — the call proceeds to ringing and answering
exactly as an equivalent offer without preconditions would.

**Why this priority**: This is the entire reason the finding exists — a
carrier that requires preconditions currently cannot reach this bridge at
all, for a segment this bridge could truthfully confirm at essentially no
cost. Without this, MT-06 stays open no matter what else changes.

**Independent Test**: Send an INVITE with `Require: precondition` and an
offer containing `a=des:qos mandatory remote sendrecv` (and the matching
`a=curr:qos remote none` the offerer sent about what it believes of this
bridge's segment). Confirm the call is **not** declined with `420`, and
that the answer's SDP contains `a=curr`/`a=conf` lines — tagged `local`,
per the offer/answer inversion — reporting this bridge's own segment as
met, with the call otherwise proceeding (ringing, answering, media)
exactly as it would without preconditions.

**Acceptance Scenarios**:

1. **Given** an INVITE with `Require: precondition` and an offer whose
   `a=des:qos` line(s) name the `remote` status type, **When** the bridge
   answers, **Then** the call is not declined for the `precondition`
   extension, and the answer's `local`-tagged lines report this bridge's
   own segment as confirmed.
2. **Given** the same offer but without `Require: precondition` (just
   `Supported: precondition`), **When** the bridge answers, **Then** the
   call proceeds identically — presence of the optional-strength or merely
   supported form was never the thing blocking the call.
3. **Given** an offer with a `remote`-status-type precondition on both
   `e2e`-less `a=des:qos` lines for sendrecv and recvonly directions on the
   audio section, **When** the bridge answers, **Then** each is answered
   with its own accurate `local`-tagged `a=curr` status, consistent with
   the negotiated direction (specs/043 SDP-02).

---

### User Story 2 - An `e2e`-segment mandatory precondition is still honestly declined (Priority: P1)

A carrier sends an inbound INVITE with `Require: precondition` whose offer
requires the `e2e` segment to reach `mandatory` strength before the call
can proceed. This bridge has no way to learn the caller's own segment
status (no UPDATE, no 100rel), so confirming this would be a false
statement in the SDP. The bridge continues to decline this specific case —
but the decline now reflects that this segment genuinely cannot be
honoured, not a blanket refusal of anything mentioning preconditions.

**Why this priority**: Equally load-bearing as User Story 1 — accepting an
`e2e` precondition the bridge cannot actually confirm would be a
regression from today's honest (if overly broad) `420`, trading one
correctness gap for a worse one (a false claim in a SIP response).

**Independent Test**: Send an INVITE with `Require: precondition` and an
offer whose `a=des:qos` line names the `e2e` segment at `mandatory`
strength. Confirm the call is declined, and that declining still happens
for this shape specifically (not merely because `Require: precondition`
was present at all — User Story 1 proves the header alone no longer blocks
a call).

**Acceptance Scenarios**:

1. **Given** an INVITE with `Require: precondition` and an offer whose
   `a=des:qos` line names the `e2e` segment at `mandatory` strength,
   **When** the bridge answers, **Then** the call is declined rather than
   answered with a fabricated confirmation.
2. **Given** an offer with two `a=des:qos` lines — one `remote` mandatory,
   one `e2e` mandatory — **When** the bridge answers, **Then** the call is
   still declined overall (the unconfirmable `e2e` line governs), not
   partially answered.
3. **Given** an offer whose `a=des:qos` line names `e2e` at `optional`
   (not `mandatory`) strength, **When** the bridge answers, **Then** the
   call proceeds — an optional precondition the bridge cannot confirm is
   truthfully reported as not-yet-met, which RFC 3312 permits the call to
   proceed past regardless.

---

### User Story 3 - The offerer's own segment is mirrored, never asserted (Priority: P3)

An offer's `a=des:qos`/`a=curr:qos` lines may also name the `local` status
type — the offerer's (caller's) own description of *its own* segment,
which inverts to `remote` in this bridge's answer. This bridge has no
basis to confirm or deny something the far end is stating about itself, so
these lines are neither used to block the call nor rewritten with a status
this bridge invented — the offer's own `a=curr:qos local` claim (if any) is
mirrored through as the answer's `a=curr:qos remote` line, inverted but
not altered, the same way an offer detail outside this bridge's
negotiation scope already passes through unassessed elsewhere in `sdp.rs`.

**Why this priority**: Narrower than User Stories 1 and 2 — the offerer's
own segment, on its own, doesn't change whether *this* bridge can answer;
getting it wrong is a smaller, cosmetic-correctness risk rather than a
call-blocking one. Included for completeness so the three segment types
aren't left with two handled and one silently unhandled.

**Independent Test**: Send an offer whose only `a=des:qos` line names the
`local` status type (no `remote` or `e2e` line present). Confirm the call
proceeds normally and the answer's `remote`-tagged line mirrors whatever
the offer's own `a=curr:qos local` line said, never a status this bridge
invented.

**Acceptance Scenarios**:

1. **Given** an offer with only a `local`-status-type precondition line,
   **When** the bridge answers, **Then** the call proceeds normally and
   the answer's `remote`-tagged line, if any, only ever mirrors what the
   offer's own `a=curr:qos local` line already said — never a status this
   bridge invented.

### Edge Cases

- An offer names `Require: precondition` but its SDP contains **no**
  `a=des:qos` lines at all (a malformed or vacuous precondition offer):
  the bridge has nothing to confirm and nothing to honestly refuse either
  — treated as no precondition asked, same as if `Require` hadn't named
  it, rather than declined for absence of the thing it required.
- An `a=des:qos` line names a strength other than `mandatory`/`optional`
  (`none`, `failure`, `unknown`, or an unrecognized token): treated
  permissively per this module's established posture for unrecognized
  tokens elsewhere (specs/043 SDP-03's research.md Decision 1) — not
  itself a reason to decline.
- Multiple `a=des:qos` lines name the same status type with conflicting
  directions (e.g. two `remote`-status-type lines, one `sendrecv` one
  `recvonly`): the bridge answers per-line, mirroring each back with its
  own accurate status rather than trying to merge them into one claim.
- An offer's precondition lines sit on a declined, non-negotiated media
  section (specs/043 SDP-01's `other_media`): no `a=curr`/`a=conf` is
  emitted for a section the bridge already declines outright — a
  precondition on media the bridge won't use is moot.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The bridge MUST NOT decline an inbound INVITE for
  `Require: precondition` solely on the presence of that `Require` tag —
  the decline decision MUST depend on what the offer's `a=des:qos` lines
  actually ask for.
- **FR-002**: The bridge MUST parse `a=curr:qos`, `a=des:qos`, and
  `a=conf:qos` lines from the audio section of an SDP offer (RFC 3312
  §5), each identifying a status type (`e2e`/`local`/`remote`, `a=curr`
  only) or, for `a=des`, a strength (`mandatory`/`optional`/`none`/
  `failure`/`unknown`) plus a status type and a direction.
- **FR-003**: For every `a=des:qos` line naming the `remote` status type in
  the offer (this bridge's own segment, once inverted per RFC 3312 §5.2 —
  see User Story 1), the bridge's answer MUST include a corresponding
  `local`-tagged `a=curr:qos` line reporting that segment as met, and, if
  the line's strength is `mandatory` or `optional`, a `local`-tagged
  `a=conf:qos` line confirming it.
- **FR-004**: The bridge MUST decline (respond as it does today for an
  unsupported `Require`) an INVITE whose offer contains an `a=des:qos`
  line naming the `e2e` status type at `mandatory` strength, since the
  bridge cannot truthfully confirm a segment it does not control without a
  synchronization mechanism (UPDATE/100rel) it does not implement.
- **FR-005**: The bridge MUST NOT decline an offer whose `e2e`
  `a=des:qos` line is at `optional` (not `mandatory`) strength — the
  answer reports that segment as not (yet) met, and the call proceeds,
  consistent with RFC 3312's treatment of optional preconditions.
- **FR-006**: The bridge MUST NOT synthesize or assert a new status for the
  offerer's own segment (the offer's `local`-status-type lines, inverted to
  `remote` in the answer) — those lines are read, never used to block the
  call, and the answer's `remote`-tagged `a=curr:qos` line, if emitted at
  all, only ever mirrors whatever the offer's own `a=curr:qos local` line
  already stated, inverted but not altered.
- **FR-007**: An offer with `Require: precondition` but no `a=des:qos`
  lines at all MUST be treated as if no precondition were requested (not
  declined for lacking one).
- **FR-008**: `a=des:qos`/`a=curr:qos` lines on a declined, non-negotiated
  media section (per specs/043 SDP-01) MUST NOT produce any
  `a=curr`/`a=conf` output for that section.
- **FR-009**: This feature MUST NOT add SIP `UPDATE` method handling,
  reliable-provisional (`100rel`) support, or any multi-message
  readiness-negotiation state machine — every precondition this feature
  honours MUST be resolvable synchronously, within the same INVITE
  transaction's existing response.

### Key Entities

- **Precondition line**: One parsed `a=des:qos`/`a=curr:qos`/`a=conf:qos`
  attribute from an offer's audio section — carries a status type
  (`local`/`remote`/`e2e`), for `a=des` a strength and direction, captured
  alongside the rest of the offer's audio-section state (specs/043
  `SdpOffer`).
- **Precondition verdict**: The bridge's own determination, per offer, of
  whether every precondition line it read is one it can honestly confirm
  (proceed and answer with status) or contains at least one it cannot
  (decline) — mirrors the existing `unsupported_required_extensions`
  outcome shape (proceed vs. `420`), but decided from the SDP body rather
  than the `Require` header alone.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An inbound call whose only obstacle today is a `local`-segment
  (or unqualified) `Require: precondition` connects successfully — where
  today it is refused 100% of the time.
- **SC-002**: An inbound call requiring a genuinely unconfirmable `e2e`
  mandatory precondition continues to be declined 100% of the time, with
  no SDP answer ever asserting a segment status the bridge did not
  actually confirm.
- **SC-003**: Every other inbound call shape already handled today (no
  preconditions, unrelated `Require` tags, declined/extra media sections,
  offerless INVITEs) is unaffected — same accept/decline outcome and
  answer content as before this feature, confirmed by the full existing
  conformance test suite passing unchanged.

## Assumptions

- No carrier hardware available for testing (Jio, and whatever real lines
  are reachable via the `test/` rig) has been observed sending
  `Require: precondition` live, and only the carrier itself can trigger
  `agent::inbound::handle_invite` at all — this feature's new logic is
  verified with synthetic offers crafted the same way batches 4/6/7/8's
  SDP-01/02/03/05/06 fixtures were, not by a live precondition call. The
  hardware round required before this PR is ready (per Clarifications) is
  scoped to regression only: a real inbound call with no preconditions
  still connects and bridges cleanly on the new build.
- "Answering with accurate status" means this feature only ever emits
  `a=curr`/`a=conf` lines whose claims are true at the moment the answer
  is sent — it does not change *when* the bridge sends its answer (still
  synchronous within `handle_invite`, no deferred/pending state), since
  the bridge's own segment truthfully has no waiting condition regardless.
- The audio-only, single-negotiated-media-section scope already
  established by specs/043 (SDP-01/02/03) applies unchanged here:
  precondition lines are only read from the negotiated audio section, per
  FR-008.
