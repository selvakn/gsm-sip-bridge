# Feature Specification: PC/SC Card-Reader-Backed VoWiFi Lines

**Feature Branch**: `023-omnikey-pcsc-vowifi`
**Created**: 2026-07-27
**Status**: Draft
**Input**: User description: "Support a physical PC/SC smart-card reader (OmniKey AG 3x21, USB 076b:3031) as a VoWiFi SIM source, alongside existing modem-based VoWiFi lines (mixed deployment). Background: this project's VoWiFi/ePDG tunnel already supports two tunnel engines. The default engine talks to a virtual PC/SC reader bridged to a modem's SIM — there's no real physical PC/SC reader support today. The user has a real PC/SC reader with a SIM inserted directly (PIN disabled, no PIN-verification work needed). The connection engine already auto-detects any connected PC/SC reader with zero extra configuration, matching a reader to a configured line purely by IMSI. This must coexist with existing modem-based VoWiFi lines in the same deployment (shared line-count limit, independent identities), and the engine with no PC/SC support should reject/fail fast if a card-reader line is configured while that engine is selected."

## Clarifications

### Session 2026-07-27

- Q: Should a card-reader-backed line be visible in this project's existing per-line status/metrics/alerting surfaces, and if so, distinguishable as a different SIM source? → A: Fully identical — card-reader lines appear in the same status/metrics/alert surfaces as modem lines, with no visible distinction.
- Q: When a failed card-reader line's card/reader becomes reachable again, should it recover automatically or require a manual service restart? → A: Automatic recovery — the system periodically retries and brings the line back into service on its own.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Register a VoWiFi line from a directly-inserted SIM card (Priority: P1)

An operator has a SIM card seated directly in a physically attached
smart-card reader (no cellular modem involved for this SIM at all). They want
this SIM to become a fully working VoWiFi line — registering to the
carrier's IMS network and able to carry calls — the same way a modem-backed
line already does, without buying or wiring up a modem for it.

**Why this priority**: This is the entire point of the feature. Without it,
there is no way to use a SIM that lives in a card reader rather than a modem.

**Independent Test**: Insert a VoWiFi-provisioned SIM into the attached
reader, add one line entry to configuration naming that SIM's identity,
start the service, and confirm the line reaches a registered state and can
send/receive a test call — with no modem device present or referenced for
that line at all.

**Acceptance Scenarios**:

1. **Given** a SIM seated in an attached card reader and a line configured
   to use it, **When** the service starts, **Then** that line registers to
   the carrier's IMS network using the SIM in the reader, with no modem
   involved.
2. **Given** such a line is registered, **When** an inbound or outbound
   VoWiFi call occurs on it, **Then** the call is handled identically to how
   a modem-backed VoWiFi line handles a call today.
3. **Given** the operator has not supplied the SIM's network identity
   (IMSI/home network) for a card-reader line, **When** the service starts,
   **Then** it reports a clear configuration error for that line instead of
   guessing or silently skipping it.

---

### User Story 2 - Run card-reader and modem-backed lines side by side (Priority: P2)

An operator already runs one or more modem-backed VoWiFi lines and wants to
add a card-reader-backed line to the same deployment without disrupting the
existing lines or having to reconfigure them.

**Why this priority**: The feature has little value if adopting it forces an
all-or-nothing switch away from the existing modem-based setup this project
already supports and has validated against live carriers.

**Independent Test**: Start a deployment with at least one existing
modem-backed line configured as before, add a card-reader line alongside it,
restart, and confirm both lines register and operate independently — a
problem with one does not stop or degrade the other.

**Acceptance Scenarios**:

1. **Given** an existing modem-backed line already in service, **When** a
   card-reader line is added to configuration and the service is restarted,
   **Then** the modem-backed line continues to register and behave exactly
   as before.
2. **Given** both line types are configured, **When** the total number of
   configured lines is counted, **Then** they share the same overall
   maximum-lines limit the deployment already enforces today.
3. **Given** both line types are running, **When** one line's registration
   fails (e.g. the card is removed), **Then** only that line is affected and
   is reported as failed — the other line's service is unaffected.

---

### User Story 3 - Fail fast on an unsupported combination (Priority: P3)

An operator configures a card-reader line while the deployment is set to use
the tunnel connection method that has no support for physical card readers.

**Why this priority**: Silently ignoring or mis-starting a line an operator
believes is active is worse than an immediate, clear rejection — it would
otherwise surface later as a confusing "why isn't this SIM registering"
investigation.

**Independent Test**: Configure a card-reader line under the unsupported
connection method and start the service; confirm it refuses to start (or
clearly refuses that specific configuration) with an actionable message,
rather than starting up while quietly never registering that line.

**Acceptance Scenarios**:

1. **Given** a card-reader line is configured together with the connection
   method that doesn't support physical readers, **When** the service
   starts, **Then** it reports a clear, specific error naming the
   incompatibility rather than starting with that line silently absent.

---

### Edge Cases

- What happens when the configured card reader is unplugged, or no card is
  seated in it, at startup or during operation? The line should be reported
  as failed/unavailable, the same way an unreadable or absent SIM is already
  reported for modem-backed lines today — not a crash of the whole service.
  If the reader/card later becomes reachable again, the line MUST recover
  automatically (periodic retry), with no operator restart required.
