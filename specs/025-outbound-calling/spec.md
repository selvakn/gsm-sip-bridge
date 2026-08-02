# Feature Specification: Outbound Calling

**Feature Branch**: `025-outbound-calling`
**Created**: 2026-08-02
**Status**: Draft
**Input**: User description: "add outbound calling capability, across all paths (packet switch, volte, vowifi, pcsc), and for pbx path as well as sip server path. copy did as is and dial. if multiple cards are there, place call via whichever card is available."

## Clarifications

### Session 2026-08-02

- Q: Which SIP-side callers may originate an outbound call — the PBX only, a
  phone registered directly to the bridge's own SIP server mode, or both? → A:
  Both. This deliberately supersedes spec 024's FR-020, which refused
  phone-originated dial-out with a 403; a SIP-server-mode phone may now
  originate an outbound call exactly as a PBX-sent call can.
- Q: Is the dialed number passed to the mobile network unrestricted, or must
  it match an operator-configured allow-list? → A: Unrestricted — the bridge
  dials whatever destination it is given, with no allow-list or dial-plan
  enforcement of its own. Preventing abuse is the responsibility of whoever
  controls access to the SIP side (PBX dial plan, network isolation, phone
  account credentials), not this feature.
- Q: When more than one line is idle across different carrier paths at once,
  is there a preferred path order? → A: No preference — the bridge places the
  call on whichever idle, registered SIM it finds first, with no path
  priority and no configuration to influence the choice.
- Q: In SIP server mode, may any registered phone originate an outbound call,
  or only the `ring_aor` account (the one designated to receive inbound
  calls)? → A: Any registered phone. SIP server mode never routes calls
  between two registered phones, so an INVITE from a registered phone has no
  meaningful destination other than the mobile network; restricting dial-out
  to `ring_aor` alone would arbitrarily block other provisioned, already
  trusted phones.
- Q: On the PBX path, does the PBX reach the bridge over the existing
  outbound trunk registration, or does the bridge need a new, separately
  configured inbound address? → A: The existing registration. The bridge
  already registers to the PBX as a trunk to place its own calls; the PBX
  sends the outbound-request INVITE to that same registered contact, so no
  new listener or configuration is needed for the PBX path.
- Q: If the selected SIM fails immediately after selection (before the call
  actually reaches the network), does the bridge automatically retry on a
  different idle SIM, or fail the whole request? → A: Fail the whole
  request, counted as a network failure. No automatic retry on another SIM —
  consistent with the feature's no-cleverness, pass-through design and the
  existing edge case ruling out unlimited silent retry.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Place an outbound mobile call from the PBX (Priority: P1)

An operator's PBX is configured to send a call to the bridge — as it already
does today only to reach a caller who dialed in — but this time the call
originates on the PBX side: an extension, an auto-attendant, or an outbound
route dials a number, and the PBX sends it to the bridge instead of out
through a different trunk. The bridge takes the dialed number exactly as
presented and places that call out over the mobile network on whichever SIM
is free, without the operator choosing a specific card or path.

**Why this priority**: This is the feature's core value: today the bridge can
only ever be the one being called from the mobile side. Every other
capability in this feature (SIP-server-mode phones, path selection,
diagnostics) builds on this same dial-out mechanism.

**Independent Test**: Configure the bridge with the mode enabled, send a call
from the PBX naming a real destination number, and confirm the mobile network
places that call and two-way audio flows once it is answered — with no
per-call operator action.

**Acceptance Scenarios**:

1. **Given** at least one SIM is idle and registered on the mobile network,
   **When** the PBX sends the bridge a call naming a destination number,
   **Then** the bridge dials exactly that number out over the mobile network
   on an idle SIM, without altering or reformatting it.
2. **Given** such a call is ringing or in progress, **When** either the PBX
   side or the mobile side hangs up, **Then** the other leg is torn down to
   match, exactly as it is for an inbound call today.
3. **Given** the destination answers, **When** audio begins, **Then** it
   flows both ways for the duration of the call, using the same audio
   handling as the existing inbound path.

---

### User Story 2 - Dial out on whichever SIM is free (Priority: P1)

A deployment has more than one SIM — several EC20 modules, several VoWiFi
lines, a VoLTE line, or a mix — and an operator wants outbound calls to use
whichever one is currently idle, without naming a specific card, so that one
busy or unreachable SIM does not block outbound calling entirely.

