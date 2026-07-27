# Feature Specification: Discord Alerts for Critical Events

**Feature Branch**: `022-discord-critical-alerts`
**Created**: 2026-07-26
**Status**: Draft
**Input**: User description: "discord alert for critical events (configurable by the config.toml), including sms incoming (existing), errors with AT commands for critical lifecycle events, call missed by the PBX, ims sip disconnection, etc (suggest options)"

## Clarifications

### Session 2026-07-26

- Q1: Which critical event categories are in scope beyond SMS-incoming? → A: All four proposed categories — (a) module/modem lifecycle failures (SIM absent/unreadable, discovery failure, unresponsive AT worker), (b) IMS/SIP registration loss on a VoLTE or VoWiFi line, (c) calls missed by the PBX, and (d) VoWiFi ePDG/IPsec tunnel failure.
- Q2: How should alert configuration be structured in config.toml? → A: One shared default webhook URL for all alert categories, with the option to override the webhook URL for an individual category. Each category also has its own enable/disable flag.
- Q3: How should the system avoid flooding Discord for repeating/flapping conditions? → A: Transition-based alerting — alert only on a healthy→unhealthy transition, and send a separate, short "recovered" notice on the unhealthy→healthy transition. While a condition remains continuously unhealthy, no repeated alerts are sent.
- Q4: Does the "missed call" alert cover only calls that were never bridged (the existing `CallStatus::Missed` outcome), or does it also cover calls that bridged but had broken/one-way audio (the existing `CallStatus::Failed` outcome)? → A: Only the existing `CallStatus::Missed` outcome (never bridged: no answer, declined, or cancelled). A call that connected but had broken audio is a distinct, already-tracked outcome and is out of scope for this alert.
- Q5: Should the four new alert categories (module lifecycle, IMS/SIP registration loss, VoWiFi tunnel failure, missed calls) default to enabled or disabled when this feature first ships? → A: Disabled by default. An operator must explicitly enable each new category in `config.toml`; only the existing SMS-incoming alert remains enabled by default.
- Q6: Is the "AT command worker unresponsive" threshold duration-based or consecutive-failure-count-based, and what is the default? → A: Duration-based, default 60 seconds with no successful AT command on that module's worker.

### Session 2026-07-27

