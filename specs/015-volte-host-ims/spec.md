# Feature Specification: Host-Side IMS Registration over LTE (VoLTE)

**Feature Branch**: `015-volte-host-ims`
**Created**: 2026-07-22
**Status**: Draft
**Input**: User description: "Host-side IMS registration over LTE (VoLTE), reusing the existing IMS stack from the VoWiFi path. Perform SIP REGISTER with IMS-AKA and Gm security from our own software over an LTE IMS PDN, instead of relying on the modem's internal IMS stack whose audio bridging quality is poor. Scope for this feature is registration only — prove a successful registration. Calls are a follow-up."

## Overview

The bridge today reaches the carrier's IMS core in two ways, and neither is satisfactory for cellular voice:

1. **Wi-Fi calling (VoWiFi)** — the bridge runs its own IMS stack over a secure tunnel to the carrier's Wi-Fi calling gateway. This works well and is in production, but it depends on the quality of the internet path.
2. **Cellular voice (VoLTE)** — the bridge delegates entirely to the modem's built-in IMS stack and receives only a decoded audio stream. This path works but delivers **poor audio quality**, because the bridge has no visibility into or control over the codec, jitter handling, or media negotiation, and must re-bridge an already-degraded audio stream.

This feature establishes a third path: the bridge performs **its own IMS registration over the cellular network**, in exactly the way it already does for Wi-Fi calling. The carrier's IMS core, the subscriber identity, the authentication method, and the signalling security are all the same — only the underlying network attachment differs. Doing so brings cellular voice under the same software control that already makes Wi-Fi calling reliable and observable, and removes the degraded audio hand-off entirely.

**This feature delivers registration only.** Establishing that the carrier accepts an authenticated registration from the bridge over cellular is the gating unknown; placing and receiving calls over that registration is deliberately deferred to a follow-up feature.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Attach the bridge to the carrier's IMS network over cellular (Priority: P1)

An operator with a cellular modem and an active SIM wants the bridge itself — not the modem's internal software — to hold a network attachment dedicated to the carrier's IMS service. Today the modem keeps that attachment private; the operator has no way to send or receive IMS traffic from their own software over cellular.

**Why this priority**: Every other part of the feature is impossible without it. It is also the step that proves the carrier is willing to grant an IMS attachment to software the operator controls, which is the single largest unknown in the whole effort.

**Independent Test**: With a SIM inserted and the modem attached to the cellular network, the operator issues one command. The bridge reports that it holds an address on the carrier's IMS network and names the carrier-assigned IMS service identifier. This is verifiable on its own and delivers immediate value as a diagnostic, even before any registration is attempted.

**Acceptance Scenarios**:

1. **Given** a modem registered on the cellular network with a provisioned SIM, **When** the operator requests an IMS network attachment, **Then** the bridge obtains an address on the carrier's IMS network and reports the carrier-assigned IMS service name and the address family in use.
2. **Given** an IMS attachment is already active, **When** the operator requests one again, **Then** the bridge reuses the existing attachment rather than creating a duplicate, and reports that it did so.
3. **Given** the carrier refuses the IMS attachment, **When** the operator requests one, **Then** the bridge reports the refusal and the reason the network gave, and leaves the operator's other connectivity as it found it.
4. **Given** an IMS attachment is active, **When** the operator releases it, **Then** the attachment is torn down and any network configuration the bridge applied is reverted.

---

### User Story 2 - Determine the IMS entry point, and report definitively when the carrier provides none (Priority: P3)

Before the bridge can register, it must know the address of the carrier's IMS entry point. On Wi-Fi calling this address is handed over during tunnel setup. Over cellular, the modem does not expose it — so the bridge probes the standard mechanisms, reports exactly what the carrier returned for each, and otherwise uses an operator-supplied address.

**Why this priority**: *Demoted from P1 to P3 after live investigation* (recorded in `plan.md` and `research.md` R2). Every standard mechanism for publishing an entry-point address was tried against the live carrier and **none yields one** — each either answers with nothing useful or cannot be used at all. Automatic discovery therefore cannot be a prerequisite for anything else. Its remaining value is **diagnostic**: running the probes and reporting exactly what the carrier returned is what makes a future firmware, carrier, or SIM behaving differently discoverable without a code change. Worth building, but last.

**Independent Test**: With an IMS attachment active, the operator runs the discovery command and gets a per-method report of what each mechanism returned. Success is a *complete and accurate report*, not necessarily a discovered address.

