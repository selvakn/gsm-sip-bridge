# Feature Specification: SMS to Discord Forwarding

**Feature Branch**: `006-sms-discord-forward`
**Created**: 2026-05-03
**Status**: Draft
**Input**: User description: "feature to forward the incoming SMS on the GSM side to a discord webhook (push notification)."

## Clarifications

### Session 2026-05-03

- Q: Should SMS be deleted from SIM after forwarding, and how should messages be persisted? → A: Delete SMS from SIM after forwarding regardless of Discord delivery outcome. Persist all received SMS to a local SQLite database before deletion.

## User Scenarios & Testing

### User Story 1 - Receive SMS as Discord Notification (Priority: P1)

When an SMS arrives on any connected EC20 GSM module, the system reads the message, persists it to a local database, forwards it to a configured Discord webhook as a push notification, and deletes it from the SIM. The operator sees the SMS content, sender number, and receiving module identifier in a Discord channel without needing physical access to the GSM hardware.

**Why this priority**: This is the core feature. Without it, there is no value delivered. Operators currently have no visibility into incoming SMS messages on the GSM modules.

**Independent Test**: Send an SMS to a SIM card in a connected EC20 module. Verify the message appears in the configured Discord channel within seconds, containing the sender number and message body. Verify the message is stored in the local database and deleted from the SIM.

**Acceptance Scenarios**:

1. **Given** the bridge is running with a valid Discord webhook URL configured, **When** an SMS arrives on any connected module, **Then** a Discord message is posted containing the sender number, message body, timestamp, and module identifier.
2. **Given** the bridge is running with multiple modules active, **When** SMS messages arrive on different modules simultaneously, **Then** each message is forwarded independently to Discord with the correct module identifier.
3. **Given** the bridge is running, **When** an SMS arrives while a GSM call is actively being bridged on the same module, **Then** the SMS is still forwarded to Discord without disrupting the ongoing call.
4. **Given** the bridge is running, **When** an SMS is received, **Then** it is persisted to the local SQLite database before being deleted from the SIM, regardless of whether Discord forwarding succeeds.

---

### User Story 2 - Graceful Failure Handling (Priority: P2)

When the Discord webhook is unreachable or returns an error, the system logs the failure and continues operating normally. The SMS is still persisted to the local database and deleted from the SIM. SMS forwarding failures do not affect call bridging functionality.

**Why this priority**: Reliability is critical. The primary function (call bridging) must never be disrupted by a secondary feature (SMS forwarding). Network transient failures to Discord are expected. The local database ensures no message is lost even when Discord is unavailable.

**Independent Test**: Configure an invalid webhook URL, send an SMS, and verify the system logs a warning, stores the message in the database, deletes it from the SIM, and continues bridging calls normally.

**Acceptance Scenarios**:

1. **Given** the Discord webhook URL is unreachable, **When** an SMS arrives, **Then** the system logs a warning with the failure reason, persists the SMS to the database, and deletes it from the SIM.
2. **Given** the Discord webhook returns an HTTP error (4xx/5xx), **When** an SMS arrives, **Then** the system logs the error response, persists the SMS to the database, and continues normal operation.
3. **Given** the webhook was temporarily unavailable and later recovers, **When** new SMS messages arrive, **Then** they are forwarded successfully without requiring a restart.

---

### User Story 3 - SMS Notification Formatting (Priority: P3)

Discord notifications are formatted clearly so operators can quickly identify the source, time, and content of each SMS at a glance in a busy channel.

**Why this priority**: Readability improves operator efficiency but the feature works without rich formatting.

**Independent Test**: Send multiple SMS messages from different numbers and verify Discord messages are visually distinct, scannable, and contain all relevant metadata.

**Acceptance Scenarios**:

1. **Given** an SMS arrives with a known sender number, **When** it is forwarded to Discord, **Then** the notification includes the sender number prominently, the message body, the receiving module ID, and a timestamp.
2. **Given** an SMS arrives with a long message body (multi-part SMS), **When** it is forwarded, **Then** the full concatenated message body is included in a single Discord notification.

---

### Edge Cases

