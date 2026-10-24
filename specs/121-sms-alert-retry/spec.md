# Feature Specification: SMS Alert Delivery Status and Retry

**Feature Branch**: `121-sms-alert-retry`
**Created**: 2026-10-24
**Status**: Draft
**Input**: User description: "Capture the delivery status of SMS alerts (SMS forwarded as notifications to Discord via webhook). Record whether each alert was delivered or failed (e.g. Discord unreachable, non-2xx response, timeout). Failed alerts must be retried: retry opportunistically when the next alert is sent, or on a long interval (about one hour), whichever comes first. Persist the status so pending/failed alerts survive restarts, avoid duplicate delivery, and expose status for observability."

## Clarifications

### Session 2026-10-24

- Q: Should retry cover only SMS-forward notifications, or also the operational alerts (registration loss, tunnel failure, missed call, etc.)? → A: SMS-forward notifications only; operational alerts are unchanged and out of scope.
- Q: How are retried texts presented in Discord? → A: One message per text, oldest first, with the original received time and a "late delivery" marker (no digest).
- Q: When is a persistently failing text abandoned? → A: By age only: 7 days since the text was received (configurable); no attempt-count cap.
- Q: Are texts already marked failed before this feature retried after upgrade? → A: No; they are left untouched. Only failures recorded after the feature is enabled are retried.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A text that could not be posted to Discord is eventually delivered (Priority: P1)

The operator receives incoming texts on the bridge's SIM lines and relies on the Discord channel as the place they see them. Today, if Discord is unreachable at the moment a text arrives (network outage, DNS failure, timeout, Discord error response), the text is recorded as failed and the operator never hears about it in Discord — it is only visible by inspecting the stored history. This story makes a failed notification get sent later, without the operator doing anything.

**Why this priority**: A text that silently never reaches the operator (verification codes, bank messages) is the core harm. Everything else in this feature supports this outcome.

**Independent Test**: Make the Discord destination unreachable, receive a text, confirm it is recorded as failed. Restore reachability, wait for the retry trigger (next alert or the long interval), and confirm the text then appears in Discord and its status becomes delivered.

**Acceptance Scenarios**:

1. **Given** a text whose Discord notification failed, **When** a later text arrives and its notification is sent, **Then** the earlier failed notification is retried in the same pass, and delivered if Discord is reachable.
2. **Given** a failed notification and no further texts arriving, **When** about an hour has passed since its last attempt, **Then** it is retried automatically.
3. **Given** a failed notification, **When** a retry succeeds, **Then** its status becomes delivered and it is never sent again.
4. **Given** a failed notification, **When** a retry also fails, **Then** it stays in the failed state with its latest failure reason and is retried on the next trigger.

---

### User Story 2 - The operator can see what happened to every alert (Priority: P2)

For each text, the operator can tell whether its Discord notification is pending, delivered, or failed; how many attempts were made; when the last attempt was; and why the last attempt failed (unreachable, timeout, rejected by Discord with a given response code, webhook not configured).

**Why this priority**: Without visibility the operator cannot tell "no texts arrived" from "texts arrived but Discord never got them". It builds on Story 1 but is useful on its own for diagnosis.

**Independent Test**: Cause one delivered, one failed (unreachable), and one rejected notification. Confirm each shows the correct status, attempt count, last-attempt time and failure reason in the stored history and in the bridge's operational status/metrics.

**Acceptance Scenarios**:

1. **Given** notifications in a mix of states, **When** the operator inspects the bridge's status output, **Then** the counts of pending, delivered and failed notifications and the age of the oldest undelivered one are shown.
2. **Given** a failed notification, **When** the operator inspects it, **Then** the failure reason, attempt count and last-attempt time are available.
3. **Given** undelivered notifications exist, **When** the operator scrapes the bridge's metrics, **Then** the undelivered count and oldest-undelivered age are exposed so an external alert can be built on them.

---

### User Story 3 - Status and retries survive restarts and never duplicate (Priority: P2)

If the bridge restarts (crash, upgrade, host reboot) while notifications are pending or failed, they are still retried afterwards. A notification that already reached Discord is never posted again, including when a restart or overlapping retry happens near the moment of delivery.

**Why this priority**: Restarts are routine on this deployment (container recreation, hourly churn, nightly uplink outages). Retry state that evaporates on restart would defeat Story 1; duplicates would erode trust in the feed.

**Independent Test**: With failed notifications on record, restart the bridge with Discord reachable; confirm each is delivered exactly once. Repeat with a restart between a send and the recording of its result.

**Acceptance Scenarios**:

1. **Given** failed or pending notifications on record, **When** the bridge restarts, **Then** they are picked up for retry without any texts needing to be re-received.
2. **Given** a delivered notification, **When** any later retry pass or restart occurs, **Then** it is not sent again.
3. **Given** two retry triggers firing close together (next-alert and hourly timer), **When** both run, **Then** each undelivered notification is attempted at most once between them.

---

### User Story 4 - A long outage does not flood Discord or the bridge (Priority: P3)

After a long Discord outage there may be many undelivered notifications. Retrying them must not hammer Discord, trip its rate limits, or delay delivery of newly arrived texts.

**Why this priority**: A robustness concern that only matters during prolonged outages; the earlier stories already deliver the main value.

**Independent Test**: Accumulate a large backlog of failed notifications, restore Discord, and confirm the backlog drains at a bounded pace while a newly arrived text is delivered promptly.

**Acceptance Scenarios**:

