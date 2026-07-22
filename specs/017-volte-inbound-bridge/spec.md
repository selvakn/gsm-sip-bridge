# Feature Specification: Inbound Call Bridging over the Host-Side LTE Registration

**Feature Branch**: `017-volte-inbound-bridge`
**Created**: 2026-07-22
**Status**: Draft
**Input**: User description: "Answer calls arriving from the carrier over the host-side LTE IMS registration and bridge them to the operator's telephone system, as a long-lived service. Single process, not the two-process split the Wi-Fi calling path uses. Inbound only; outbound already works."

## Overview

Three features built to this point, each a prerequisite for the next:

- `015-volte-host-ims` gave the bridge its own cellular IMS registration.
- `016-volte-calls` proved calls work over it, with the media under the bridge's control — wideband audio, a dedicated conversational-voice channel granted by the network, 0.3% packet loss, and an operator judgement that it sounds markedly better than the modem's own path.

Both are diagnostics. Neither carries a call anyone actually wanted.

**This feature makes the work useful.** Calls arriving *from* the network are answered and connected through to the operator's telephone system, so a real incoming call to the SIM is carried with the bridge in control of the audio — which is what the audio-quality complaint that started all of this was ultimately about.

It is also the first piece of this work that must run **continuously**. Everything so far has been a command an operator runs and watches. This is a service: it holds one registration open indefinitely, keeps it alive, and answers calls whenever they arrive.

That shift is where the real work is. The registration must serve two masters — staying alive, and carrying calls — and those two jobs interfere with each other in ways the one-shot commands never had to confront.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Answer an incoming cellular call and connect it through (Priority: P1)

Someone dials the SIM's number. The operator wants the bridge to answer that call over its own cellular registration and connect it to their telephone system, with the audio under the bridge's control rather than the modem's.

**Why this priority**: It is the feature. Everything else here keeps this working over time or explains it when it does not.

**Independent Test**: With the service running, dial the SIM's number from another phone. The operator's telephone system rings, someone answers, and the two parties can hold a conversation in both directions.

**Acceptance Scenarios**:

1. **Given** the service is running with an accepted cellular registration, **When** someone dials the SIM's number, **Then** the bridge answers and the operator's telephone system rings.
2. **Given** the telephone system answers, **When** the two parties speak, **Then** each hears the other, in both directions, for the duration of the call.
3. **Given** a call is connected, **When** the calling party hangs up, **Then** the bridge ends the telephone-system side too, and returns to waiting for the next call.
4. **Given** a call is connected, **When** the *called* party hangs up, **Then** the bridge ends the cellular side too, and the calling party's phone shows the call ended normally.
5. **Given** the telephone system does not answer, **When** the attempt times out, **Then** the bridge reports that outcome and leaves the caller with a normal, non-hanging result.

---

### User Story 2 - Keep answering calls, indefinitely (Priority: P1)

The operator wants the service to stay ready without supervision: still registered, still answering, after hours and after the network has done whatever it does overnight.

**Why this priority**: Also P1, because a bridge that answers one call and then quietly stops is not a bridge — and this is where the genuinely new engineering is. The registration must be renewed before it expires, but renewing during a call would break that call, and the underlying cellular attachment is known to be torn down by the carrier roughly every couple of hours.

**Independent Test**: Leave the service running for several hours across at least one attachment teardown and several registration renewals, placing a call before and after each. Every call connects.

**Acceptance Scenarios**:

1. **Given** the service has been running past a registration's expiry, **When** a call arrives, **Then** it is answered — the registration was renewed in time.
2. **Given** a call is in progress, **When** the registration would otherwise be renewed, **Then** renewal waits until the call ends rather than disturbing it.
3. **Given** the carrier tears down the cellular attachment while the service is idle, **When** it happens, **Then** the service re-establishes the attachment and registration without operator intervention, and the next call is answered.
4. **Given** the carrier tears down the attachment *during* a call, **When** it happens, **Then** the call ends cleanly with that stated as the cause, and the service recovers and answers the next call.
5. **Given** the service cannot recover, **When** that state persists, **Then** it is visible to the operator rather than silently unavailable.

---

### User Story 3 - See what the service is doing (Priority: P2)

An operator running this unattended needs to know whether it is healthy, what calls it has handled, and why a call failed — without reproducing the failure.

**Why this priority**: Necessary for unattended operation, but a service that works and is opaque still beats one that reports beautifully and drops calls.

**Independent Test**: Query the service's status while idle, during a call, and after a failure, and confirm each is accurately reported.

**Acceptance Scenarios**:

1. **Given** the service is running, **When** the operator asks for status, **Then** it reports the registration state, whether a call is in progress, and when the registration expires.
2. **Given** calls have been handled, **When** the operator asks, **Then** recent call outcomes are available with enough detail to distinguish a normal call from a failed one.
3. **Given** a call failed, **When** the operator reviews it, **Then** the stage it failed at is named, distinguishing at minimum: the caller's side, the telephone system's side, and the audio path.
4. **Given** the service reports a call as successful, **When** that is compared against what actually happened, **Then** a call that connected but carried no audio is **not** among the successes.