**Acceptance Scenarios**:

1. **Given** an active IMS attachment, **When** the operator requests entry-point discovery, **Then** the bridge attempts each supported method in a defined order and reports a per-method breakdown of what each returned.
2. **Given** one method yields nothing, **When** discovery runs, **Then** the bridge proceeds to the next method rather than failing, and records the empty result.
3. **Given** every method fails — **the expected outcome on the currently tested carrier** — **When** discovery completes, **Then** the bridge reports each method's result distinctly and directs the operator to supply an address, rather than presenting the failure as a fault.
4. **Given** the operator has supplied an entry-point address in configuration, **When** the bridge needs one, **Then** the configured address is used and reported as the source in effect.
5. **Given** a method returns a result where it previously returned none, **When** discovery runs, **Then** that address is used and attributed to the method that produced it — so improved firmware or a different carrier is picked up without code changes.

---

### User Story 3 - Register with the carrier's IMS core over cellular (Priority: P2)

With an attachment and an entry-point address in hand, the operator wants the bridge to authenticate to the carrier's IMS core using the SIM's credentials and obtain an accepted registration — the same outcome the bridge already achieves over Wi-Fi calling, but over cellular.

**Why this priority**: This is the feature's headline outcome, but it depends on both preceding stories. Sequencing it second lets the two risky prerequisites be proven first.

**Independent Test**: With an attachment and a known entry point, the operator runs the register command and observes the carrier accepting the registration. Directly verifiable and is the feature's definition of done.

**Acceptance Scenarios**:

1. **Given** an active IMS attachment and a known IMS entry point, **When** the operator initiates registration, **Then** the bridge authenticates using the SIM's credentials and the carrier accepts the registration.
2. **Given** the carrier requires protected signalling before accepting a registration, **When** the bridge registers, **Then** it negotiates and establishes signalling protection and completes the registration over the protected channel.
3. **Given** the carrier rejects the registration, **When** the attempt completes, **Then** the bridge reports the rejection reason and the stage at which it occurred, distinguishing an identity or credential problem from a signalling-protection or transport problem.
4. **Given** the SIM's subscriber identity is available, **When** the bridge registers, **Then** it derives its IMS identities from that subscriber identity in the same manner already proven on the Wi-Fi calling path.

---

### User Story 4 - Keep the cellular registration alive and observable (Priority: P3)

An operator running the bridge over time wants the cellular IMS registration to renew itself before it lapses, and wants to see its current state alongside the existing Wi-Fi calling status.

**Why this priority**: Valuable for any sustained use, but a single successful registration already proves the concept. Deferring this keeps the first increment small.

**Independent Test**: Leave a registration running past its expiry window and confirm it renews without operator action, and that its state is visible in the bridge's status output.

**Acceptance Scenarios**:

1. **Given** an accepted registration with a known expiry, **When** the expiry approaches, **Then** the bridge renews it before it lapses and the registration remains continuously accepted.
2. **Given** a registration is active, **When** the operator requests bridge status, **Then** the cellular IMS registration state is reported alongside the existing Wi-Fi calling state, using consistent terminology.
3. **Given** a renewal fails, **When** the failure occurs, **Then** the bridge reports the failure and retries on a bounded schedule rather than silently dropping the registration.

---

### Edge Cases

- **Carrier grants only one address family.** The carrier's IMS service may be reachable on only one address family, which may differ from the one the Wi-Fi calling path uses. The bridge must work with whichever family the carrier assigns, and must report clearly if it is handed a family it cannot operate on.
- **All entry-point discovery methods fail.** The feature must degrade to a clearly-reported dead end with per-method diagnostics, and must accept an operator-supplied address as an escape hatch, rather than failing opaquely.
- **The modem's own IMS software competes for the attachment.** If the modem's built-in IMS stack is active, it may claim the IMS attachment first or interfere with the bridge's. The bridge must detect contention and report it rather than producing confusing downstream failures.
- **The IMS attachment displaces the operator's general internet connectivity.** On hardware with a single data path to the host, dedicating it to IMS may remove general connectivity. The bridge must state this consequence before applying it and restore the prior arrangement on teardown.
- **Concurrent access to the modem's control channel.** Establishing the attachment and reading SIM credentials both require the modem's control channel. Simultaneous use must not corrupt either operation.
- **The attachment drops mid-registration** (loss of cellular coverage, modem reset, SIM removal). The bridge must notice, report the cause, and not leave stale network configuration behind.
- **The carrier demands protected signalling the bridge did not offer.** The bridge must recognise this specific rejection and report it distinctly, since it is actionable and differs from a credential failure.
- **Registration succeeds but the carrier immediately deregisters** the bridge — for instance because the same identity is registered elsewhere. The bridge must surface this rather than reporting a false success.

