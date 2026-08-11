# Feature Specification: Card Phone Number and Instance Identity in Alerts

**Feature Branch**: `034-alert-identity`
**Created**: 2026-08-11
**Status**: Draft
**Input**: User description: "on the alert notifications (SMS and critical events), it should include the current card's phone number and the hostname where the module is running"

## Clarifications

### Session 2026-08-11

- Q: When a card/line's phone number cannot be determined, what should the notification do with the phone field? → A: Always show the phone field; display the literal value `unknown` when no number is resolved.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Attribute an alert to a specific SIM (Priority: P1)

An operator watching the alert channel receives an SMS forward or a critical-event
alert and can immediately tell **which SIM/line** it concerns by reading the
card's phone number on the notification — without cross-referencing an internal
card id against a spreadsheet or the logs.

**Why this priority**: Today every alert carries only a derived, machine-shaped
card/line id (e.g. `ec20-A1B2C3`). When several SIMs feed one alert channel, the
operator cannot tell at a glance which physical line is affected, which slows
triage of exactly the incidents (SIM loss, missed calls, registration loss)
these alerts exist to surface.

**Independent Test**: Configure a phone number for a line, trigger an SMS forward
and a critical event on it, and confirm both notifications display that phone
number alongside the existing identifiers.

**Acceptance Scenarios**:

1. **Given** a line with an operator-configured phone number, **When** an SMS is
   forwarded from that line, **Then** the notification shows the line's phone
   number.
2. **Given** a line with an operator-configured phone number, **When** a critical
   event fires for that line, **Then** the notification shows the line's phone
   number.
3. **Given** a circuit-switched card with **no** configured number but a number
   readable from its SIM, **When** an alert fires for it, **Then** the
   notification shows the SIM-read number.
4. **Given** a card/line whose number is neither configured nor readable, **When**
   an alert fires, **Then** the notification still delivers and shows the phone
   field with the literal value `unknown` (never a fabricated or empty-looking
   number).

---

### User Story 2 - Attribute an alert to a specific host/deployment (Priority: P1)

An operator running more than one bridge deployment, all reporting into the same
alert channel, can tell **which host/deployment** an alert came from by reading an
instance name on every notification.

**Why this priority**: With multiple bridges feeding one channel, an alert with no
host identity is ambiguous — the operator cannot tell which machine to act on.
An instance name makes every alert self-attributing.

**Independent Test**: Set an instance name in configuration, trigger any alert,
and confirm the notification displays that instance name. Unset it and confirm the
notification falls back to the host's system hostname.

**Acceptance Scenarios**:

1. **Given** an operator-configured instance name, **When** any alert (SMS or
   critical) fires, **Then** the notification displays that instance name.
2. **Given** no configured instance name, **When** any alert fires, **Then** the
   notification displays the host's system hostname.
3. **Given** two bridge deployments with distinct instance names feeding one
   channel, **When** each emits an alert, **Then** each notification shows its own
   instance name.

---

### User Story 3 - Configure identity per deployment (Priority: P2)

An operator can set, per line, the phone number to display, and set one instance
name for the deployment, using the existing configuration file — reusing the
number field that already exists for the IMS-over-LTE path and extending the same
concept to the other line types.

**Why this priority**: The reliable source of a card's number and a meaningful host
label is operator knowledge; auto-detection alone is insufficient (SIM number
storage is frequently blank, and container hostnames are often opaque hashes). A
simple config surface makes the first two stories dependable rather than
best-effort.

**Independent Test**: Add a phone number to a line and an instance name to the
deployment in configuration, restart, and confirm both appear on subsequent
alerts.

**Acceptance Scenarios**:

1. **Given** the operator adds a phone number to a line's configuration, **When**
   the bridge restarts, **Then** that line's alerts show the configured number.
2. **Given** the operator sets an instance name in configuration, **When** the
   bridge restarts, **Then** alerts show the configured instance name.

---

### Edge Cases

- **Unprovisioned SIM, no configured number**: a circuit-switched card whose SIM
  has no stored number and no operator-set number produces alerts whose phone
  field shows the literal value `unknown`, never a value that could be mistaken
  for a real number.