---

### User Story 4 - Choose which cards use this path (Priority: P3)

An operator with several cards wants to select, per card, whether incoming cellular calls are handled this way or by the existing arrangement.

**Why this priority**: Only matters once the path is trusted enough to adopt selectively; until then the existing arrangements continue untouched.

**Independent Test**: Configure one card each way and confirm each behaves as configured, with the other unaffected.

**Acceptance Scenarios**:

1. **Given** a card not configured for this path, **When** a call arrives for it, **Then** its behaviour is exactly as it is today.
2. **Given** no explicit choice, **When** the service starts, **Then** the card uses the existing arrangement — the new path is opt-in.
3. **Given** the Wi-Fi calling path is active for a subscriber, **When** this path is also enabled for it, **Then** the conflict is refused with the reason, because the two cannot both hold that subscriber's registration.

---

### Edge Cases

- **The network never delivers an incoming call.** Registration works and the network already delivers other messages to the bridge, but an actual incoming call over this path has **never been observed**. If the carrier does not route calls to the bridge, the feature cannot work at all, and that must be established early rather than discovered late.
- **A second call arrives while one is in progress.** Must be handled deliberately — rejected with a sensible response rather than ignored, silently dropped, or allowed to corrupt the call already up.
- **The caller hangs up while the telephone system is still ringing.** The bridge must withdraw the telephone-system side rather than leaving it ringing at nobody.
- **The telephone system is unreachable** when a call arrives. The caller must get a defined outcome quickly, not silence.
- **A call outlives its registration.** Renewal is deferred during calls, so a long enough call could outlast the registration entirely. What happens then must be defined rather than left to chance.
- **The cellular attachment drops mid-call.** Known to happen roughly every couple of hours. The call must fail visibly, attributed to the attachment, and the service must recover.
- **Audio flows in only one direction.** A known, previously-experienced failure on the Wi-Fi calling path. A call that connects but carries audio only one way must be reported as failed, and the failing direction named.
- **The network offers audio formats the bridge cannot carry.** Must be refused clearly, and the refusal must not leave the caller hanging.
- **The service is asked to run while the Wi-Fi calling path holds the same subscriber's registration.** The two displace each other; this must be refused, not attempted.

## Requirements *(mandatory)*

### Functional Requirements

**Answering and bridging**

- **FR-001**: The service MUST accept calls arriving from the carrier over its own cellular registration.
- **FR-002**: The service MUST connect an accepted call through to the operator's telephone system, and MUST relay audio in both directions for the call's duration.
- **FR-003**: The service MUST present the calling party's number to the telephone system, so the operator can see who is calling.
- **FR-004**: The service MUST end both sides of a bridged call when either side ends it, and MUST report which side did.
- **FR-005**: The service MUST give the caller a defined outcome when the telephone system does not answer or cannot be reached, rather than leaving the call unanswered indefinitely.
- **FR-006**: The service MUST reject a second concurrent call with a defined response rather than ignoring it or disturbing the call already in progress.
- **FR-007**: The service MUST choose the audio format with the same deliberate preference the outbound path uses, since that choice determines whether the network treats the call as voice.

**Running continuously**

- **FR-008**: The service MUST hold one registration and keep it alive indefinitely, renewing it before it expires.
- **FR-009**: The service MUST NOT renew a registration while a call is in progress; renewal MUST wait until the call ends.
- **FR-010**: The service MUST re-establish the underlying network attachment and registration without operator intervention when the carrier tears them down.
- **FR-011**: The service MUST end a call in progress, with the cause stated, when the attachment underneath it is lost — and MUST recover afterwards.
- **FR-012**: The service MUST use exactly one registration for both staying alive and carrying calls; it MUST NOT establish a second one per call.
- **FR-013**: The service MUST make a persistent inability to register or attach visible to the operator rather than remaining silently unavailable.

**Observability**

- **FR-014**: The service MUST report, on request, its registration state, whether a call is in progress, and the registration's remaining lifetime.
- **FR-015**: The service MUST record recent call outcomes in enough detail to distinguish a normal call from a failed one without reproducing the failure.
- **FR-016**: The service MUST name the stage a failed call reached, distinguishing at minimum: the calling side, the telephone-system side, and the audio path.
- **FR-017**: The service MUST NOT report a call that carried no audio, or audio in only one direction, as successful — and MUST name the failing direction.
- **FR-018**: The service MUST report its health in the same terms the existing Wi-Fi calling service does, so an operator does not learn a second vocabulary.

**Reuse and non-regression**

