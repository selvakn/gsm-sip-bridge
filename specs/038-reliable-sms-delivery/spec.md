# Feature Specification: Reliable SMS Delivery

**Feature Branch**: `038-reliable-sms-delivery`
**Created**: 2026-08-16
**Status**: Draft
**Input**: User description: "this feature, we want reliable sms delivery for all modes, vowifi with or without cs enabled, volte with or wihtout cs enabled, also with cs only mode"

## User Scenarios & Testing *(mandatory)*

<!--
  IMPORTANT: User stories should be PRIORITIZED as user journeys ordered by importance.
  Each user story/journey must be INDEPENDENTLY TESTABLE - meaning if you implement just ONE of them,
  you should still have a viable MVP (Minimum Viable Product) that delivers value.
-->

### User Story 1 - No SMS is silently lost on a VoWiFi- or VoLTE-only line (Priority: P1)

The operator runs a line with VoWiFi or VoLTE as its only active call path (circuit-switched handling turned off for that line). A carrier sometimes delivers a text — including one part of a multi-part text — through the classic cellular SMS bearer instead of over the IMS registration. Today that text lands in the modem's own storage and is never read by anything, so the operator never sees it, in Discord or in the stored history. This story makes every such text reach the operator regardless of which bearer carried it.

**Why this priority**: This is an active, confirmed data-loss bug (a 7-part SMS where 6 of 7 parts were sitting unread in modem storage, never forwarded). Losing a subscriber's texts silently — with no error, no alert — is the worst failure mode a messaging feature can have.

**Independent Test**: Configure a line with `[cs].enabled = false` and VoWiFi (or VoLTE) enabled. Have the carrier (or a test harness standing in for one) deliver a text through the modem's own storage rather than the IMS registration. Confirm the text appears in the operator's Discord channel and in the stored SMS history, without any other subsystem being enabled.

**Acceptance Scenarios**:

1. **Given** a VoWiFi-only line (`[cs].enabled = false`), **When** the carrier delivers a text into the modem's own SMS storage instead of as an IMS message, **Then** the text is forwarded to the operator (Discord + history) without any manual intervention.
2. **Given** a VoLTE-only line (`[cs].enabled = false`), **When** the carrier delivers a text the same way, **Then** the same guarantee holds.
3. **Given** a multi-part (concatenated) text where the carrier splits delivery across both bearers (some parts over IMS, some through modem storage), **When** all parts arrive, **Then** every part reaches the operator — none are silently dropped because they used the "other" bearer.
4. **Given** texts that accumulated in a line's modem storage before this capability existed or while it was not running (e.g. after a restart), **When** the line comes back up, **Then** those backlogged texts are also delivered, not just ones that arrive afterward.

---

### User Story 2 - No SMS is shown to the operator twice (Priority: P2)

A carrier occasionally delivers the same text over both bearers for the same line — once over the IMS registration and once into the modem's own storage. The operator must see it exactly once, not twice.

**Why this priority**: A duplicate notification is a real annoyance and erodes trust in the feed, but it is far less harmful than a message that never arrives at all — hence lower priority than Story 1.

**Independent Test**: Simulate the same sender/body text arriving once over the IMS registration and once through modem storage for the same line. Confirm the operator's Discord channel and stored history show it exactly once.

**Acceptance Scenarios**:

1. **Given** a line where both bearers are being watched, **When** the identical text (same sender, same body) arrives over both within a short window, **Then** only one notification and one stored record result.
2. **Given** the same duplicate scenario, **When** the modem-storage copy is the second to arrive, **Then** it is still cleared from the modem's storage (so it does not keep re-appearing on every check) even though it is not forwarded again.

---

### User Story 3 - VoLTE lines keep their existing reliability, with or without CS (Priority: P2)

VoLTE lines already have a working version of this capability (added for specs/017-volte-inbound-bridge). This story is about confirming — and keeping — that guarantee as this capability is generalized to also cover VoWiFi, so unifying the two does not regress the one that already works.

**Why this priority**: Protects an existing, relied-upon capability while the underlying mechanism is shared with VoWiFi. Not new user value on its own, but a regression here would be a step backward.

**Independent Test**: Run a VoLTE line with `[cs].enabled = false`, and separately with `[cs].enabled = true`. In both cases, deliver a text through modem storage and confirm it still reaches the operator exactly as it does today.

**Acceptance Scenarios**:

1. **Given** a VoLTE-only line (`[cs].enabled = false`), **When** a text is delivered through modem storage, **Then** it reaches the operator (unchanged from today).
2. **Given** a VoLTE line where `[cs].enabled = true` but this specific modem is exclusively assigned to VoLTE, **When** a text is delivered through modem storage, **Then** it still reaches the operator — CS being globally on elsewhere does not exempt this line.

---

### User Story 4 - CS-only deployments are unaffected (Priority: P3)

A deployment with no VoWiFi or VoLTE at all — every line handled by the circuit-switched daemon — must keep receiving every SMS exactly as it does today. This capability must not change or interfere with that existing path.

**Why this priority**: Pure regression protection for the oldest, most established delivery path. Lowest priority only because there is no known gap here today — the risk is purely "don't break it while fixing the others."

**Independent Test**: Run a deployment with `[cs].enabled = true` and VoWiFi/VoLTE both disabled. Confirm SMS delivery rate and latency are unchanged from current behavior.

**Acceptance Scenarios**:

1. **Given** a CS-only deployment, **When** a text arrives, **Then** it is forwarded to the operator exactly as before, with no new delay or duplicate.

---

### Edge Cases

