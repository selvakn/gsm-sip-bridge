# Feature Specification: Voice Calls over the Host-Side LTE Registration

**Feature Branch**: `016-volte-calls`
**Created**: 2026-07-22
**Status**: Draft
**Input**: User description: "Voice calls over the host-side LTE IMS registration. Place a real outbound call over the registration established in specs/015-volte-host-ims, exchange audio, and prove the media path works with the audio under our own software control. Outbound diagnostic call only; inbound and PBX bridging are a follow-up. The modem-internal path stays as a per-card option."

## Overview

`specs/015-volte-host-ims` gave the bridge its own IMS registration over cellular. That was the prerequisite. **This feature is the point of the exercise.**

Cellular voice today works, but the bridge only ever receives an *already-decoded, already-degraded* audio stream from the modem's internal voice path, and then has to re-bridge it. Every decision that determines how the call sounds — which codec is negotiated, how jitter is absorbed, when frames are dropped — is made inside the modem, invisibly, and cannot be measured or tuned. The audio quality complaints that motivated this whole effort come from that hand-off.

Placing the call over the bridge's own registration removes the hand-off entirely: the bridge negotiates the codec, sends and receives the audio itself, and can measure exactly what happened.

**Scope is deliberately one outbound call.** An operator runs a command, the bridge calls a number over the cellular registration, sends a speech sample, and records what comes back. That is the smallest thing that proves the media path end to end — and it is also the only way to answer the question the whole effort rests on: **is the audio actually better?** Answering the question is a first-class goal here, not a side effect.

Answering inbound calls, and bridging calls to the operator's telephone system, are explicitly a follow-up.

## Clarifications

### Session 2026-07-22

- Q: What audio does the bridge send during the call? → A: A speech sample by default, with a test tone selectable
- Q: What counts as evidence that the network gave the call preferential handling? → A: Query the modem before, during and after the call and report the change; report "undetermined" explicitly when the modem will not answer
- Q: How does the call end? → A: A default call duration, overridable; ends early if the far end hangs up; operator interrupt also ends it cleanly
- Q: Does this feature produce the modem-internal comparison call? → A: No. The operator compares manually; no comparison tooling is built
- Q: What counts as "no received audio" for failing an answered call? → A: Received audio below a defined proportion of what was sent (default 10%), not an absolute count

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Place a call over the cellular registration and exchange audio (Priority: P1)

An operator with an established cellular IMS registration wants the bridge to place a real call to a real number, send audio, and capture the audio that comes back — with the bridge, not the modem, handling the media.

**Why this priority**: It is the feature. Everything else here either measures this call or diagnoses it when it fails.

**Independent Test**: The operator runs one command with a destination number. The called phone rings, a person answers, the bridge sends recognisable audio the answering party can assess, and afterwards the operator has a recording containing the answering party's speech.

**Acceptance Scenarios**:

1. **Given** an accepted cellular IMS registration, **When** the operator places a call to a reachable number, **Then** the called party's phone rings and the bridge reports the call progressing through ringing to answered.
2. **Given** the call is answered, **When** audio flows, **Then** the far end hears the bridge's outgoing audio clearly enough to judge its quality, and the bridge records the far end's audio to a file the operator can play back.
3. **Given** the call is in progress, **When** the configured duration elapses, **Then** the bridge terminates the call cleanly, the far end sees it end normally, and the report says the duration ended it.
6. **Given** the call is in progress, **When** the far end hangs up first, **Then** the bridge ends immediately rather than waiting out the remaining duration, and reports that the far end ended it.
4. **Given** the called party does not answer or is busy, **When** the attempt completes, **Then** the bridge reports that specific outcome rather than a generic failure.
5. **Given** the carrier will not accept the audio formats offered, **When** the call is attempted, **Then** the bridge reports that the formats were refused and which were offered.

---

### User Story 2 - Establish whether the audio is actually better (Priority: P1)

The operator needs evidence, not assertion, that routing voice through the bridge improves on the modem's internal path — and evidence of *why*, so a disappointing result is actionable rather than mysterious.

**Why this priority**: This is the question the entire effort exists to answer, and it is answerable the moment US1 works. Shipping call capability without measuring it would leave the original complaint unresolved and unfalsifiable. The feature's job is to *produce the evidence*; the operator draws the comparison. It is also where the largest technical risk sits: the cellular network gives voice traffic preferential treatment only when it recognises the call as voice, and whether the bridge's audio receives that treatment is **unverified**.

**Independent Test**: After a call, the operator gets a report of what actually happened to the media — how much audio was sent, how much arrived, how it was affected in transit, and how the network's treatment of the connection changed while the call was up — plus a recording they can listen to.