- **FR-019**: The service MUST use the same implementation of registration, authentication, signalling protection, call handling and audio as the existing Wi-Fi calling path, differing only in the underlying network attachment.
- **FR-020**: The existing Wi-Fi calling path MUST continue to behave exactly as it does today, with no change to its configuration, operational commands, or observable behaviour.
- **FR-021**: The existing modem-internal cellular voice path MUST remain available and unchanged.
- **FR-022**: The service MUST refuse to run while the Wi-Fi calling path holds the same subscriber's registration, and MUST say why.

**Selection**

- **FR-023**: An operator MUST be able to choose, per card, whether incoming cellular calls are handled by this service.
- **FR-024**: When no choice is made, the card MUST use the existing arrangement — this path is opt-in.

### Key Entities

- **Bridged Call**: One incoming call and its two sides — the caller's and the telephone system's — with the stage reached, the outcome, which side ended it, and how long it lasted.
- **Calling Party**: The number that dialled in, presented onward to the telephone system.
- **Service Registration**: The single long-lived registration serving both liveness and calls, with its expiry, renewal schedule, and current state.
- **Call Audio Report**: What the audio did — whether it flowed both ways, and its condition in transit.
- **Service Health**: Registration state, current call, attachment state, and the last failure, as an operator would ask for them.
- **Card Path Selection**: Per card, whether incoming cellular calls use this service.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A call dialled to the SIM's number is answered and reaches the operator's telephone system within the time a caller would normally wait for a phone to ring.
- **SC-002**: On an answered call, both parties can hold a conversation, each hearing the other, for at least 60 seconds.
- **SC-003**: The service runs unattended for at least 4 hours, spanning at least one attachment teardown and several registration renewals, and answers a call successfully at the end of that period.
- **SC-004**: No call in progress is ever interrupted by the service's own maintenance.
- **SC-005**: Every call outcome exercised in testing is reported accurately, and no call that carried no audio is reported as successful.
- **SC-006**: Every failure mode exercised in testing names the stage it failed at, and the operator can act on that without reproducing it.
- **SC-007**: The Wi-Fi calling path shows zero behavioural regressions: its entire existing automated test suite passes unchanged, and a live Wi-Fi call completes.
- **SC-008**: Registration, authentication, signalling protection, call handling and audio exist once and serve both paths, with no duplicated implementation of any of them.
- **SC-009**: An operator can determine from the service's status alone whether it is currently able to answer a call.

## Assumptions

**Scope boundaries**

- **Incoming calls only.** Outgoing calls over this registration already work and are not revisited.
- **One call at a time.** Concurrent calls are out of scope; a second arriving call is refused deliberately (FR-006).
- Text messaging over this registration is out of scope.
- Call recording is out of scope; the diagnostic recording built previously remains available separately.

**Architecture**

- **The service is a single process.** The Wi-Fi calling path splits into two cooperating processes purely because its tunnel forces an isolation boundary that the telephone-system-side library cannot cross. The cellular path has no such boundary, so reproducing the split would add a private link, a control protocol and a second process for no isolation benefit. *(Recorded as a constraint during the previous feature, and carried forward deliberately.)*
- **The registration serving both liveness and calls is this feature's central problem.** The previous feature's one-shot command sidestepped it by owning a registration for a single call. That is explicitly not sufficient here. The existing Wi-Fi calling service already solves the same hazard — it defers renewal while a call is active, because renewing mid-call destroys the transport the call's own ending still needs — and that solution is expected to carry over rather than be reinvented.

**Environment and dependencies**

- A cellular IMS registration over the modem, as delivered and verified previously.
- The network delivers messages it originates to the bridge — already proven for registration-related messages. **Whether it delivers actual incoming calls the same way is unproven**, and is the single largest risk in this feature (see Edge Cases).
- The carrier tears the underlying attachment down roughly every couple of hours; automatic recovery already exists and is expected to carry the service across those events.
- The audio format preference determines whether the network treats a call as voice — established on the outbound path, where offering the wrong preference produced a 45-fold worse loss rate. The same care applies to how an incoming call's format is chosen.
- Privileged network operations run inside the existing container.
- A second telephone is available to place calls into the bridge, and audio quality is judged by people on a real call rather than asserted.

**Open questions this feature is expected to settle**

- **Whether the carrier routes incoming calls to the bridge at all.** Everything else here depends on it. It should be established as early as possible, because a negative answer invalidates the feature rather than merely delaying it.
- **Whether the network grants an incoming call the same conversational-voice treatment it grants outgoing ones.** Verified for outgoing calls previously; unverified in this direction.

**Reasonable defaults chosen**

- A second concurrent call is refused with a "busy" style outcome rather than queued, since the bridge fronts a single subscriber line.
- A call that outlives its registration is allowed to continue rather than being cut short, on the grounds that dropping a live conversation to satisfy a timer is worse than a registration that lapses slightly late.
- Health and call reporting follow the conventions the Wi-Fi calling service already established, so operators do not learn a second vocabulary.
