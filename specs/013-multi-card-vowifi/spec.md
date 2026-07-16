# Feature Specification: Multi-Card VoWiFi

**Feature Branch**: `013-multi-card-vowifi`
**Created**: 2026-07-14
**Status**: Draft
**Input**: User description: "we have multi card support for the usb audio flow, but i dont think we have that capability for the vowifi, comeup with a plan to build the capability. bare minimum, we need to have auto discover capability for the AT command compatible interfaces"

## Context

The circuit-switched (USB-audio) call path has handled multiple modem cards since feature 004: at
startup the system scans the USB bus, finds every attached card, and bridges inbound calls from all
of them concurrently. The VoWiFi path (features 011 and 012) never gained that capability. Today it
serves exactly **one** SIM, for three reasons:

1. **No discovery.** The operator hand-types the VoWiFi modem's serial port into configuration. The
   VoWiFi-capable module cannot even be auto-detected: the existing scanner deliberately skips it,
   because that scanner only recognizes cards that also expose a USB audio device.
2. **Singleton tunnel resources.** One network namespace, one tunnel interface identity, one
   internal link, one P-CSCF file, one virtual-SIM-reader port, one control port — all fixed.
3. **Singleton agents.** Startup launches exactly one of each VoWiFi process.

Feature 012 anticipated this (its FR-013, "multi-ready, single-line") by keeping every one of those
resources parametrized rather than hardcoded. This feature cashes that in.

An ePDG tunnel is bound one-to-one to a SIM by its authentication, so **N SIMs means N tunnels**.
The unit this feature introduces is the **VoWiFi line**: one SIM, one tunnel, one IMS registration.

## Clarifications

### Session 2026-07-14