- Q7: Given the `supervise` module (rebased in from main) already runs its own automatic recovery loops — SIM auto-reset up to a bounded retry count, and per-line tunnel establish/restart cycles — should a module-lifecycle or VoWiFi-tunnel-failure alert fire on the first detected problem, or only once that built-in recovery is exhausted or a timeout is reached? → A: Only once built-in recovery is exhausted or a fixed timeout is reached; a single self-healed blip (e.g. one SIM reset that succeeds, one tunnel restart that re-establishes) MUST NOT raise an alert.
- Q8: The SIM recovery loop has a concrete "exhausted" signal (`GiveUpForThisIncident` after its bounded reset count), but the VoWiFi tunnel establish loop does not — strongswan retries forever by design, while swu bounds at ~180s. What duration should count as "tunnel failure" for alerting? → A: The tunnel has been continuously non-`Up` for 5 minutes (covers swu's ~180s bounded establish window plus one steady-state restart cycle before alerting).
- Q9: The `vowifi-ims-agent` also auto-restarts on crash/registration failure with an unbounded 5-second retry loop (`daemon_supervisor::RESTART_DELAY`), the same "no natural give-up" shape as the tunnel loop. What duration should count as "registration loss" for alerting? → A: Same 5-minute threshold as VoWiFi tunnel failure — continuously unregistered for 5 minutes before alerting.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Module/Modem Lifecycle Failure Alerts (Priority: P1)

When a GSM/LTE module hits a critical failure in its lifecycle — its SIM is missing or unreadable, it fails discovery/initialization, or its AT command worker stops responding — the operator receives a Discord notification identifying the affected module and the nature of the failure, without needing to watch logs or a dashboard.

**Why this priority**: These failures silently take a line out of service. Today they are only visible in logs or Grafana, which the operator does not watch continuously. This is the highest-value category because it directly causes lost inbound/outbound call capacity.

**Independent Test**: Remove or corrupt a SIM in a running module (or simulate a discovery failure), and verify a Discord notification appears identifying the module and describing the failure within the configured alerting window.

**Acceptance Scenarios**:

1. **Given** the bridge is running with alerting enabled, **When** a module's SIM becomes absent or unreadable and the module's own automatic SIM recovery gives up without success, **Then** a Discord notification is posted naming the module and the SIM condition.
2. **Given** a module's SIM becomes absent or unreadable, **When** the automatic SIM recovery power-cycle succeeds within its retry budget, **Then** no alert is sent for that incident.
3. **Given** the bridge is running, **When** a module fails initialization/discovery at startup, **Then** a Discord notification is posted describing the failure and the module never becomes available for calls.
4. **Given** a module's AT command worker stops responding (no successful AT command for 60 seconds, configurable), **When** that threshold is exceeded, **Then** a Discord notification is posted identifying the stalled module.

---

### User Story 2 - IMS/SIP Registration Loss Alerts (Priority: P1)

When a VoLTE or VoWiFi line unexpectedly loses its SIP registration with the PBX (as opposed to a deliberate shutdown), the operator receives a Discord notification identifying the affected line and the reason for the loss, so they know that line cannot currently receive or place calls.

**Why this priority**: An unregistered line is invisible to the PBX — no calls can be routed to or from it — and this can persist for extended periods before anyone notices, as previously observed with registration instability.

**Independent Test**: Force a line's SIP registration to fail (e.g., block the PBX or expire the registration) and verify a Discord notification appears naming the line and the disconnect reason, while a deliberate/clean shutdown of the bridge does not trigger the same alert.

**Acceptance Scenarios**:

1. **Given** a line is registered with the PBX, **When** the registration is lost unexpectedly and remains lost for 5 continuous minutes (surviving the agent's own automatic crash/restart retries), **Then** a Discord notification is posted naming the line and the reason for the loss.
2. **Given** a line's registration drops and the agent's own automatic restart re-registers it within 5 minutes, **When** this occurs, **Then** no alert is sent for that incident.
3. **Given** the bridge is shutting down cleanly and deliberately unregisters its lines, **When** this occurs, **Then** no critical alert is sent for that expected unregistration.
4. **Given** a line's registration was lost for over 5 minutes and a failure alert was sent, **When** the line successfully re-registers, **Then** a short "recovered" notification is posted naming the line, and no further failure alerts are sent while it stays registered.

---

### User Story 3 - VoWiFi Tunnel Failure Alerts (Priority: P2)

When a VoWiFi line's IPsec tunnel to the carrier's ePDG fails to establish or drops unexpectedly, the operator receives a Discord notification identifying the affected line, distinct from a plain SIP registration loss, since the underlying cause (network/tunnel layer vs. SIP layer) determines how the operator responds.

**Why this priority**: Tunnel failures are a known source of VoWiFi instability; surfacing them separately from SIP registration loss helps the operator triage faster, but a working SIP registration loss alert already covers the case where the tunnel failure also takes the line unregistered.

**Independent Test**: Block the ePDG endpoint or force the IPsec tunnel down for a VoWiFi line and verify a Discord notification appears identifying the line and the tunnel condition, separate from any SIP registration alert.

**Acceptance Scenarios**:

1. **Given** a VoWiFi line's tunnel is established, **When** the tunnel unexpectedly fails or drops and remains non-established for 5 continuous minutes (surviving the supervisor's own automatic restart/reinitiate attempts), **Then** a Discord notification is posted naming the line and the tunnel failure.
2. **Given** a VoWiFi line's tunnel drops, **When** the supervisor's own automatic restart re-establishes it within 5 minutes, **Then** no alert is sent for that incident.
3. **Given** a tunnel failure alert was sent, **When** the tunnel is successfully re-established, **Then** a short "recovered" notification is posted naming the line.
4. **Given** the bridge is shutting down cleanly and deliberately tears down the tunnel, **When** this occurs, **Then** no critical alert is sent for that expected teardown.

---

### User Story 4 - Missed Call Alerts (Priority: P2)

When an inbound call reaches the PBX but is never answered (rings out unanswered, is declined, or is cancelled before being bridged — the system's existing "missed" call outcome), the operator receives a Discord notification with the caller's number, the receiving line, and the time of the call, so missed business calls can be followed up. A call that did connect but suffered broken or one-way audio is a separate, already-tracked outcome and does not raise this alert.

**Why this priority**: Missed calls have direct business impact (a customer or contact could not be reached), but are lower urgency than an entire line being down, since other lines may still be functioning.

**Independent Test**: Place a call to a configured line and let it ring without answering. Verify a Discord notification appears with the caller number, the line/module that received it, and a timestamp.

**Acceptance Scenarios**:

1. **Given** alerting is enabled, **When** an inbound call is classified as missed (never bridged: no answer, declined, or cancelled), **Then** a Discord notification is posted with the caller number, receiving line, and timestamp.
2. **Given** an inbound call is answered normally, **When** the call completes, **Then** no missed-call alert is sent.
3. **Given** an inbound call bridges but suffers broken or one-way audio, **When** the call ends, **Then** no missed-call alert is sent, since this is a distinct, already-tracked outcome.
4. **Given** multiple calls are missed in quick succession across different lines, **When** each occurs, **Then** each is reported with its own correct line and caller identification.

---

### User Story 5 - Per-Category Alert Configuration (Priority: P3)

An operator configures which critical event categories raise Discord alerts, and where those alerts are sent, entirely through `config.toml`, without needing to rebuild or patch the bridge. A single default webhook covers all categories, and any category can be pointed at a different webhook (e.g. a dedicated channel for module failures) by overriding it individually.

**Why this priority**: Different deployments care about different failure modes and may want to triage certain categories in a separate channel; making this configurable avoids alert fatigue and keeps the feature useful across environments, but the alerting itself (P1/P2 stories) delivers value even with just the shared default webhook.

**Independent Test**: Disable one alert category in `config.toml`, trigger that condition, and verify no Discord notification is sent for it while other enabled categories still alert normally. Separately, override one category's webhook URL and verify only that category's alerts go to the overridden destination while the rest continue using the default.

**Acceptance Scenarios**:

1. **Given** an alert category is disabled in `config.toml`, **When** its triggering condition occurs, **Then** no Discord notification is sent for that category, and this is logged locally instead.
2. **Given** a fresh upgrade with no changes to `config.toml`, **When** the bridge starts, **Then** SMS-incoming alerting remains enabled as before, and the four new categories (module lifecycle, IMS/SIP registration loss, VoWiFi tunnel failure, missed calls) are disabled until the operator opts in.
3. **Given** no default alert webhook is configured and no category overrides one, **When** any critical event occurs, **Then** the event is still logged and reflected in metrics, but no Discord call is attempted.
4. **Given** a category has its own webhook override configured, **When** that category's event occurs, **Then** the alert is sent to the overridden webhook, not the shared default.
5. **Given** the configuration is changed, **When** the bridge is restarted, **Then** the new alert configuration takes effect without requiring a rebuild.

---

### Edge Cases

- What happens when the Discord webhook for alerts is unreachable or returns an error? The system logs the failure, continues operating normally, and does not retry indefinitely or block any call-handling or AT-command path.
- What happens when many modules fail at once (e.g., at bridge startup before modules are discovered, or during a mass power-cycle)? Each affected module still only sends one alert per healthy→unhealthy transition, so a simultaneous multi-module failure produces one Discord message per module, not a repeating stream per module.
- What happens when a critical event fires for a module/line that has since been removed from configuration? The alert is still sent if the event was in flight before removal; no alert is sent after the line/module no longer exists in configuration.
- What happens when the same underlying condition would otherwise trigger two different categories at once (e.g., a module's SIM failure also causes its IMS line to deregister)? Each category alerts independently and clearly identifies its own condition; the operator can correlate them by module/line identifier and timestamp.
- What happens during a planned maintenance restart initiated by the operator (e.g., scheduled card restart)? Expected, operator-initiated restarts must not raise the same alert as an unexpected failure.
- How does the system behave if the alerting subsystem itself errors (e.g., malformed config)? The bridge must still start and handle calls/SMS normally; alerting configuration errors are logged, and default to alerting disabled for the affected category rather than crashing the bridge.
- What happens when a module or line's own built-in automatic recovery (SIM power-cycle, tunnel re-establish, IMS agent restart) resolves the problem before its category's recovery-exhaustion/timeout threshold is reached? No alert is sent; a self-healed blip is routine operation, not a critical event.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: System MUST continue to forward incoming SMS to Discord as it does today, and MUST bring that existing behavior under the same configurable alerting mechanism defined by this feature.
- **FR-002**: System MUST detect and alert on the following critical event categories: module/modem lifecycle failure (SIM absent/unreadable, discovery/initialization failure, unresponsive AT command worker), IMS/SIP registration loss on a VoLTE or VoWiFi line, VoWiFi ePDG/IPsec tunnel failure, and calls missed by the PBX (calls that were never bridged — no answer, declined, or cancelled — excluding calls that bridged but had broken/one-way audio, which are a separate, already-tracked outcome).
- **FR-003**: An AT command worker MUST be considered unresponsive, and MUST raise a module lifecycle failure alert, after 60 continuous seconds with no successful AT command on that module; this duration MUST be configurable via `config.toml`.
- **FR-004**: A SIM absent/unreadable condition MUST NOT raise a module lifecycle failure alert while the module's own automatic SIM recovery (power-cycle retries) is still in progress; the alert MUST fire only once that recovery is exhausted for the incident (i.e. it gives up without a successful recovery).
- **FR-005**: A VoWiFi line's tunnel MUST NOT raise a tunnel-failure alert for a single establish/restart cycle; the alert MUST fire only once the tunnel has remained continuously non-established for 5 minutes, regardless of how many automatic restart attempts occurred in that window. This duration MUST be configurable via `config.toml`.
- **FR-006**: A VoLTE or VoWiFi line MUST NOT raise an IMS/SIP registration-loss alert for a single agent crash/restart cycle; the alert MUST fire only once the line has remained continuously unregistered for 5 minutes, regardless of how many automatic restart attempts occurred in that window. This duration MUST be configurable via `config.toml`.
- **FR-007**: System MUST allow an operator to enable or disable Discord alerting independently per event category via `config.toml`. The existing SMS-incoming category MUST default to enabled (unchanged from today); the four new categories (module lifecycle failure, IMS/SIP registration loss, VoWiFi tunnel failure, missed calls) MUST default to disabled and require explicit opt-in.
- **FR-008**: System MUST support one shared default Discord webhook URL that applies to all alert categories, and MUST allow an operator to override the webhook URL for any individual category via `config.toml`, in which case that category's alerts go to its overridden webhook instead of the default.
- **FR-009**: Each Discord alert MUST identify: the event category, the affected module/line identifier (when applicable), a human-readable description of the condition, and a timestamp.
- **FR-010**: System MUST distinguish deliberate/expected state changes (clean shutdown, operator-initiated restart, configuration-driven removal) from unexpected failures, and MUST NOT raise a critical alert for the former.
- **FR-011**: System MUST NOT block or delay call handling, SMS handling, or AT command processing while sending or retrying a Discord alert.
- **FR-012**: System MUST log every critical event and its alert outcome (sent/suppressed/skipped/failed) regardless of whether Discord delivery succeeds.
- **FR-013**: For module lifecycle, IMS/SIP registration, and VoWiFi tunnel categories, the system MUST alert only on the healthy→unhealthy transition (as refined by FR-004/FR-005/FR-006's recovery-exhaustion rules) and MUST send a distinct "recovered" notification on the unhealthy→healthy transition; it MUST NOT re-send the failure alert while the condition remains continuously unhealthy.
- **FR-014**: When no webhook applies to a category (no default configured and no override set), or alerting is disabled for that category, the system MUST skip the Discord call while still logging the event and updating relevant metrics.
- **FR-015**: Critical alert delivery attempts and outcomes MUST be reflected in the existing Prometheus metrics alongside the current SMS-forwarding metrics.
- **FR-016**: System MUST treat all alert webhook URLs (default and per-category overrides) as secrets, consistent with existing handling of the SMS Discord webhook URL.

### Key Entities

- **Critical Event**: A category (module lifecycle failure, SIP registration loss, VoWiFi tunnel failure, or missed call), the affected module/line identifier, a description/reason, a timestamp, and whether it represents a new failure or a recovery from one.
- **Alert Configuration**: A default webhook URL, plus per-category enabled/disabled state and an optional webhook URL override.
- **Alert Delivery Record**: The category, resolved target webhook (default or override), delivery outcome (sent/suppressed/skipped/failed), and time sent — extending the existing forwarding-status tracking used for SMS.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An operator learns of a critical module or line failure via Discord within 30 seconds of it being detected internally, without checking logs or dashboards.
- **SC-002**: Critical-event alerting has zero measurable impact on call setup time, call audio quality, or SMS delivery latency.
- **SC-003**: An operator can change which event categories alert, and where alerts are sent, using only configuration changes — zero code changes required.
- **SC-004**: While a failure condition remains continuously unhealthy, the operator receives exactly one failure alert and, upon recovery, exactly one recovery notice — never a repeating stream of messages for the same ongoing condition.
- **SC-005**: 100% of critical events are captured in local logs/metrics even when Discord delivery fails or is disabled, so no failure is silently lost.

## Assumptions

- "Critical events" in this feature are operational/reliability failures (module down, line unregistered, call missed) — not routine informational events (e.g., a normal call start/end, a successful registration renewal).
- The existing SMS-to-Discord forwarding mechanism (embed formatting, retry/backoff, non-blocking async delivery) is the reference implementation this feature extends; new alert categories reuse the same delivery approach rather than inventing a new one.
- Alert configuration lives in `config.toml` alongside existing sections (e.g. `[sms]`), consistent with how the bridge is already configured.
- Recipients read Discord on a phone or desktop with push notifications enabled; no additional notification channel (email, SMS-out, PagerDuty) is in scope for this feature.
- Historical alert data (a searchable log of past alerts) is not required beyond what is already logged/persisted for SMS; this feature does not require a new UI.
- Existing scheduled/operator-initiated restarts (e.g. the scheduled card restart feature) are treated as expected and must be excluded from failure-alert triggers.