**Why this priority**: Without this, the feature would only work on
single-SIM deployments, which is a small fraction of the installations this
bridge already supports for inbound calls.

**Independent Test**: With two or more SIMs configured and one already on a
call, place another outbound call and confirm it goes out on a different,
idle SIM rather than being rejected or queued behind the busy one.

**Acceptance Scenarios**:

1. **Given** multiple SIMs are configured and at least one is idle and
   registered, **When** an outbound call is requested, **Then** it is placed
   on one of the idle ones; the caller does not choose which.
2. **Given** every configured SIM is already on a call or otherwise
   unavailable (not registered, mid-recovery, disabled), **When** an outbound
   call is requested, **Then** it is refused immediately with a reason the
   caller's side can distinguish from "the destination didn't answer."
3. **Given** SIMs of different carrier paths (circuit-switched, VoWiFi,
   VoLTE) are all idle at once, **When** an outbound call is requested,
   **Then** exactly one of them is used and the others remain idle and
   available for the next call.

---

### User Story 3 - Dial out from a phone registered directly to the bridge (Priority: P2)

An operator running the bridge in its own SIP server mode — no PBX in the
deployment — has a phone registered directly to the bridge. Today that phone
can only receive calls; it cannot dial out. They want the same phone to be
able to place an outbound call over the mobile network, the same way a PBX
extension would in a PBX-backed deployment.

**Why this priority**: Without this, outbound calling would be unavailable on
exactly the small, PBX-free deployments that SIP server mode exists to serve
— the same population that most benefits from not needing a second system
just to place a call out.

**Independent Test**: On a deployment with SIP server mode enabled and no
PBX, dial a number from a phone registered to the bridge and confirm the
mobile network places that call and carries audio once answered.

**Acceptance Scenarios**:

1. **Given** a phone is registered to the bridge's own SIP server and at
   least one SIM is idle, **When** the phone dials a number, **Then** the
   bridge dials that exact number out over the mobile network on an idle SIM.
2. **Given** such a call is in progress, **When** either the phone or the
   mobile side hangs up, **Then** the other leg is torn down to match.
3. **Given** no SIM is idle, **When** the phone dials a number, **Then** the
   phone is told the call cannot be placed, using the same signalling it
   would see if a PBX-side call were refused for the same reason.

---

### User Story 4 - Place an outbound call the same way regardless of carrier path (Priority: P2)

An operator's SIMs may reach the mobile network over the plain
circuit-switched path, over VoWiFi, or over VoLTE — and a VoWiFi line's SIM
may itself be read from a modem or from a standalone smart-card reader. They
want outbound calling to work the same way no matter which of those a given
idle SIM happens to use, so path choice remains purely an inbound-call and
provisioning concern, not something that limits outbound calling.

**Why this priority**: Restricting outbound calling to only one carrier path
would make it unusable on the VoWiFi- and VoLTE-only deployments this bridge
already supports for inbound calls, and which the project's own guidance
prefers over the plain circuit-switched path for reliability.

**Independent Test**: With outbound calling enabled on a deployment carrying
calls over VoWiFi (or VoLTE) rather than the circuit-switched path, place an
outbound call and confirm it is dialed and carries audio with no difference
visible to the caller.

**Acceptance Scenarios**:

1. **Given** an idle SIM reachable only over VoWiFi or only over VoLTE,
   **When** it is chosen for an outbound call, **Then** the call is placed
   and carries audio exactly as it would over the circuit-switched path.
2. **Given** a VoWiFi line whose SIM is read through a smart-card reader
   rather than a modem, **When** it is chosen for an outbound call, **Then**
   it behaves identically to a modem-sourced VoWiFi line for this purpose.

---

### User Story 5 - Diagnose a failed or refused outbound call (Priority: P3)

An operator whose outbound call did not go through wants to tell, from the
service's existing logs and metrics, whether it was refused before dialing
(no SIM free), refused by the network, or simply not answered — without
needing a packet capture or physical access to a handset.

**Why this priority**: Valuable for day-two operation but not required to
demonstrate the feature delivering its core value.