1. **Given** a backlog of failed notifications and Discord responding with a rate-limit signal, **When** a retry pass runs, **Then** it stops early, honors the wait Discord requested, and resumes on the next trigger.
2. **Given** a backlog, **When** a new text arrives, **Then** its own notification is attempted first and is not queued behind the backlog.
3. **Given** a backlog, **When** notifications are retried, **Then** they are delivered oldest first, and each is clearly identifiable as a late delivery (it shows the time the text was originally received).

---

### Edge Cases

- Discord webhook is not configured: notifications are recorded as skipped, not failed, and are never retried; if the webhook is configured later, previously skipped ones are not retroactively sent.
- Discord rejects a notification permanently (for example the webhook was deleted): the notification keeps failing; attempts must not continue unbounded, so it stops being retried once the text passes the age limit (default 7 days) and is marked abandoned, with the reason visible.
- A text is very old by the time Discord recovers (days): it is still delivered, labeled with its original received time, unless it has passed the abandonment limit.
- The bridge is killed after Discord accepted a notification but before the result was recorded: on restart the notification may be re-sent at most once (at-least-once delivery); this narrow window is an accepted limitation.
- The clock changes or jumps (for example after an uplink outage): the hourly interval must not cause a burst of retries or stall retries indefinitely.
- Multiple lines or modems receive texts: retries cover every line's notifications, not only the line that triggered the pass.
- The stored history from before this feature contains rows already marked failed: they are left untouched and never retried, so upgrading does not post a burst of stale texts. Only failures recorded after the feature is enabled are retried.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST record, for every received text, the state of its Discord notification: pending, delivered, failed, skipped (no webhook configured), or abandoned (retries exhausted).
- **FR-002**: For every failed attempt, the system MUST record the reason category (unreachable, timeout, rejected by Discord with the response code, rate limited) and the time of the attempt, and keep a running attempt count.
- **FR-003**: The system MUST retry undelivered (pending or failed) notifications whenever a new notification is about to be sent.
- **FR-004**: The system MUST also retry undelivered notifications on a fixed long interval (default one hour) when no new texts arrive.
- **FR-005**: The retry interval MUST be operator-configurable, and retrying MUST be disableable.
- **FR-006**: The system MUST NOT send a notification again once it is recorded as delivered.
- **FR-007**: Notification state MUST be stored durably so pending and failed notifications are retried after a restart without re-receiving the text.
- **FR-008**: The system MUST ensure that concurrent retry triggers do not attempt the same notification at the same time.
- **FR-009**: Retrying MUST NOT delay the first attempt for a newly received text.
- **FR-010**: The system MUST stop retrying a notification once the text is older than an operator-configurable age limit (default 7 days since received), mark it abandoned, and keep the last failure reason visible. There is no attempt-count cap.
- **FR-011**: When Discord signals rate limiting, the system MUST stop the current retry pass, honor the requested wait, and continue on the next trigger.
- **FR-012**: Each retried text MUST be posted as its own Discord message (no digest), oldest first, carrying the original received time and an explicit "late delivery" marker so the operator can tell it from a fresh one.
- **FR-013**: The system MUST expose, via its existing status and metrics surfaces, the number of pending, failed, delivered and abandoned notifications and the age of the oldest undelivered one.
- **FR-014**: The system MUST log each retry pass outcome (attempted, delivered, still failing) at a level that lets the operator diagnose a prolonged outage without log flooding.
- **FR-015**: Notifications recorded as skipped (webhook not configured) MUST NOT be retried.
- **FR-016**: Behavior of first-attempt delivery, message content for fresh texts, and all existing configuration MUST remain unchanged when no failures occur.

### Key Entities

- **SMS notification record**: The delivery state attached to each received text — state (pending, delivered, failed, skipped, abandoned), attempt count, time of last attempt, last failure reason, time delivered, and the text's original received time.
- **Retry pass**: One sweep over undelivered notifications, triggered either by a new notification being sent or by the long-interval timer; has an outcome summary (attempted, delivered, still failing, stopped early by rate limit).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: After a Discord outage of any length up to the abandonment limit, 100% of texts received during the outage appear in Discord within one retry trigger (next text or about one hour) of Discord becoming reachable.
- **SC-002**: Across a restart with undelivered notifications on record, 0 notifications are lost and no delivered notification is re-sent.
- **SC-003**: The operator can determine the delivery state, attempt count and last failure reason of any received text in under one minute from the bridge's existing inspection tools.
- **SC-004**: An external monitor can detect "notifications stuck undelivered for more than 2 hours" from exposed metrics alone, without reading logs.
- **SC-005**: With a 500-notification backlog, a newly received text is delivered to Discord within the same time as with no backlog, and the backlog never causes a Discord rate-limit lockout lasting longer than the interval Discord itself requests.
- **SC-006**: With no failures, observable behavior (what is posted, when, and what is stored) is identical to before this feature.

## Assumptions

- "SMS alerts" means the Discord notification the bridge posts for each received text. The separate operational alert categories (module lifecycle, registration loss, tunnel failure, missed call, etc.) are explicitly out of scope and unchanged (confirmed in clarification); extending the same mechanism to them could be a follow-up.
- The existing stored SMS history already records a forwarding status per text; this feature builds on it rather than introducing a separate store.
- Default retry interval is one hour, default abandonment limit is 7 days (operator-configurable); these are reasonable defaults, not requirements from the user.
- Delivery is at-least-once: a crash between Discord accepting a message and the result being recorded may cause one duplicate, which is acceptable.
- Only Discord is a notification destination; no other channel is retried.
- The existing short in-line retry within a single send attempt (for transient errors and rate-limit waits) stays as-is; this feature adds retry across attempts.
