# Feature Specification: Inbound VoWiFi-to-SIP Call Bridge

**Feature Branch**: `011-vowifi-sip-bridge`
**Created**: 2026-07-12
**Status**: Draft
**Input**: User description: "we have the gsm sip bridge which can forward calls from gsm (via quectel usb audio) to sip, and we also have tested the vowifi call implemented with tunnel to ePDG. Now plan for combining these two. We should receive the gsm call over vowifi and bridge it to the sip side with two way bridging."

## Clarifications

### Session 2026-07-12

- Q: When the bridge declines an inbound VoWiFi call (busy, or SIP/PBX side unreachable), what should the calling party experience? → A: Call is answered then immediately rejected with a standard busy/unavailable signal — fast, explicit feedback.
- Q: Should the success-criteria timing targets (answer time, network-recovery time) mirror the existing GSM circuit-switched resiliency feature's established norms, or use independent tolerances for VoWiFi's heavier IMS-AKA/IPsec handshake? → A: Align with existing norms — keep the 5s answer-time target as-is, but loosen the network-recovery target to 90 seconds (existing 60s network-loss-detection window plus headroom for the IMS-AKA/IPsec handshake).
- Q: Can the carrier ever deliver a call over both the circuit-switched and VoWiFi paths simultaneously on the same line, requiring the bridge to coordinate across paths? → A: Out of scope — the carrier network already ensures a line can't have simultaneous CS and VoWiFi calls; the bridge needs no cross-path coordination.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Inbound VoWiFi call reaches a SIP extension (Priority: P1)

Someone dials the phone number associated with the bridge's SIM card. Instead of arriving as a
traditional circuit-switched cellular call, the call arrives over the carrier's VoWiFi (WiFi
Calling) service because the line is registered for VoWiFi. The bridge answers the call
automatically and connects the caller's audio to the operator's SIP/PBX phone system, exactly as
it already does for circuit-switched GSM calls, so the caller can talk to whoever answers on the
SIP side and both parties hear each other clearly for the whole call.

**Why this priority**: This is the entire point of the feature — without it, VoWiFi calls simply
aren't answered at all. Everything else (resiliency, visibility) only matters in support of this
core flow working reliably.

**Independent Test**: Register the line for VoWiFi, call the SIM's number from an external phone
while the bridge is running, and confirm the call is answered and both directions of audio are
audible and intelligible for the call's full duration, using an existing SIP extension as the
far end.

**Acceptance Scenarios**:

1. **Given** the bridge is running with an active VoWiFi registration, **When** an external caller
   dials the SIM's number and the call arrives over VoWiFi, **Then** the bridge answers the call
   and the caller is connected to the configured SIP destination with two-way audio.
2. **Given** a VoWiFi call is connected and bridged, **When** the caller speaks, **Then** the
   audio is heard on the SIP side, and when the SIP-side party speaks, the caller hears it, with
   no perceptible one-way silence.
3. **Given** a bridged VoWiFi call is in progress, **When** either the caller or the SIP-side
   party hangs up, **Then** the other leg of the call is also terminated promptly and the bridge
   returns to being ready for the next call.

---

### User Story 2 - VoWiFi availability is maintained without manual intervention (Priority: P2)

The operator starts the bridge (or it restarts after a reboot, power blip, or transient network
issue) and expects it to keep itself continuously reachable over VoWiFi without anyone needing to
re-run a setup step by hand. If the underlying network path to the carrier drops and comes back,
or the VoWiFi session naturally expires, the bridge re-establishes reachability on its own so
inbound calls keep working unattended.

**Why this priority**: A bridge that must be manually re-armed after every network hiccup isn't
usable as a real always-on phone line — it directly undermines the value of User Story 1. It is
ranked below Story 1 because the answer/bridge mechanics must exist first before resiliency around
them is meaningful.

**Independent Test**: With the bridge running and successfully receiving VoWiFi calls, deliberately
interrupt the underlying network path (e.g., disconnect and reconnect the WAN link) and confirm
that, without any manual command, the bridge becomes reachable over VoWiFi again within a bounded
time and successfully answers a subsequent test call.

**Acceptance Scenarios**:

1. **Given** the bridge has lost and then regained the underlying network path to the carrier,
   **When** no operator action is taken, **Then** the bridge automatically re-establishes VoWiFi
   reachability and can answer a subsequent inbound call.