## Requirements *(mandatory)*

### Functional Requirements

**IMS network attachment**

- **FR-001**: The bridge MUST be able to establish a network attachment to the carrier's IMS service over cellular, separate from any general-purpose internet attachment.
- **FR-002**: The bridge MUST make that attachment usable from its own software, such that it can send and receive IMS signalling traffic directly.
- **FR-003**: The bridge MUST report, for an established attachment, the carrier-assigned IMS service name, the address the carrier assigned, and the address family in use.
- **FR-004**: The bridge MUST detect an already-established attachment and reuse it rather than creating a duplicate.
- **FR-005**: The bridge MUST provide an explicit teardown that releases the attachment and reverts any host network configuration it applied.
- **FR-006**: The bridge MUST report when establishing the attachment will displace existing connectivity, before it does so.
- **FR-024**: The bridge MUST configure the host interface using the interface identifier the network assigned to the attachment, and MUST NOT rely on host-generated addressing. *(Added after the Gate G1 spike: the carrier addresses its router advertisements to the assigned identifier, so a host-generated one causes them to be silently discarded and leaves the attachment unusable — see `research.md` R7.)*

**IMS entry-point discovery**

- **FR-007**: The bridge MUST attempt automatic discovery of the carrier's IMS entry-point address and MUST report the result of each attempt. It MUST NOT require discovery to succeed in order to proceed — an operator-supplied address (FR-010) is a fully supported source, and on the currently tested carrier it is the only one that works.
- **FR-008**: The bridge MUST support more than one discovery method and MUST attempt them in a defined, documented order, proceeding to the next when one yields no result.
- **FR-009**: The bridge MUST report which discovery method produced the address it is using.
- **FR-010**: The bridge MUST accept an operator-supplied IMS entry-point address that overrides automatic discovery.
- **FR-011**: When all discovery methods fail, the bridge MUST report a per-method breakdown of what was attempted and what each returned.

**Registration**

- **FR-012**: The bridge MUST authenticate to the carrier's IMS core using credentials held on the SIM, without requiring any separately provisioned secret.
- **FR-013**: The bridge MUST derive its IMS identities from the SIM's subscriber identity using the same rules already proven on the Wi-Fi calling path.
- **FR-014**: The bridge MUST negotiate and establish protected signalling with the carrier when the carrier requires it, and complete registration over that protected channel.
- **FR-015**: The bridge MUST report the outcome of a registration attempt, and on failure MUST identify the stage that failed, distinguishing at minimum: attachment failure, entry-point discovery failure, credential or identity rejection, and signalling-protection failure.
- **FR-016**: The bridge MUST renew an accepted registration before it expires, and MUST retry a failed renewal on a bounded schedule.

**Reuse and non-regression**

- **FR-017**: The bridge MUST use one common implementation of IMS registration, authentication, and signalling protection for both the Wi-Fi calling path and the cellular path, with only the underlying network attachment differing between them.
- **FR-018**: The network attachment MUST be a substitutable component, such that the Wi-Fi calling attachment and the cellular attachment satisfy the same contract and either can be selected without altering registration behaviour.
- **FR-019**: The existing Wi-Fi calling path MUST continue to behave exactly as it does today, with no change to its configuration, its operational commands, or its observable behaviour.
- **FR-020**: The bridge MUST operate correctly when the carrier's IMS service is reachable on an address family that the Wi-Fi calling path does not exercise.

**Operability**

- **FR-021**: The operator MUST be able to run attachment, discovery, and registration as separate, individually invokable steps, so that each can be diagnosed in isolation.
- **FR-022**: The bridge MUST report cellular IMS registration state in its status output alongside the existing Wi-Fi calling state, using consistent terminology.
- **FR-023**: The bridge MUST record enough detail about each registration attempt for an operator to diagnose a failure without re-running it with additional instrumentation.

### Key Entities