**Independent Test**: Trigger an outbound call with every SIM busy, trigger
one that the mobile network rejects, and trigger one that rings out
unanswered; confirm the three are distinguishable from logs and metrics
alone.

**Acceptance Scenarios**:

1. **Given** an outbound call could not be placed because no SIM was idle,
   **When** an operator inspects logs and metrics, **Then** that is stated
   plainly and counted separately from a call the network itself rejected.
2. **Given** an outbound call attempt, **When** an operator inspects the
   service's metrics, **Then** they can see counts of outbound attempts by
   outcome, matching the granularity already available for inbound calls.

---

### Edge Cases

- **No SIM is idle when an outbound call is requested**: refused immediately
  with a distinct reason, not left to time out.
- **The dialed number is empty or contains characters that cannot be part of
  a phone number**: refused before any SIM is touched.
- **The chosen SIM loses network registration, or otherwise fails, between
  being selected and the call actually being placed**: the whole request
  fails cleanly and is counted as a network failure. The bridge does not
  automatically retry on a different idle SIM.
- **Two outbound requests arrive at effectively the same time and only one
  SIM is idle**: exactly one succeeds; the other is refused as if no SIM had
  been idle at all, rather than both racing for the same line.
- **An outbound call is requested while that same SIM is mid-recovery** (the
  self-healing behaviour the bridge already performs after a USB disconnect
  or registration loss): it is treated as unavailable, not idle.
- **The destination is busy, unreachable, or rejects the call**: reported to
  the caller using the same signalling the bridge already relays for other
  call failures, and distinguished in logs/metrics from "no SIM was idle."
- **Outbound calling is requested while inbound calling on the same SIM is
  also possible**: an inbound call already ringing or in progress makes that
  SIM unavailable for a new outbound attempt, and vice versa — one call at a
  time per SIM, consistent with the bridge's existing design.

## Requirements *(mandatory)*

### Functional Requirements

#### Mode selection and configuration

- **FR-001**: The system MUST provide an outbound calling capability that is
  disabled by default, so that existing deployments are unaffected by its
  introduction.
- **FR-002**: The system MUST allow a call arriving from the PBX side to
  trigger an outbound mobile call, symmetrically with how a call arriving
  from the mobile side already triggers a call to the PBX today. The PBX
  reaches the bridge over the same outbound trunk registration the bridge
  already establishes to place its own calls; no new listener or address is
  introduced for this path.
- **FR-003**: The system MUST also allow a phone registered directly to the
  bridge's own SIP server mode to originate an outbound mobile call, using
  the same dial-out mechanism as a PBX-originated call. This supersedes the
  SIP-server-mode restriction recorded in `specs/024-sip-server-mode` (its
  FR-020), which refused phone-originated dial-out; that refusal no longer
  applies once this feature is enabled. Eligibility to dial out extends to
  **any** currently registered phone, not only the account designated by
  `ring_aor` — SIP server mode never routes calls between two registered
  phones, so a registered phone's INVITE has no meaningful destination other
  than the mobile network.

#### Selecting a SIM

- **FR-004**: When an outbound call is requested, the system MUST select an
  idle, network-registered SIM without requiring the caller to name one.
- **FR-005**: The system MUST consider a SIM idle only when it is not
  currently carrying an inbound or outbound call and is not mid-recovery from
  a fault.
- **FR-006**: The system MUST make the same selection decision correctly
  regardless of which carrier path (circuit-switched, VoWiFi, or VoLTE) a
  candidate SIM uses, and regardless of whether a VoWiFi SIM is read from a
  modem or a smart-card reader.
- **FR-007**: The system MUST NOT apply any preference among carrier paths
  when more than one idle SIM is available; it selects whichever idle,
  registered SIM it finds first, with no configuration to bias the choice.
- **FR-008**: When two outbound requests contend for the same last-idle SIM,
  the system MUST grant it to exactly one of them and refuse the other with
  the same reason used when no SIM is idle at all.
- **FR-009**: When no SIM is idle, the system MUST refuse the request
  immediately with a reason distinguishable from a network-side failure.
- **FR-009a**: If the selected SIM fails before the call reaches the mobile
  network (for example it loses registration between selection and dialing),
  the system MUST fail the whole request and count it as a network failure.
  The system MUST NOT automatically retry the request on a different SIM.

#### Placing the call