- Q: With N lines, what determines the destination the PBX sees for a bridged VoWiFi call? → A: Keep today's semantics per line — empty `sip_destination` means DID passthrough (the number the caller dialled, i.e. that SIM's own number); a non-empty value is a fixed extension shared by all lines. Line attribution rides in the card identifier reported in logs/SMS/metrics, not in SIP routing.
- Q: VoWiFi enabled but discovery finds zero usable lines — fail or degrade? → A: Degrade. Log a prominent error, skip the VoWiFi subsystem, and let the circuit-switched daemon start and serve its cards. The container stays up rather than crash-looping on an unplugged modem; the health check reports VoWiFi as down without failing the container. (Replaces today's fatal-exit-on-missing-modem-port behavior.)

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Auto-Discovery of AT-Capable Modems (Priority: P1)

An operator attaches one or more VoWiFi-capable modems to the host and starts the system without
naming any serial port in configuration. The system scans the USB bus, and for each recognized
modem it determines which of that modem's serial interfaces actually accepts AT commands — by
trying them, not by assuming a fixed interface number, since that number varies by model and
firmware. For each modem that responds, the system reads the SIM's identity and reports the modem,
its AT port, and its SIM. Modems that expose no working AT port, or whose SIM is absent or locked,
are reported as failed and skipped; the system continues with the remaining ones.

**Why this priority**: This is the minimum capability the operator asked for, and it is the
foundation every other story depends on — without it there is no way to know which modems and SIMs
exist, so no way to build a per-SIM line table.

**Independent Test**: Attach a VoWiFi-capable modem, leave the modem port unset in configuration,
start the system, and verify it logs the discovered modem, the AT port it settled on, and the SIM
identity — with no hand-typed device path anywhere.

**Acceptance Scenarios**:

1. **Given** one VoWiFi-capable modem is attached and no modem port is configured, **When** the
   system starts, **Then** it discovers the modem, identifies its AT-capable serial interface, reads
   the SIM identity, and brings up VoWiFi on it.
2. **Given** two VoWiFi-capable modems are attached, **When** the system starts, **Then** it
   discovers both, with a distinct AT port and distinct SIM identity for each.
3. **Given** a modem is attached with no SIM inserted, **When** the system starts, **Then** it
   reports that modem as unusable for VoWiFi and continues with the remaining modems.
4. **Given** a modem's AT interface number differs from the one previously assumed for its model,
   **When** the system starts, **Then** discovery still finds the working AT port by probing.
5. **Given** an existing configuration that names a modem port explicitly, **When** the system
   starts, **Then** that port is used as-is and discovery does not override it.

---

### User Story 2 - One VoWiFi Line Per SIM, Concurrently (Priority: P2)

Every discovered VoWiFi SIM gets its own complete line: its own carrier tunnel, its own IMS
registration, and its own inbound call path to the PBX. Lines are independent — a call arriving on
one SIM is answered and bridged while the other SIMs stay registered and ready, and two calls
arriving on two SIMs at the same time are both bridged concurrently with independent audio.

**Why this priority**: This is the actual capability parity with the circuit-switched path. It
depends on discovery (P1) to know what the lines are.

**Independent Test**: With two VoWiFi SIMs on different carriers, verify two tunnels come up and two
IMS registrations succeed, then call each SIM's number and confirm each is bridged to the PBX —
including both at the same time.

**Acceptance Scenarios**:

1. **Given** two VoWiFi SIMs are discovered, **When** the system starts, **Then** two independent
   carrier tunnels are established and both SIMs reach a registered state.
2. **Given** two lines are registered, **When** a call arrives on line 1, **Then** it is answered and
   bridged to the PBX while line 2 remains registered and idle.
3. **Given** two lines are registered, **When** calls arrive on both within seconds of each other,
   **Then** both are bridged concurrently with independent audio and no cross-talk.
4. **Given** two lines are registered, **When** one line's tunnel drops and reconnects, **Then** the
   other line's tunnel, registration, and any in-progress call are unaffected.
5. **Given** one line's SIM fails to authenticate with its carrier, **When** the system starts,
   **Then** the remaining lines still come up and serve calls.
6. **Given** exactly one SIM is present, **When** the system starts, **Then** behavior is
   indistinguishable from the current single-line system.

---

### User Story 3 - Per-Line Identification and Status (Priority: P3)

Every VoWiFi log line, metric, status report, and forwarded SMS identifies which card and SIM it
came from, using the same stable card identifier the circuit-switched path already uses. A single
status command reports every line's tunnel and registration health, not just one.

**Why this priority**: Operating a multi-line deployment without per-line attribution is impractical
— but the system still functions correctly without it.

**Independent Test**: With two lines active, run the status command and verify both lines are listed
with their own tunnel/registration state; trigger an SMS on each SIM and verify each is attributed
to the correct card.

**Acceptance Scenarios**:

1. **Given** two lines are active, **When** the operator runs the VoWiFi status command, **Then** it
   reports tunnel and registration state for **both** lines, each labeled with its card identifier.
2. **Given** two lines are active, **When** a call or SMS arrives on one, **Then** every log entry
   and forwarded notification names that line's card identifier, not a generic label.
3. **Given** two lines are active, **When** metrics are scraped, **Then** VoWiFi metrics are
   distinguishable per line.

---

### Edge Cases

- **No VoWiFi-capable modem found** while VoWiFi is enabled: report a clear error naming the
  condition; the circuit-switched bridge must still start and serve its own cards.
- **Modem claimed by both subsystems**: a modem must serve exactly one role. Two subsystems opening
  the same serial port at once corrupts both.
- **A SIM is swapped between restarts**: line identity follows the SIM, so the line's carrier
  identity, tunnel, and registration follow the new SIM.
- **A modem is unplugged while running**: its line fails and is reported; the other lines continue.
- **A modem is added while running**: not picked up until restart (discovery runs at startup).
- **Two SIMs from the same carrier**: both lines must come up — nothing may assume one line per
  carrier, or a single home network across lines.
- **A SIM is PIN-locked or not yet ready** at probe time: report and skip rather than hang.
- **More modems than the system can support**: the number of lines is bounded; excess modems are
  reported and skipped rather than silently dropped.

## Requirements *(mandatory)*

### Functional Requirements

**Discovery**

- **FR-001**: The system MUST discover attached VoWiFi-capable modems automatically, without the
  operator naming a serial device path.
- **FR-002**: For each discovered modem, the system MUST determine its AT-capable serial interface
  by probing the modem's serial interfaces for a valid AT response, rather than assuming a fixed
  interface number per model.
- **FR-003**: The system MUST recognize modems that expose no USB audio device (VoWiFi-only models),
  which the existing circuit-switched scanner deliberately excludes today.
- **FR-004**: The system MUST read each modem's SIM identity during discovery and use it as the
  line's identity.
- **FR-005**: The system MUST assign each modem a stable identifier derived from its hardware serial
  number, consistent across restarts, reusing the identifier scheme the circuit-switched path
  already uses.
- **FR-006**: A modem that fails discovery (no AT-capable interface, no SIM, SIM not ready) MUST be
  reported with a reason and skipped, without preventing the remaining modems from being used.

**Role assignment**

- **FR-007**: Each modem MUST be assigned exactly one role — circuit-switched **or** VoWiFi — so that
  no two subsystems open the same serial port.
- **FR-008**: By default the system MUST assign VoWiFi-only modems (those with no USB audio path) to
  VoWiFi, leaving audio-capable cards to the circuit-switched bridge.
- **FR-009**: The operator MUST be able to override that default by naming explicitly which modems
  serve VoWiFi.

**Multi-line operation**

- **FR-010**: The system MUST support one VoWiFi line (SIM + tunnel + IMS registration + call path)
  per discovered SIM, running concurrently.
- **FR-011**: Each line MUST have its own isolated set of runtime resources — network namespace,
  tunnel interface identity, internal link and addresses, control channel, virtual SIM reader, and
  carrier-address file — with no collision between lines.
- **FR-012**: Each line MUST derive its own home network identity (country/network code) from its own
  SIM; the system MUST NOT assume a single home network shared across lines.
- **FR-013**: A line's failure (tunnel down, authentication failure, registration failure, modem
  removed) MUST NOT affect any other line's tunnel, registration, or in-progress call.