**Acceptance Scenarios**:

1. **Given** a completed call, **When** the operator reviews the report, **Then** it states how much audio was sent and how much was received, separately and in comparable units.
2. **Given** a completed call, **When** the operator reviews the report, **Then** it states which audio format was actually used, and at what bandwidth.
3. **Given** a completed call, **When** the operator reviews the report, **Then** it states how the network's treatment of the connection changed between before, during and after the call.
6. **Given** the modem will not report how the network is treating the connection, **When** the report is produced, **Then** it says so explicitly and names what was asked, rather than silently omitting the finding.
4. **Given** audio was sent but little or none arrived, **When** the report is produced, **Then** it distinguishes that from the reverse case, so a one-way-audio fault points at the responsible direction immediately.
5. **Given** a completed call, **When** the operator plays back the recording and reads the report, **Then** they have enough material to judge the new path's audio quality for themselves and compare it, by ear, against their experience of the modem-internal path.

---

### User Story 3 - Diagnose a failed call by the stage it failed at (Priority: P2)

When a call does not work, the operator needs to know *where* it broke without re-running it under instrumentation.

**Why this priority**: Valuable, and cheap once US1 exists, but a working call is worth more than good diagnostics on a broken one.

**Independent Test**: Induce failures at different stages — no registration, unreachable destination, refused formats, media blocked — and confirm each produces a distinct, accurate report.

**Acceptance Scenarios**:

1. **Given** no active registration, **When** a call is attempted, **Then** the bridge reports that as the cause and does not attempt to dial.
2. **Given** the network rejects the call attempt, **When** it does so, **Then** the bridge reports the rejection reason it was given.
3. **Given** the call connects but the audio arriving falls below the defined proportion of what was sent, **When** the call ends, **Then** the bridge reports the call as answered-but-silent rather than as a success, and names the direction that failed.
4. **Given** the network attachment drops mid-call, **When** that happens, **Then** the bridge reports the call as interrupted by the attachment, distinct from the far end hanging up.

---

### User Story 4 - Choose which voice path a card uses (Priority: P3)

An operator with several cards wants to select, per card, whether cellular voice goes through the bridge's own path or the modem's internal one.

**Why this priority**: Only matters once the new path is trusted enough to adopt selectively. Until then the diagnostic command is sufficient, and the modem-internal path continues to work untouched.

**Independent Test**: Configure one card each way and confirm each behaves as configured, with the other unaffected.

**Acceptance Scenarios**:

1. **Given** a card configured for the modem-internal path, **When** it handles cellular voice, **Then** its behaviour is exactly as it is today.
2. **Given** a card configured for the bridge's own path, **When** it handles cellular voice, **Then** the bridge controls the media.
3. **Given** no explicit choice, **When** a card handles cellular voice, **Then** it uses the modem-internal path — the established behaviour, not the new one.

---

### Edge Cases

- **The carrier refuses the offered audio formats.** Cellular carriers commonly require a specific wideband voice format and reject an offer that lacks it. The bridge must report this distinctly, naming what it offered, rather than as a generic call failure.
- **The build lacks the wideband audio format.** The format depends on optional components that may be absent from a given build. The bridge must detect this before dialling and say so, rather than making an offer it knows the carrier will refuse.
- **Audio is sent but nothing comes back (or vice versa).** This is a *known, previously-experienced* failure mode on the Wi-Fi calling path (`docs/incidents/2026-07-15-vowifi-oneway-audio.md`). The lesson from that incident — that comparing sent-versus-received counts is what separates "the carrier isn't sending" from "we can't decode what it sends" — must be designed in from the start here, not retrofitted after an outage.
- **The call is answered but the audio path never establishes.** Must be reported as a failure, never as success.
- **The network attachment drops mid-call.** The attachment is known to be torn down by the carrier periodically (`specs/015-volte-host-ims` research R15). A call in progress when that happens must fail visibly and distinctly.
- **The registration lapses mid-call.** A call outliving its registration must not be reported as healthy.
- **The Wi-Fi calling path is active on the same subscriber.** The two cannot register simultaneously; attempting a call while the other path holds the registration must be refused with the reason.
- **The destination is unreachable, busy, or unanswered.** Each is a distinct, normal outcome and must be reported as itself.
- **The far end answers and immediately hangs up.** Must produce a valid, if short, result rather than an error.

## Requirements *(mandatory)*

### Functional Requirements

**Placing the call**