2. **Given** the bridge's VoWiFi session is nearing its natural expiry, **When** the expiry
   threshold is reached, **Then** the bridge renews the session before it lapses, with no gap in
   which an inbound call would go unanswered.
3. **Given** the bridge process restarts (e.g., after a crash or reboot), **When** it comes back
   up, **Then** it re-establishes VoWiFi reachability on its own without requiring a manual
   command to be run.

---

### User Story 3 - Operator can confirm the VoWiFi line is healthy (Priority: P3)

The operator wants to be able to check, at a glance, whether the VoWiFi line is currently reachable
and whether recent calls were bridged successfully or failed — the same way they can already check
the health of the circuit-switched GSM line — so they can trust the system is working unattended
and quickly notice if it isn't.

**Why this priority**: Valuable for trust and troubleshooting, but the bridge already delivers its
core value (Stories 1–2) without it; this is an operational nicety layered on top.

**Independent Test**: With the bridge running, query its status through the existing operational
tooling and confirm it reports current VoWiFi registration health and the outcome (answered,
failed, rejected) of recent inbound call attempts.

**Acceptance Scenarios**:

1. **Given** the bridge is running, **When** the operator checks its status, **Then** the current
   VoWiFi registration state (reachable / not reachable, and how long until renewal) is reported.
2. **Given** a VoWiFi call was answered and bridged, **When** the operator reviews recent activity,
   **Then** the call's outcome (answered, duration, which SIP destination it was bridged to) is
   recorded and visible.
3. **Given** a VoWiFi call failed to bridge (e.g., the SIP side was unreachable), **When** the
   operator reviews recent activity, **Then** the failure and its reason are visible.

---

### Edge Cases

- What happens when an inbound VoWiFi call arrives while another VoWiFi call is already in
  progress? (See FR-009 — declined with a fast busy/unavailable signal; single concurrent call is
  the assumed norm for a single-SIM line.)
- What happens when the SIP/PBX side cannot be reached at all (down, unreachable) at the moment a
  VoWiFi call arrives? (See FR-010 — the inbound call is declined with the same fast
  busy/unavailable signal rather than answered into dead air.)
- What happens when the caller abandons the call (hangs up) before the SIP-side leg finishes
  connecting? Both legs must be cleanly torn down with no orphaned call state.
- What happens when the VoWiFi session cannot be renewed in time (e.g., the SIM/authentication
  step needed for renewal is temporarily unavailable)? The bridge should keep retrying and surface
  the degraded state (Story 3) rather than silently going dark.
- What happens when the caller's device and the network only support an audio format the SIP side
  doesn't handle well? The call should still connect with usable, intelligible audio even if
  quality isn't optimal, rather than failing outright.
- What happens to the existing circuit-switched GSM-to-SIP bridge while VoWiFi is active on the
  same line? It must continue operating independently and unaffected (see FR-006). The carrier
  network — not the bridge — is responsible for ensuring a line never has simultaneous
  circuit-switched and VoWiFi calls; the bridge does not need to detect or coordinate across the
  two paths itself (out of scope), only handle whichever single call its own path receives.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The bridge MUST establish and continuously maintain VoWiFi reachability for the
  configured subscriber line while the service is running, so inbound calls can arrive at any time.
- **FR-002**: The bridge MUST detect an inbound call arriving over VoWiFi and automatically answer
  it without requiring operator action.
- **FR-003**: Upon answering an inbound VoWiFi call, the bridge MUST establish a corresponding call
  to the SIP/PBX side, using the same call-routing configuration (fixed destination vs. passthrough
  of caller identity) already used for circuit-switched GSM-originated calls, so behavior is
  consistent between the two inbound paths.
- **FR-004**: The bridge MUST relay audio in both directions between the VoWiFi caller and the
  SIP/PBX call leg for the entire duration of the call, with no perceptible one-way audio loss.
- **FR-005**: The bridge MUST end both call legs promptly when either the VoWiFi caller or the
  SIP/PBX side terminates the call, and MUST return to a ready state for the next inbound call.
- **FR-006**: The bridge MUST operate the VoWiFi call path as an independent mode that does not
  disrupt or interfere with the existing circuit-switched GSM-to-SIP bridge functionality.
- **FR-007**: The bridge MUST automatically recover VoWiFi reachability after a network
  interruption or session expiry, without requiring manual intervention.