- **FR-014**: Each line MUST retain the existing per-line resiliency behavior (unbounded tunnel
  retry, re-authentication, keepalive, reconnect) established in feature 012.
- **FR-015**: Concurrent inbound calls on different lines MUST be bridged simultaneously with
  independent audio paths and no cross-talk.
- **FR-016**: The number of concurrently supported lines MUST be bounded, and modems beyond that
  bound MUST be reported and skipped.

**Observability**

- **FR-017**: Every VoWiFi log entry, metric, and forwarded SMS MUST identify its originating line by
  card identifier, replacing today's single generic VoWiFi label.
- **FR-017a**: A bridged VoWiFi call MUST use the same destination semantics per line that a
  single-line deployment uses today: when no fixed SIP destination is configured, the call passes
  through the number the caller dialled (that SIM's own number); when one is configured, every line
  routes to it. Line attribution MUST be carried by the card identifier (FR-017), not by SIP routing.
- **FR-018**: The VoWiFi status command MUST report tunnel and registration state for every line.
- **FR-019**: The health check MUST consider every line, not only the first.

**Compatibility**

- **FR-020**: An existing single-SIM configuration MUST continue to work unchanged, resolving to
  exactly one line whose externally observable behavior matches today's.
- **FR-021**: The circuit-switched multi-card bridge's existing behavior (feature 004) MUST be
  unchanged by this feature.

### Key Entities

- **Modem**: A physical module attached over USB. Attributes: stable card identifier, model,
  hardware serial, AT-capable serial port, optional audio device, role (circuit-switched or VoWiFi).
- **SIM**: The subscriber identity inside a modem. Attributes: subscriber identity, home network
  (country code + network code). Bound one-to-one to a modem.
- **VoWiFi Line**: The unit this feature introduces — one SIM plus the full set of runtime resources
  needed to carry its calls: a carrier tunnel, an IMS registration, an isolated network context, and
  a bridge to the PBX. One line per SIM; lines are independent and individually recoverable.
- **Line Table**: The startup-resolved, deterministically ordered set of lines, derived from
  discovery plus configuration. Each line's resources are a function of its position in this table,
  so they are stable across restarts and never collide.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With VoWiFi-capable modems attached and no serial port named in configuration, the
  system discovers every one of them and reports each modem's AT port and SIM — the operator types
  no device path.
- **SC-002**: With 2 VoWiFi SIMs attached, 2 independent carrier tunnels are established and both
  SIMs reach a registered state within the same startup window a single SIM takes today.
- **SC-003**: An inbound call to **either** SIM is answered and bridged to the PBX within 5 seconds,
  including ≥ 12 hours after startup (matching feature 012's SC-003, now per line).
- **SC-004**: Two inbound calls arriving on two SIMs within 5 seconds of each other are both bridged
  concurrently, each with intelligible two-way audio and no cross-talk.
- **SC-005**: Forcing one line's tunnel down leaves the other line's registration intact and its
  in-progress call unaffected; the failed line recovers on its own within 90 seconds of the fault
  clearing (matching feature 012's SC-002, now per line).
- **SC-006**: Every line sustains ≥ 24 hours of unattended uptime spanning at least one carrier
  rekey, with zero agent restarts (matching feature 012's SC-001, now for all lines at once).
- **SC-007**: An operator can tell, from status output and logs alone, which card and SIM any VoWiFi
  call, SMS, or failure belongs to — with no ambiguity between lines.
- **SC-008**: An unchanged single-SIM configuration produces exactly one line whose behavior is
  indistinguishable from today's, with no operator-visible migration step.

## Assumptions

- **Discovery is a startup activity.** Modems are enumerated once at startup, before any subsystem
  claims a serial port. Hot-plugging a new modem into a running system is out of scope (matching the
  circuit-switched path, which also discovers at boot).
- **A modem serves one role.** The default split is by capability: modules with no USB audio path are
  VoWiFi-only and go to VoWiFi; audio-capable cards stay with the circuit-switched bridge. A modem
  that could serve either is not shared — an explicit operator override decides.
- **Line identity follows the SIM**, and line ordering is derived deterministically from the modems'
  hardware serial numbers, so a line's resources are stable across restarts.
- **Line count is bounded** by the same order of magnitude as the existing circuit-switched card
  limit (8); this is a small-deployment feature, not a carrier-scale one.
- **One call at a time per line.** Feature 011/012's single-call-per-line limit is unchanged; this
  feature adds concurrency *across* lines, not within one.
- **The PBX accepts calls from all lines over one connection.** The bridge presents a single SIP
  identity to the PBX and distinguishes lines by the card identifier it reports, rather than
  registering once per line. Call destination follows today's per-line semantics (FR-017a): DID
  passthrough by default, or one shared fixed extension if configured.
- **Existing tunnel and IMS behavior is reused per line**, not redesigned: this feature replicates
  and isolates the feature-012 line, it does not change what a line does.
- **Live multi-SIM verification is operator-run** on real hardware with real carrier SIMs, the same
  boundary features 003–012 draw; automated tests cover discovery, line-table resolution, and
  configuration without hardware.