- A line's modem SMS storage approaches or reaches capacity because nothing has been able to drain it for an extended period (e.g. the reading capability itself has been failing) — out of scope to guard against proactively (see FR-011); this feature's responsibility is limited to recovering the backlog once reading resumes.
- A line uses a PC/SC card reader instead of a physical modem — there is no cellular bearer to poll at all, so the guarantee in this feature naturally covers the IMS registration route only for that line.
- The process restarts mid-relay (message read from modem storage but not yet forwarded, or forwarded but not yet cleared) — must not result in a lost message, and any resulting retry must be caught by the existing duplicate-suppression behavior rather than shown twice.
- A message can be read from the modem but not decoded/parsed cleanly — it must still reach the operator in whatever form is available rather than being silently dropped, consistent with how the IMS path already handles undecodable bodies.
- Multiple VoWiFi and/or VoLTE lines running concurrently (multi-card deployments) — each line's guarantee holds independently of the others.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: System MUST forward to the operator every inbound SMS a carrier delivers for a line, regardless of which bearer (the IMS registration's message channel, or the line's own modem storage) actually carried it, whenever at least one call-path subsystem (circuit-switched, VoWiFi, or VoLTE) is active for that line.
- **FR-002**: For any line running VoWiFi or VoLTE — whether or not `[cs].enabled` is true — system MUST also check that line's own modem storage for text delivered through the classic cellular bearer, and forward each one found there exactly as if it had arrived over the IMS registration.
- **FR-003**: System MUST NOT forward the same logical message (same sender and body) to the operator more than once when it is delivered redundantly over more than one bearer for the same line.
- **FR-004**: For a line that uses a PC/SC card reader instead of a physical modem, the system's delivery guarantee applies to the IMS registration route only, since no cellular bearer/modem storage exists for that line to poll.
- **FR-005**: In CS-only mode (a line with no VoWiFi/VoLTE enabled), the system MUST continue to receive every SMS delivered to that line's modem exactly as it does today — this feature must not change or regress that existing path.
- **FR-006**: The SMS-delivery guarantee for a VoWiFi- or VoLTE-assigned line MUST hold regardless of `[cs].enabled`'s value elsewhere in the deployment — a globally-enabled circuit-switched daemon never reads a modem that is exclusively assigned to VoWiFi or VoLTE, so this feature cannot rely on CS being on or off to determine whether coverage is needed.
- **FR-007**: System MUST recover SMS that had already accumulated in a line's modem storage before the reading capability started or after it resumes from an outage — not only messages that arrive afterward — so nothing already stuck there is left behind.
- **FR-008**: System MUST preserve the existing behavior for multi-part (concatenated) SMS — each part is decoded and forwarded individually, labeled with its sequence, and not reassembled — regardless of which bearer carried which part.
- **FR-009**: For every delivered SMS, system MUST record which bearer actually carried it (IMS registration vs. modem storage), so delivery-path behavior remains observable and diagnosable after the fact.
- **FR-010**: System MUST detect and forward a text delivered only through a line's modem storage within a bounded, predictable amount of time — a periodic poll of the modem's storage (reusing the existing ~20s interval already proven for VoLTE) is the required mechanism; event-driven detection via the modem's own unsolicited new-message notification is explicitly out of scope for this feature.
- **FR-011**: Proactively guarding against a line's modem SMS storage filling up during an extended outage (e.g. alerting the operator before the carrier starts rejecting new texts) is explicitly out of scope for this feature. System MUST still recover whatever has already accumulated in storage once the reading capability starts or resumes (FR-007) — that backlog recovery is the full extent of this feature's responsibility for storage-capacity concerns.

### Key Entities *(include if feature involves data)*

- **Inbound SMS**: One text delivered by the carrier to a line — sender, body, time received, which bearer carried it, and its forwarding status (pending/sent/failed).
- **Line**: One subscriber identity (SIM/modem or PC/SC card) the bridge serves — has a call-path role (circuit-switched, VoWiFi, or VoLTE) and, where a physical modem is involved, an associated modem SMS storage.
- **Delivery Bearer**: The channel a given SMS actually arrived over — either the IMS registration's message channel, or the line's own modem storage (the classic cellular SMS route).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: In a VoWiFi-only or VoLTE-only deployment (circuit-switched handling off), 100% of SMS the carrier delivers to that line's SIM reach the operator, regardless of which bearer carried them.
- **SC-002**: No SMS is ever shown to the operator more than once, even when the carrier delivers it redundantly over both bearers.
- **SC-003**: A backlog of SMS that accumulated in a line's modem storage while nothing was reading it is fully delivered to the operator within one ~20s poll cycle after the reading capability starts or resumes.
- **SC-004**: CS-only deployments show no measurable change in SMS delivery rate or latency compared to before this feature.
- **SC-005**: A VoLTE line's existing SMS-delivery reliability is unchanged (same guarantee, same or better latency) after this feature generalizes the underlying mechanism to also cover VoWiFi.

## Assumptions

- "Reliable" means every SMS the carrier actually delivers to a line eventually reaches the operator exactly once — it is not a claim about the carrier's own delivery guarantees, which are outside this system's control.
- Multi-part (concatenated) SMS reassembly remains explicitly out of scope, matching existing, documented behavior — each part continues to be decoded and forwarded on its own.
- Each physical modem serves exactly one line's role (circuit-switched, VoWiFi, or VoLTE) at a time — the existing exclusive card-assignment behavior from prior features is assumed to continue unchanged; this feature only closes the gap in what happens to SMS on an already-assigned line, not how assignment itself works.
- This feature applies per line: in multi-card/multi-line deployments, each line's delivery guarantee is independent of every other line's.
- The existing exactly-once safety pattern (record before acknowledging/clearing, bounded duplicate-suppression window) already established for VoLTE's modem-storage reading is the right foundation to extend to VoWiFi, not something this feature needs to redesign.