- **FR-008**: The bridge MUST make current VoWiFi line health (reachable/not, time until renewal)
  and recent call outcomes (answered, failed, declined, with reasons) available to the operator
  through the bridge's existing operational status tooling.
- **FR-009**: The bridge MUST decline (rather than queue or silently drop) an inbound VoWiFi call
  that arrives while another VoWiFi call is already active, giving the caller a fast, explicit
  busy/unavailable signal rather than leaving the call ringing unanswered.
- **FR-010**: The bridge MUST decline an inbound VoWiFi call rather than answer it into dead air if
  the corresponding SIP/PBX call leg cannot be established, giving the caller the same fast,
  explicit busy/unavailable signal as FR-009.
- **FR-011**: The bridge MUST forward the caller's identifying information (calling number) to the
  SIP/PBX side when bridging a VoWiFi call, consistent with how caller identity is already
  forwarded for circuit-switched GSM calls.
- **FR-012**: The bridge MUST NOT persist recordings of production call audio by default (see
  Assumptions); any audio capture remains an explicit, separately-invoked diagnostic capability.

### Key Entities

- **VoWiFi Line Registration**: Represents the subscriber line's current reachability state over
  VoWiFi — whether it is currently registered/reachable, when that registration was last
  established, and when it needs renewal.
- **Bridged Call**: Represents one inbound VoWiFi call and its paired SIP/PBX call leg for the
  duration they are connected — who called, when it started/ended, its outcome (answered, declined,
  failed), and which SIP destination it reached.
- **SIP/PBX Destination**: The existing call-routing configuration (already used by the
  circuit-switched bridge) that determines where a bridged call is delivered on the SIP/PBX side.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An inbound VoWiFi call is answered and bridged to the SIP side within 5 seconds of
  arriving, matching the responsiveness already expected of the circuit-switched bridge.
- **SC-002**: In a bridged call, both directions of audio remain intelligible and without
  perceptible dropouts for calls of at least 5 minutes' duration.
- **SC-003**: After a network interruption to the underlying carrier path, VoWiFi reachability is
  automatically restored, and the line is able to answer a call again, within 90 seconds of the
  network path being restored — with zero manual operator steps. (This mirrors the existing
  circuit-switched bridge's 60-second network-loss-detection window, with headroom added for
  VoWiFi's additional IMS-AKA/IPsec re-handshake.)
- **SC-004**: An operator can determine whether the VoWiFi line is currently healthy and view the
  outcome of the most recent inbound call attempt in under 30 seconds, using the same operational
  tooling already used for the circuit-switched line.
- **SC-005**: Across a run of 20 consecutive test calls placed under normal network conditions, at
  least 95% are answered and bridged successfully (the remainder failing only for reasons outside
  the bridge's control, e.g., carrier-side call delivery issues).

## Assumptions

- The subscriber line's VoWiFi/IMS service is provisioned and enabled by the carrier; the bridge
  does not provision VoWiFi service itself, only uses it once available.
- Real-world validation is limited to whichever carrier currently permits this SIM's VoWiFi
  registration to succeed. A carrier that is known to block VoWiFi registration for this line by
  network-side policy is out of scope for this feature until that restriction is lifted on the
  carrier's side; this is a pre-existing constraint of the underlying VoWiFi capability, not a
  behavior this feature controls.
- Only one VoWiFi call is active at a time (single-line assumption, consistent with a single SIM);
  simultaneous multi-call handling on the VoWiFi path is out of scope for this feature.
- This feature covers inbound calls only (VoWiFi caller → SIP/PBX). Placing outbound calls from the
  SIP/PBX side out over VoWiFi is out of scope.
- The existing circuit-switched GSM-to-SIP bridge remains available and unmodified; VoWiFi is an
  additional, independent inbound path on the same line, not a replacement. The carrier network is
  assumed to ensure a line is never delivered simultaneous circuit-switched and VoWiFi calls, so
  the bridge does not need cross-path coordination logic.
- SIP/PBX-side call routing (which extension or number a bridged call reaches) reuses the same
  configuration and defaults already established for the circuit-switched bridge, rather than
  introducing a separate configuration scheme.
- Production calls are not recorded by default, consistent with normal expectations for a live
  phone bridge; any audio capture for diagnostics remains a separate, explicitly-invoked capability
  rather than always-on behavior.
- "Operational status tooling" refers to whatever status/health-reporting mechanism the bridge
  already exposes for the circuit-switched line; this feature extends that existing mechanism
  rather than introducing a new one.