- **IMS Network Attachment**: A dedicated connection to the carrier's IMS service over cellular, distinct from general internet connectivity. Carries a carrier-assigned service name, an assigned address, an address family, and a lifecycle state.
- **IMS Entry Point**: The address and port of the carrier node the bridge registers against, together with the method by which it was discovered.
- **IMS Identity**: The private identity used for authentication and the public identity the bridge registers, both derived from the SIM's subscriber identity and the carrier's home network.
- **Registration Session**: An accepted registration, carrying its expiry, its renewal schedule, its current state, and the identity it was established for.
- **Transport Provider**: The substitutable component that produces a ready-to-use network attachment and an entry-point address for the registration machinery. Wi-Fi calling and cellular are two implementations.
- **Signalling Protection Association**: The negotiated security state protecting signalling between the bridge and the carrier's entry point.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Starting from an idle modem with a provisioned SIM, an operator can obtain an IMS network attachment with a single command, in under 60 seconds, without supplying any carrier-specific address by hand.
- **SC-002**: Every entry-point discovery run produces a per-method report of what the carrier returned, and the endpoint actually in use is always identified together with its source. *(Revised after the Gate G1 spike — the original criterion required the address to be found automatically with no hand-entered value, which investigation established is not achievable on the tested carrier.)*
- **SC-003**: The bridge obtains an accepted IMS registration over cellular within 60 seconds of the operator initiating it.
- **SC-004**: An accepted registration survives at least two consecutive automatic renewals without operator intervention.
- **SC-005**: Every failure mode exercised during testing produces a report that names the failing stage, and an operator can act on that report without re-running with extra instrumentation.
- **SC-006**: The Wi-Fi calling path shows zero behavioural regressions: its entire existing automated test suite passes unchanged, and a live Wi-Fi call completes successfully after the change.
- **SC-007**: The registration, authentication, and signalling-protection logic is shared between the two paths, with no duplicated implementation of any of the three.
- **SC-008**: An operator can determine, from status output alone, whether the cellular IMS registration is currently accepted.

## Assumptions

**Scope boundaries**

- Placing and receiving calls over the cellular IMS registration is **out of scope**. Media handling, codec negotiation, and audio bridging are deferred to a follow-up feature. This feature ends at an accepted registration.
- Messaging over the cellular IMS registration is out of scope.
- Integration with the bridge's multi-card topology is out of scope. This feature targets a single modem, since a single successful registration is the goal. Multi-card support is a follow-up concern.
- The existing modem-internal cellular voice path remains available and unchanged. This feature adds a parallel capability; it does not replace or remove the current path.

**Environment and dependencies**

- A cellular modem with a provisioned SIM on a carrier that offers IMS voice service is attached to the test machine and registered on the cellular network.
- The carrier will grant an IMS network attachment to a host-controlled request. This was **confirmed by live investigation** on the target hardware and carrier before this specification was written; it is the assumption the feature most depends on.
- The modem in use does **not** expose the carrier's IMS entry-point address through any interface it offers, **and neither does the carrier through any standard network mechanism**. Confirmed by live investigation (Gate G1): DHCPv6 returns no SIP-server options, the router advertisement carries no entry-point option, and no usable resolver is provided. An operator-supplied address is consequently the working source, and obtaining one is tracked as a separate gate in `plan.md`.
- The carrier's IMS service is reachable on a single address family, which differs from the one the Wi-Fi calling path currently exercises. **A code audit found the existing signalling stack already handles both families** at every socket bind and address-formatting site, so this is verification work rather than new implementation — with one exception: the signalling-protection layer has never been exercised on this family and must be verified independently (`plan.md` Gate G2).
- The network assigns the attachment an interface identifier and expects the host to use it. Host-generated addressing leaves the attachment unusable (FR-024).
- The modem's built-in IMS software is idle and not competing for the IMS attachment. If this changes, contention handling (see Edge Cases) applies.
- Privileged network operations run inside a privileged container, following the arrangement the existing Wi-Fi calling path already uses. No new privilege model is introduced.
- Establishing the IMS attachment may consume the modem's single host-facing data path, displacing general internet connectivity through the modem. This is accepted for this feature, since the target hardware's data path is otherwise unused. FR-006 requires the bridge to state this before doing it.

**Reasonable defaults chosen**

- If automatic entry-point discovery fails entirely, an operator-supplied address (FR-010) is the escape hatch, rather than the feature failing outright.
- Registration renewal timing follows the expiry the carrier returns, using the same policy the Wi-Fi calling path already applies, rather than introducing a separate schedule.
- Diagnostic output follows the conventions already established by the Wi-Fi calling path, so operators do not learn a second vocabulary.