- What happens if the SIM in the reader requires a PIN to unlock? Out of
  scope for this feature (see Assumptions) — the line should fail clearly
  rather than hang indefinitely waiting for a PIN that never arrives.
- What happens if a configured card-reader line's network identity doesn't
  match any SIM actually present? That line is reported as failed
  independently, without affecting other lines.
- What happens when the configured maximum-lines limit is already reached
  by modem-backed lines and a card-reader line is added on top? The
  overflow line is reported as failed/excluded exactly as an excess
  modem-backed line is today, not silently dropped.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: System MUST allow an operator to configure a VoWiFi line whose
  SIM identity comes from a physically attached smart-card reader rather
  than from a cellular modem.
- **FR-002**: System MUST register and operate a card-reader-backed line
  (IMS registration, inbound/outbound call handling) using the SIM seated
  in the reader, with no modem present or required for that line.
- **FR-003**: System MUST require the operator to explicitly supply a
  card-reader-backed line's SIM network identity (IMSI and home network)
  in configuration, since it cannot be auto-derived from a modem.
- **FR-004**: System MUST support at least one card-reader-backed line
  operating concurrently with any number of existing modem-backed lines in
  the same deployment.
- **FR-005**: System MUST NOT change the configuration or runtime behavior
  of existing modem-backed lines as a result of this feature being
  available or used.
- **FR-006**: System MUST count card-reader-backed lines against the same
  overall maximum-lines limit that already bounds modem-backed lines,
  rather than treating them as an unbounded separate pool.
- **FR-007**: System MUST treat each line (modem- or card-reader-backed) as
  independently isolated, so that one line's failure or misconfiguration
  does not prevent other lines from registering or operating.
- **FR-008**: System MUST detect when a card-reader-backed line is
  configured under a tunnel connection method that does not support
  physical card readers, and MUST fail with a clear, specific message
  identifying the incompatible line and setting, rather than starting with
  that line silently inactive.
- **FR-009**: System MUST report a card-reader-backed line whose reader or
  card cannot be reached (unplugged, absent, unreadable) as a failed line —
  using the same kind of failure reporting already used for an
  absent/unreadable modem SIM — rather than crashing or hanging the
  deployment.
- **FR-010**: System MUST surface a card-reader-backed line's registration
  status through the same status, metrics, and alerting surfaces already
  used for modem-backed lines, with no operator-visible distinction between
  the two SIM sources in those surfaces.
- **FR-011**: System MUST automatically retry and recover a failed
  card-reader-backed line once its reader/card becomes reachable again,
  without requiring the operator to restart the service.

### Key Entities

- **VoWiFi Line**: One SIM identity that registers to a carrier's IMS
  network for VoWiFi calling and can carry calls. Previously always backed
  by a cellular modem; this feature adds a second kind of backing.
- **SIM Source**: Where a line's SIM material comes from — either a
  cellular modem (existing) or a directly attached physical smart-card
  reader (new). Determines how the line's network identity is obtained and
  what hardware must be present.
- **Tunnel Connection Method**: The existing choice of how a line's traffic
  reaches the carrier's network. One of the two existing methods supports
  card-reader-backed SIM sources; the other does not and must reject that
  combination.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An operator can bring a card-reader-backed VoWiFi line into
  full service (registered and able to carry a call) using configuration
  changes alone.
- **SC-002**: In a mixed deployment, every configured line — regardless of
  SIM source — reaches its registration outcome (success or a clearly
  reported failure) independently; no line's outcome is affected by
  another line's state.
- **SC-003**: 100% of deployments that pair a card-reader line with an
  unsupported connection method are stopped with a clear error before any
  call-handling is attempted, with zero instances of a silently-inactive
  line going unnoticed.
- **SC-004**: Existing modem-backed deployments observe no change in
  configuration requirements or registration behavior after this feature
  becomes available.
- **SC-005**: An operator monitoring the deployment through its existing
  status/metrics/alerting tooling cannot tell a card-reader-backed line
  apart from a modem-backed line by observation alone — both are visible
  and reported identically.

## Assumptions

- The physical card reader is attached to, and reachable by, the same host
  or environment that already runs the VoWiFi service — no new remote or
  networked reader-access scenario is in scope.
- The SIM seated in the reader does not require a PIN to unlock; PIN entry
  is out of scope for this feature, and a PIN-protected card is expected to
  be unlocked (or have its PIN removed) before use.
- Each card-reader-backed line's network identity (IMSI, home network) is
  supplied directly by the operator in configuration rather than
  auto-discovered from the card at startup.
- Exactly one attached reader/card is expected to back one configured line;
  supporting multiple simultaneous readers is a natural extension of the
  same mechanism but each still maps one-to-one to a line.
- Only one of the two existing tunnel connection methods is expected to
  gain card-reader support in this feature; the other remains modem-only
  and must reject the combination clearly (FR-008) rather than support it.
- The reader hardware itself is not restricted to one specific vendor or
  model by this feature — any standards-compliant smart-card reader the
  underlying connection method can already see is in scope, though only one
  specific model has been used to validate it.