- **FR-001**: The bridge MUST place an outbound voice call over its own cellular IMS registration.
- **FR-002**: The bridge MUST use the existing registration rather than establishing a second one for the call.
- **FR-003**: The bridge MUST protect the call's signalling the same way it protects the registration's.
- **FR-004**: The bridge MUST report call progress through at least: attempting, ringing, answered, ended.
- **FR-005**: The bridge MUST terminate the call cleanly when it ends, whether that is because the configured duration elapsed, the far end hung up, or the operator interrupted it — and MUST report which of those ended it.
- **FR-027**: The bridge MUST run the call for a default duration long enough to assess audio quality, MUST allow that duration to be overridden, and MUST end the call early when the far end hangs up rather than holding it open for the remaining time.
- **FR-006**: The bridge MUST refuse to place a call when it has no accepted registration, and say so.

**Audio**

- **FR-007**: The bridge MUST send audio the far end can hear, and MUST receive audio from the far end.
- **FR-008**: The bridge MUST record the received audio to a file the operator can play back.
- **FR-009**: The bridge MUST offer the audio formats the carrier accepts, and MUST report when the carrier refuses all of them, including which were offered.
- **FR-010**: The bridge MUST detect, before dialling, that a required audio format is unavailable in the running build, and report it rather than making an offer that cannot succeed.
- **FR-011**: The bridge MUST report which audio format was actually used for the call.
- **FR-025**: The bridge MUST send speech-like audio by default, and MUST allow a simple tone to be selected instead. *(A tone survives codec artefacts, loss concealment and jitter far more gracefully than speech, so it can sound perfect on a call a listener would find unusable — it cannot support the quality judgement SC-004 requires.)*

**Measuring the result**

- **FR-012**: The bridge MUST report how much audio it sent and how much it received, separately.
- **FR-013**: The bridge MUST report enough about the received audio's condition in transit for an operator to judge quality without specialist tooling.
- **FR-014**: The bridge MUST determine whether the network gave the call preferential handling by querying the modem for its own view of how the network is treating the connection — sampled before the call, while it is in progress, and after it ends — and MUST report the change across those samples.
- **FR-026**: When the modem will not report that information, the bridge MUST say so explicitly and name what it asked. Reporting "undetermined" MUST be a stated outcome with a reason, never the silent default.
- **FR-015**: The bridge MUST distinguish a call where audio flowed only one way from one where it flowed both ways, and identify which direction failed.
- **FR-016**: The bridge MUST report an answered call as a failure when the audio received falls below a defined proportion of the audio sent, rather than requiring the received amount to be exactly zero. The proportion MUST have a documented default and be overridable.
- **FR-028**: The bridge MUST apply that test to each direction independently, so that a call failing in one direction is reported as failing in *that* direction.

**Diagnostics**

- **FR-017**: On failure, the bridge MUST identify the stage reached, distinguishing at minimum: no registration, call rejected by the network, audio formats refused, call answered but no audio, and attachment lost mid-call.
- **FR-018**: The bridge MUST report the reason the network gave when it rejects a call.

**Reuse and non-regression**

- **FR-019**: The bridge MUST use one implementation of call setup, audio format negotiation, and audio handling across both the Wi-Fi calling path and the cellular path, differing only in the underlying network attachment.
- **FR-020**: The existing Wi-Fi calling path MUST continue to behave exactly as it does today, with no change to its configuration, operational commands, or observable behaviour.
- **FR-021**: The existing modem-internal cellular voice path MUST remain available and unchanged.
- **FR-022**: The bridge MUST refuse to place a call over the cellular path while the Wi-Fi calling path holds the subscriber's registration, and say why.

**Selection**

- **FR-023**: An operator MUST be able to choose, per card, which cellular voice path is used.
- **FR-024**: When no choice is made, the bridge MUST use the modem-internal path.

### Key Entities

- **Call Attempt**: One outbound call — its destination, the stage it reached, its outcome, and how long it lasted.
- **Negotiated Audio Format**: The format both ends agreed to use, and its bandwidth characteristics.
- **Media Report**: What happened to the audio — sent and received volumes, condition in transit, and whether the network gave the call preferential handling.
- **Call Recording**: The captured far-end audio, in a form the operator can play back.
- **Voice Path Selection**: Per card, which cellular voice path is in use.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An operator places a call to a real telephone number with a single command; the phone rings and can be answered.
- **SC-002**: On an answered call, the answering party hears the bridge's audio, and the operator can play back a recording containing the answering party's speech.
- **SC-003**: After every call, the operator can tell from the report alone whether audio flowed in both directions, and if not, which direction failed — judged by comparing the two directions against each other, so the verdict holds regardless of call length.
- **SC-004**: After a call, the operator has both a playable recording and measurements of the media, and can judge from them whether the new path's audio quality is acceptable. *(Comparison against the modem-internal path is performed manually by the operator; this feature does not automate it — see Assumptions.)*
- **SC-005**: Every failure mode exercised in testing produces a report naming the stage that failed, actionable without re-running under instrumentation.
- **SC-006**: A call runs unattended for its configured duration of at least 30 seconds, with audio flowing continuously in both directions throughout, and ends without operator intervention.
- **SC-007**: The Wi-Fi calling path shows zero behavioural regressions: its entire existing automated test suite passes unchanged, and a live Wi-Fi call completes.
- **SC-008**: Call setup, audio format negotiation, and audio handling exist once and serve both paths, with no duplicated implementation of any of the three.