- What happens when an SMS arrives with an empty body (flash SMS or status report)?
  The system forwards a notification indicating an empty/status SMS was received.
- What happens when the message contains non-ASCII characters (Unicode/emoji)?
  The system preserves the original encoding and forwards the content as-is.
- What happens when the Discord webhook URL is not configured?
  SMS forwarding to Discord is disabled; SMS messages are still persisted to the local database and deleted from the SIM.
- What happens when the GSM module receives a multi-part SMS?
  The system waits for all parts and forwards the concatenated message as a single notification.
- What happens when the system receives a delivery status report (not a user SMS)?
  Delivery reports are ignored and not forwarded.
- What happens when the local database is inaccessible (disk full, permissions)?
  The system logs an error. The SMS is still deleted from the SIM to prevent storage exhaustion. The message content is preserved in the log output.

## Requirements

### Functional Requirements

- **FR-001**: System MUST monitor all connected EC20 modules for incoming SMS using AT commands (`+CMT` or `+CMTI` URCs).
- **FR-002**: System MUST forward each received SMS to a Discord webhook URL configured in the `[sms]` section of `config.ini`.
- **FR-003**: Each Discord notification MUST contain: sender phone number, message body, receiving module ID, and timestamp.
- **FR-004**: SMS forwarding MUST operate independently of call bridging; failures in SMS forwarding MUST NOT affect call handling.
- **FR-005**: System MUST log all SMS forwarding attempts and outcomes (success/failure) with relevant details.
- **FR-006**: When the Discord webhook URL is not configured, SMS forwarding to Discord MUST be disabled; local database persistence MUST still operate.
- **FR-007**: System MUST handle multi-part (concatenated) SMS and forward them as a single notification.
- **FR-008**: SMS forwarding events MUST be reflected in Prometheus metrics (SMS received count, forwarding success/failure count, database write count).
- **FR-009**: System MUST use Discord webhook embed formatting for clear, structured notifications.
- **FR-010**: System MUST not block the AT command polling loop while waiting for the Discord HTTP response; forwarding MUST be asynchronous.
- **FR-011**: System MUST persist every received SMS to a local SQLite database before deleting it from the SIM.
- **FR-012**: System MUST delete SMS from the SIM after receiving it, regardless of Discord forwarding outcome.
- **FR-013**: The SQLite database MUST record: sender number, message body, timestamp, receiving module ID, and Discord forwarding status (success/failure/skipped).

### Key Entities

- **SMS Message**: Sender number, message body, timestamp, receiving module ID, part count (for multi-part).
- **SMS Record (database)**: Sender number, message body, received timestamp, module ID, Discord forwarding status, forwarding timestamp.
- **Discord Webhook**: URL endpoint, embed payload structure, HTTP POST delivery.
- **SMS Configuration**: Webhook URL (optional), database path, enable/disable state.

## Success Criteria

### Measurable Outcomes

- **SC-001**: Incoming SMS messages appear in the Discord channel within 5 seconds of reception on the GSM module.
- **SC-002**: SMS forwarding has zero impact on call bridging latency and reliability (no measurable degradation).
- **SC-003**: 99% of SMS forwarding attempts succeed when the Discord webhook is reachable.
- **SC-004**: All SMS events (received, forwarded, failed) are visible in the Grafana monitoring dashboard.
- **SC-005**: 100% of received SMS messages are persisted in the local database, including those that fail Discord forwarding.

## Assumptions

- The EC20 modules support SMS reception in text mode (`AT+CMGF=1`) or PDU mode, and the `+CMT` or `+CMTI` URC is available on the AT command serial port already in use by the bridge.
- Discord webhook API remains stable and accepts embed-formatted POST requests.
- SMS reception is infrequent relative to call traffic; no queuing or batching is needed.
- Outbound SMS sending is out of scope for this feature.
- The Discord webhook URL is treated as a secret and configured via `config.ini` (same as SIP credentials).
- The SQLite database file path defaults to a sensible location (e.g., `/var/lib/gsm-sip-bridge/sms.db`) and is configurable.
- Database retention/cleanup is out of scope for this feature; the database grows unbounded.