- **FR-010**: The system MUST dial the destination number exactly as
  presented by the SIP-side caller, with no digit stripping, prefix
  insertion, or reformatting.
- **FR-011**: The system MUST NOT apply any allow-list, dial-plan, or other
  authorization check of its own to the destination number; every
  syntactically valid destination presented by an eligible SIP-side caller
  (FR-002, FR-003) is dialed. Restricting who may reach the bridge and what
  they may dial is the responsibility of the PBX's own dial plan, network
  access controls, and SIP-server-mode account credentials — not this
  feature.
- **FR-012**: The system MUST relay the mobile network's call progress (for
  example ringing, busy, rejected, answered) back to the SIP-side caller
  using the same signalling patterns it already uses for other call
  outcomes.
- **FR-013**: Once a destination answers, audio handling and teardown
  behaviour MUST be unchanged from the bridge's existing call handling.
- **FR-014**: The system MUST reject, before dialing, a destination number
  that is empty or contains characters that cannot form part of a phone
  number.

#### Observability

- **FR-015**: The system MUST count outbound call attempts by outcome —
  placed, refused for no idle SIM, refused for an invalid destination,
  refused by the network, and unanswered — at a granularity matching what is
  already available for inbound calls.
- **FR-016**: The system MUST log enough detail on a refused or failed
  outbound attempt for an operator to distinguish "no SIM was idle" from a
  network-side failure without a packet capture.

#### Compatibility

- **FR-017**: With outbound calling disabled, the system's behaviour MUST be
  byte-for-byte unchanged from before this feature, on all inbound call
  paths and both SIP-side topologies (PBX and SIP server mode).

### Key Entities

- **Outbound call request**: a request from the SIP side naming a
  destination number, to be placed on whichever eligible SIM is idle at the
  moment it arrives. Not tied to any particular SIM until one is selected.
- **Idle SIM**: a configured line — circuit-switched, VoWiFi, or VoLTE —
  that is currently registered on the mobile network, not carrying a call in
  either direction, and not mid-recovery.
- **Destination number**: the number the SIP-side caller dialed, carried
  through to the mobile network unmodified.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A SIP-side call naming a destination number results in that
  exact number being dialed out over the mobile network on an idle SIM, with
  no per-call operator action.
- **SC-002**: On a deployment with multiple SIMs, one SIM being busy never
  blocks an outbound call that another idle SIM could carry.
- **SC-003**: Outbound calling works identically whether the SIM used
  reaches the network over the circuit-switched path, VoWiFi, or VoLTE, and
  regardless of whether a VoWiFi SIM comes from a modem or a card reader.
- **SC-004**: When no SIM is idle, the caller is told so immediately — not
  left to wait for a timeout that looks the same as an unanswered call.
- **SC-005**: The three outcomes an operator cares about — no SIM available,
  network refused the call, destination didn't answer — are distinguishable
  from logs and metrics alone.
- **SC-006**: Every deployment that works today continues to work
  identically with outbound calling present and disabled.

## Assumptions

- **One call per SIM at a time**, matching the bridge's existing
  one-call-at-a-time design; outbound calling contends for the same idle/busy
  state as inbound calling rather than adding a separate capacity model.
- **No dial plan or number translation.** "Copy the DID as-is and dial" is
  taken literally: no area-code normalization, no prefix stripping, no
  E.164 conversion. Whatever the mobile carrier does with the raw digits is
  outside this feature's control.
- **Card/path selection has no operator-visible configuration beyond
  enabling the feature.** The caller does not name a card, there is no path
  priority, and "whichever is available" needs no additional knob.
- **Abuse prevention is out of scope for this feature.** The bridge trusts
  that anything able to reach it on the SIP side (a PBX or a SIP-server-mode
  phone) is already authorized to place calls and dial any destination.
  Deployments that need to restrict who can dial what must do so upstream —
  in the PBX's own dial plan, via network isolation, or by limiting which
  accounts can register in SIP server mode — the same trust boundary that
  already governs who can register a phone to the bridge at all.
- **This does not change the mobile network's own call acceptance.** A
  destination number the carrier rejects (invalid, barred, unreachable) fails
  exactly as it would if dialed from a handset on the same SIM; the bridge is
  not expected to validate numbers against a dial plan beyond basic
  well-formedness.