## Assumptions

**Scope boundaries**

- **Answering inbound calls is out of scope.** Only outbound calls placed by the operator.
- **Bridging calls to the operator's telephone system is out of scope.** The call terminates at the bridge, which plays a tone and records; it is not connected to anything else.
- **A design constraint is recorded for that follow-up**: when bridging is built, it must be a single process. The Wi-Fi calling path splits into two cooperating processes only because its tunnel forces an isolation boundary that the telephone-system-side library cannot cross; the cellular path has no such boundary (`specs/015-volte-host-ims` research R4), so reproducing the split would add moving parts with nothing behind them.
- Multiple simultaneous calls are out of scope. One call at a time.
- **Comparing against the modem-internal path is out of scope as tooling.** The operator performs that comparison manually. Building symmetric measurement for the old path would be a large scope increase for little return, because the bridge receives already-decoded audio there and cannot obtain most of the measurements FR-012/FR-013 require — promising a like-for-like comparison would be promising a rigour the old path cannot supply. This feature's obligation is to produce a recording and measurements good enough for the operator to judge.
- Automatic selection between voice paths based on conditions is out of scope; selection is explicit configuration.

**Environment and dependencies**

- An accepted cellular IMS registration exists — the capability delivered by `specs/015-volte-host-ims`, which is complete and verified on live hardware.
- The wideband audio format depends on optional components compiled into the build. **Verified present in the deployed container image**, which links all three required libraries; **absent from a plain local build**, which is why FR-010 requires detecting this rather than assuming it.
- Media travels over the same cellular attachment the registration uses. That attachment is known to be torn down by the carrier periodically and re-established automatically; a call in progress at that moment is expected to fail (see Edge Cases).
- Privileged network operations run inside the existing container.
- A live telephone number is available for testing and is answered by a person, so audio quality can be judged directly and not only inferred from measurements.

**Open questions this feature is expected to settle**

- **Whether the network gives the bridge's audio the preferential handling cellular voice receives.** This is the largest open risk and the one that decides whether the quality goal is met. The bridge's audio reaches the network over a link the modem controls, so the host cannot observe this directly — only the modem can be asked, and it may decline. FR-014 therefore requires sampling the modem's view before, during and after the call and reporting the change; FR-026 requires an explicit, reasoned "undetermined" when it will not answer, so the finding can never pass as success by silence. **If it turns out the audio is not prioritised, the quality gain may not materialise, and that is a legitimate finding for this feature to produce.**
- **Which of the attachment's two addresses the network actually routes** (`specs/015-volte-host-ims` research R9). Signalling works today, but media is what will settle it: audio sent and never received would be the symptom.

**Reasonable defaults chosen**

- Audio quality is assessed both by a person listening to a recording and by the measurements in the media report; neither alone is sufficient, since measurements can look healthy while audio is unusable, and listening alone gives nothing actionable.
- The recording captures the far end only. Capturing both directions mixed would make it impossible to tell which side a defect came from.
- A call is judged one-way by comparing received audio against sent audio as a proportion, defaulting to 10%. A ratio rather than an absolute count is what diagnosed the previous one-way-audio incident, and it stays correct whatever the call's length. A quiet answering party still produces audio frames, so this distinguishes "nothing is reaching us" from "they said nothing" — which a loudness measurement would not.
- Outgoing audio is speech by default because the feature's headline outcome is a quality judgement, and only speech exposes the artefacts a listener would object to. A tone remains selectable for the cheaper "is there an audio path at all" check.
- Failure reporting follows the conventions already established by the registration work, so operators do not learn a second vocabulary.
- The call runs for a fixed default duration rather than until interrupted, so that the quality outcome (SC-006) is reproducibly testable and the command can be scripted into validation runs instead of depending on someone hanging up at the right moment. Ending early on far-end hangup keeps the busy/no-answer/hung-up outcomes distinguishable.