- **Opaque container hostname**: when running in a container with a random
  hostname and no configured instance name, the fallback hostname may be a hash;
  the operator resolves this by setting an instance name.
- **Multiple lines, one channel**: each alert must carry its own line's number,
  not a shared or first-seen number.
- **Alerts detected centrally**: for conditions detected away from the line's own
  process (e.g. registration/tunnel/signaling loss observed by the aggregating
  daemon, which only holds the line id), the number must still be resolved from
  configuration and shown.
- **Delivery unaffected on failure to resolve identity**: inability to determine a
  phone number or hostname must never prevent the alert itself from being sent.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Every forwarded-SMS notification MUST include the receiving card's
  phone number when it can be determined.
- **FR-002**: Every critical-event notification MUST include the affected
  card/line's phone number when it can be determined.
- **FR-003**: The phone number MUST be taken from operator configuration for a
  line when the operator has configured one, reusing the existing per-line number
  configuration where it already exists and extending the same concept to line
  types that lack it today.
- **FR-004**: For a circuit-switched card without a configured number, the system
  MUST use the number read from the card's SIM when one is available.
- **FR-005**: Every alert MUST include the phone field; when no phone number can
  be determined for a card/line, the field MUST show the literal value `unknown`
  (never a fabricated or empty-looking number), and the alert MUST still be
  delivered.
- **FR-006**: Every notification — both forwarded-SMS and critical-event — MUST
  include an instance name identifying the host/deployment the alert originates
  from.
- **FR-007**: The instance name MUST be taken from operator configuration when
  set.
- **FR-008**: When no instance name is configured, the system MUST fall back to
  the host's system hostname.
- **FR-009**: Adding phone number and instance name MUST NOT change existing
  notification behavior — category enable/disable, webhook routing, failure/
  recovery transitions, de-duplication, and retry/delivery semantics remain
  unchanged — and MUST NOT block or delay call/SMS/command handling.
- **FR-010**: Configuration and examples MUST NOT contain real subscriber
  identifiers; only operator-supplied real values at deployment time and
  synthetic placeholders in shipped examples are permitted.

### Key Entities

- **Alert notification**: an operator-facing message, in one of two shapes —
  forwarded SMS, or critical event (failure/recovery). Gains two identity
  attributes: card phone number (optional) and instance name (always present).
- **Card / line identity**: the affected unit an alert concerns. Already has a
  derived id; now also associated with a phone number (configured, or SIM-read
  for circuit-switched) resolvable from the unit's id.
- **Instance / deployment identity**: a single human-meaningful name for the
  host/deployment, configured or defaulted to the system hostname, applied to
  every alert the deployment emits.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: 100% of alert notifications (SMS and critical) display an instance
  name.
- **SC-002**: For any line with a configured phone number, 100% of that line's
  alerts display that number.
- **SC-003**: An operator can attribute any single alert to a specific SIM/line
  and a specific host using only the notification's contents — no logs, dashboard,
  or id lookup required — for every line that has a resolvable number.
- **SC-004**: No regression in existing alerting behavior: all previously passing
  alert delivery, categorization, and de-duplication checks continue to pass.
- **SC-005**: When multiple deployments and multiple lines share one channel, each
  alert is unambiguously attributable to exactly one line and one host.

## Assumptions

- Operator-configured phone numbers and instance names are trusted and displayed
  as provided (no validation beyond non-empty).
- The instance name is scoped to alert notifications for this feature; it is not
  introduced as a general node label for metrics or other subsystems.
- The existing per-line number configuration used by the IMS-over-LTE path is the
  model to reuse and extend; circuit-switched cards have no per-card
  configuration table and therefore rely on the SIM-read number as their source.
- Auto-detection of a card's number is best-effort only: SIM number storage is
  frequently blank, so operator configuration is the reliable path and the
  default expectation for a meaningful display.
- Both the phone field and the instance name are always present on every alert:
  an undeterminable number renders as the literal `unknown`, and an unset
  instance name falls back to the host's system hostname. Neither can block
  delivery.
