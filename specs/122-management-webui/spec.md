# Feature Specification: Read-only management web UI

**Feature Branch**: `122-management-webui`
**Created**: 2026-10-24
**Status**: Draft
**Input**: User description: "Read-only management web UI served by the daemon. Pages: (1) Lines overview — per-line card with carrier, masked MSISDN, transport (cs/vowifi/volte), registration state + expiry countdown, VoWiFi tunnel/child SA/Gm/uplink-held, VoLTE PDN/registered, signal strength (CSQ dBm; LTE RSRP/RSRQ/SINR), active calls, registered-since, agent last-report age; global header with daemon uptime, build version, PBX SIP registration, active alerts. (2) Calls — today/7d counts answered/missed/failed, avg duration, filterable log by line/transport/status/date, live calls with RTP jitter/loss/RTT. (3) SMS — inbox from the DB (never modem; CMGL marks read), filter by line, search, forwarding status, OTP detect + copy. (4) Events & health — persisted timeline of alerts, recoveries, dead-peer losses, uplink holds, scheduled restarts; modules active/failed, store queue depth. (5) Config — read-only effective config with secrets redacted. No control actions (no AT terminal, USSD, eSIM, SMS send, restart, network lock). Must require authentication (SMS has OTPs). Signal polling through the existing AT worker only, also exported as a Prometheus gauge. No time-series graphs (link to Grafana instead). Negligible image size growth (<1 MB), no Node toolchain in the build. Inspired by github.com/mengmengcode/vocat."

## Background

Today, an operator who wants to know "is everything OK?" has four places to look:

- **`vowifi-status` / `volte-status`** over SSH, one line at a time.
- **Prometheus metrics** and the Grafana dashboard, which show health over time but need a separate stack and don't show individual calls or messages.
- **Discord**, which receives SMS and alerts as they happen but can't be searched or filtered by line.
- **The bridge's own database**, which already records every call and SMS (with transport and forwarding outcome) but has no viewer.

Several facts the operator cares about aren't visible anywhere today:

- **Signal strength** is read once during module discovery and never again.
- **Alert history**: alerts exist only as counters and as Discord posts, so "what happened overnight on line 2?" can't be answered from the bridge.

[vocat](https://github.com/mengmengcode/vocat) is a modem-management web UI for the same class of hardware. This feature borrows its *observation* side (device overview, radio info, SMS history, health, logs) and deliberately leaves out its *control* side (AT terminal, USSD, eSIM, network/cell lock, flight mode, restart, SMS send, proxy routing, notification setup). The bridge owns the modem ports and the IMS state. A second actor issuing commands would compete with it, and several of vocat's actions would break a running bridge.

## Clarifications

### Session 2026-10-24

- Q: Should the call log include outbound calls (PBX → carrier), which are currently only counted in metrics and never stored? → A: Yes. Outbound calls are stored with a direction (inbound/outbound), and the Calls page totals and filters cover both directions.
- Q: How is the UI exposed relative to the existing unauthenticated metrics endpoint? → A: On its own listener with a configurable port (default 8080), on all interfaces once enabled. The metrics endpoint's port and behaviour are unchanged.
- Q: How does the operator log in? → A: A login page that issues a session cookie (HttpOnly, SameSite=Strict) valid for 7 days, with a logout button. Sessions don't survive a bridge restart.
- Q: Does one UI cover several bridge deployments? → A: No. Each bridge serves its own UI for its own lines only. Cross-bridge views are out of scope; Grafana already combines metrics across bridges.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - See at a glance whether every line is healthy (Priority: P1)

The operator opens the web UI in a browser on the LAN and, on a single page, sees one card per line. Each card shows:

- carrier, transport and masked own number;
- whether the line is registered, and how long until the registration needs renewing;
- for VoWiFi lines: whether the tunnel, its child SA and the Gm connection are up, and whether the uplink is currently being held;
- for VoLTE lines: whether the PDN is up;
- signal strength, how long the line has been registered, how many calls it is carrying, and how fresh the data is.

A header shows how long the bridge has been running, which version is deployed, whether the PBX registration is up, and any alert that is currently active. The page keeps itself current without a manual reload.

**Why this priority**: This is the question the operator asks most often, and today it needs SSH plus one status command per line. On its own it replaces that routine.

**Independent Test**: Run the bridge with at least one VoWiFi line and one other line. Open the overview and confirm that every field matches `vowifi-status` / `volte-status` and the metrics endpoint. Then bring a tunnel down and confirm the card turns unhealthy within the refresh interval, without a reload.

**Acceptance Scenarios**:

1. **Given** two registered lines, **When** the operator opens the overview, **Then** both cards show "registered" with an expiry countdown that agrees with `vowifi-status` to within one refresh interval.
2. **Given** a VoWiFi line whose child SA goes down, **When** the next refresh happens, **Then** that card is visibly marked unhealthy and names the failing component; the other card is unaffected.
3. **Given** a line agent that has stopped reporting, **When** its last report is older than the existing staleness threshold, **Then** the card says its data is stale and shows the age instead of continuing to show old values as current.
4. **Given** a circuit-switched line, **When** the overview is shown, **Then** the card shows the signal strength in dBm with a bar indicator, and LTE quality values (RSRP/RSRQ/SINR) when the modem is on an LTE cell.
5. **Given** an alert is currently active (e.g. registration loss), **When** the overview is shown, **Then** the header lists it with its line and how long ago it started.

---

### User Story 2 - Read SMS without Discord or the modem (Priority: P1)

The operator opens the SMS page to find a message, typically a one-time code. The page lists the messages the bridge has received, newest first, with sender, line, time, body and whether forwarding succeeded. The operator can filter by line, search the text, and copy a detected one-time code with a single click.

**Why this priority**: Reading OTPs is a frequent, time-sensitive task. When Discord forwarding fails, the message is currently only reachable through the database.

**Independent Test**: Deliver an SMS containing a 6-digit code to a line and open the SMS page. Confirm the message appears with its forwarding status, the code is offered for copying, and the modem's stored-message read state is unchanged by viewing the page.

**Acceptance Scenarios**:

1. **Given** messages on two lines, **When** the operator filters by one line, **Then** only that line's messages are listed.
2. **Given** a message whose Discord forwarding failed, **When** it is listed, **Then** it is visibly marked as not forwarded.
3. **Given** a message body containing a 4–8 digit verification code, **When** it is listed, **Then** the code is highlighted with a copy control, and copying puts exactly the code on the clipboard.
4. **Given** the operator searches for a word, **When** results are shown, **Then** only messages whose sender or body contains it are listed.
5. **Given** the page is open, **When** the operator views or refreshes it, **Then** no command is sent to any modem. The page reads only what the bridge has already stored.

---

### User Story 3 - Review calls (Priority: P2)

The operator opens the Calls page and sees totals for today and the last 7 days, inbound and outbound: answered, missed and failed calls, and average duration. Below the totals is a call log that can be filtered by line, direction, transport, status and date range. Calls in progress are listed at the top with their live audio quality (jitter, packet loss, round-trip time).

**Why this priority**: Missed and failed calls are the main signal that something is wrong in a way health indicators don't show. Live audio quality turns a user complaint ("the call sounded bad") into something checkable.

**Independent Test**: Place one answered, one missed and one failed inbound call, plus one answered and one failed outbound call. Confirm each appears in the log with the right status, direction and transport and the totals change accordingly. During an active call, confirm it is listed with quality figures that agree with the metrics endpoint.

**Acceptance Scenarios**:

1. **Given** calls recorded across several days, **When** the operator filters to "missed" on one line for one day, **Then** only matching calls are listed and their count matches the filter summary.
2. **Given** a call in progress, **When** the Calls page refreshes, **Then** the call is listed as active with its line, caller and running duration. Once it ends, it moves into the log.
3. **Given** a SIP client places an outbound call that the carrier rejects, **When** the Calls page refreshes, **Then** the call appears as outbound and failed, with the dialled number.
4. **Given** a call with no audio-quality data available, **When** it is listed, **Then** quality fields read "unavailable" rather than zero.

---

### User Story 4 - Find out what happened while nobody was watching (Priority: P2)

The operator opens the Events page and sees a timeline, newest first, of what the bridge has noticed: alerts raised and resolved, tunnel recoveries, dead-peer losses, uplink holds and releases, scheduled restarts, module failures and discoveries. The timeline can be filtered by line and by kind. The same page shows current internal health: active and failed modules, and the depth of the pending-writes queue.

**Why this priority**: Overnight incidents (see the nightly uplink outage on the Pi) are currently reconstructed from Discord scrollback and container logs. A persisted timeline answers "when, which line, for how long" directly.

**Independent Test**: Cause a registration loss and its recovery on one line. Confirm both appear on the timeline with the right line and times, and remain there after the bridge restarts.

**Acceptance Scenarios**:

1. **Given** an alert raised and later resolved, **When** the timeline is shown, **Then** both entries appear, and the resolution shows how long the condition lasted.
2. **Given** the bridge restarts, **When** the timeline is shown, **Then** events from before the restart are still listed.
3. **Given** an alert whose Discord delivery is suppressed or disabled, **When** the condition occurs, **Then** it still appears on the timeline.
4. **Given** events older than the retention period, **When** retention runs, **Then** they are removed and newer events are unaffected.

---

### User Story 5 - Check the configuration the bridge is actually running with (Priority: P3)

The operator opens the Config page and sees the effective configuration the running bridge loaded, with every secret replaced by a redaction marker. It can be used to check things like a stale modem port or a toggled feature flag without opening the file on the device.

**Why this priority**: Useful when diagnosing, but it's needed rarely and SSH remains an option.

**Independent Test**: Load a configuration containing a SIP password, a Discord webhook and an `env:` reference. Confirm the page shows every other value unchanged and none of the secret values, including the resolved value of the `env:` reference.

**Acceptance Scenarios**:

1. **Given** a configuration with secrets, **When** the Config page is shown, **Then** no password, webhook URL, token or resolved environment-variable secret appears anywhere in the page or the data it loads.
2. **Given** a configuration value that differs from the file on disk (e.g. edited after start-up), **When** the page is shown, **Then** it shows what the running bridge loaded, and says so.

---

### User Story 6 - Keep the UI closed to everyone else (Priority: P1)

Only someone with the configured credential can see any page or data. Anyone else gets a login prompt and nothing more. The UI is off unless the operator turns it on.

**Why this priority**: The UI exposes one-time codes, caller numbers and network details. Exposing it without authentication would be a security defect, not a missing feature.

**Independent Test**: With the UI enabled, request every page and data endpoint without credentials, then with a wrong credential, and confirm none returns line, call, SMS, event or config data.

**Acceptance Scenarios**:

1. **Given** the UI is enabled with a credential, **When** an unauthenticated request asks for any page or data, **Then** it is refused and no line, call, SMS, event or config data is returned.
2. **Given** the UI section is absent from the configuration, **When** the bridge starts, **Then** no web UI is served, and the existing metrics endpoint behaves exactly as before.
3. **Given** the UI is enabled but no credential is configured, **When** the bridge starts, **Then** it refuses to serve the UI and logs why, instead of serving it unauthenticated.
4. **Given** a logged-in operator, **When** they log out or the bridge restarts, **Then** the next request is sent back to the login page.
5. **Given** repeated failed logins, **When** a threshold is crossed, **Then** further attempts from that client are slowed or refused for a period.

---

### Edge Cases

- **A modem is busy with a call, SMS or recovery when signal is due to be polled**: the poll waits its turn or is skipped for that interval. It never interrupts or delays call/SMS handling, and the card shows the age of the last reading.
- **A modem returns "not known" for signal (`99`) or an LTE query fails**: the value shows as "unknown", never as a misleading number.
- **VoWiFi lines, where the modem is used only for its SIM**: signal is shown de-emphasised and labelled as not affecting calls.
- **No lines discovered yet, or all lines failed**: the overview says so explicitly instead of showing an empty page.
- **Database unavailable or locked by a write**: the SMS, Calls and Events pages show a clear error, and the overview (which doesn't depend on the database) keeps working.
- **Very large histories (tens of thousands of calls or SMS)**: lists are paged and stay responsive.
- **SMS bodies containing markup or script**: shown as literal text, never interpreted by the browser.
- **Several browser tabs or users viewing at once**: each tab refreshes independently, and the load on the bridge doesn't grow per tab beyond serving the requests.
- **Hot-swapped or re-enumerated modem** (stale port): its card reflects the discovery state rather than freezing on its last values.
- **Clock skew or timezone differences** (e.g. a Pi running on local time): all times are shown in the browser's timezone, from consistently stored timestamps.

## Requirements *(mandatory)*

### Functional Requirements

**Access and enablement**

- **FR-001**: The web UI MUST be disabled unless explicitly enabled in the configuration. A configuration without the UI section MUST behave exactly as today.
- **FR-002**: When enabled, every page and every data request MUST require authentication with an operator credential set in the configuration. The credential MUST support the configuration's existing `env:` indirection, so the secret need not be in the file.
- **FR-002a**: The operator MUST log in on a login page. A successful login MUST start a session that lasts 7 days and is carried in a cookie that scripts can't read and that isn't sent on cross-site requests. A logout control MUST end the session immediately. Sessions MUST NOT survive a bridge restart. The password MUST be sent only to the login page, never with each refresh.
- **FR-003**: The bridge MUST refuse to serve the UI if it is enabled without a credential, and MUST log the reason.
- **FR-004**: The UI MUST be served on its own listener, separate from the metrics endpoint. Its port MUST be configurable (default 8080), and so MUST its listen address (default: all interfaces). Enabling, disabling or exposing the UI MUST NOT change the metrics endpoint's port, address or unauthenticated scrape behaviour. If the UI port can't be bound, the bridge MUST log the error and keep running everything else.
- **FR-005**: Repeated failed authentication attempts MUST be rate-limited.
- **FR-006**: The UI MUST offer no control actions: it MUST NOT send AT commands on the operator's behalf, start or end calls, send SMS, change configuration, restart anything, or alter modem or network state. The only new modem interaction this feature introduces is the periodic signal poll (FR-020).

**Lines overview**

- **FR-010**: The overview MUST show one card per known line with: line identifier/slot, carrier, transport (CS, VoWiFi or VoLTE), the line's own number masked to its last 4 digits (revealable on demand), registration state, time until registration expiry, time registered, active call count, and age of the last report.
- **FR-011**: For VoWiFi lines the card MUST show tunnel, child SA and Gm connection state and whether the uplink is currently held. For VoLTE lines it MUST show PDN and registration state.
- **FR-012**: Registration and health wording MUST be the same as `vowifi-status` / `volte-status`, so that the UI and the CLI never describe the same state differently.
- **FR-013**: A line whose data is older than the existing agent-staleness threshold MUST be shown as stale, with its age, instead of as healthy.
- **FR-014**: The overview header MUST show daemon uptime, build version, PBX (upstream SIP) registration state, and all currently active alerts.
- **FR-015**: The overview and Calls pages MUST refresh automatically at an interval no longer than 10 seconds while visible, and MUST pause refreshing while the browser tab is hidden.

**Signal strength**

- **FR-020**: The bridge MUST periodically read the signal strength of each modem it reaches over an AT port. A line whose SIM is read through a PC/SC reader has no modem AT port, so its signal shows as "not available". Each reading covers RSSI, plus RSRP/RSRQ/SINR when on LTE, and is taken at a configurable interval that defaults to 60 seconds.
- **FR-021**: Signal reads MUST go through the bridge's existing per-modem command path. They MUST NOT open a modem port independently, and MUST yield to call, SMS and recovery traffic.
- **FR-022**: Signal values MUST also be published on the existing metrics endpoint, so Grafana can graph them.
- **FR-023**: A "not known" or failed reading MUST be shown and exported as unknown (absent), never as zero or a sentinel number.

**Calls**

- **FR-029**: The bridge MUST record outbound calls (PBX/SIP client → carrier) in the call history, the same way it records inbound calls, with a direction of inbound or outbound. For an outbound call, the remote party is the dialled number. An outbound call that never connects is recorded as failed, or as missed if the far end didn't answer. Calls stored before this feature are treated as inbound.
- **FR-030**: The Calls page MUST show answered, missed and failed counts and average answered-call duration, for today and for the last 7 days, overall, per line and per direction.
- **FR-031**: The call log MUST be filterable by line, direction, transport, status and date range, newest first, and paged.
- **FR-032**: Calls in progress MUST be listed with line, remote party, direction, running duration and live jitter, packet loss and round-trip time. Unavailable quality values MUST be shown as unavailable.

**SMS**

- **FR-040**: The SMS page MUST list received messages from the bridge's own store, newest first and paged, with sender, line, transport, received time, body and forwarding status.
- **FR-041**: Viewing SMS MUST NOT send any command to a modem or change any message's state on the SIM or modem.
- **FR-042**: The SMS list MUST be filterable by line and searchable by sender and body text.
- **FR-043**: The UI MUST detect likely one-time codes (a standalone 4–8 digit number, preferring one near words like "OTP", "code" or "verification") and offer to copy the code on its own.
- **FR-044**: Message bodies and all other carrier- or caller-supplied text (caller ID, display names) MUST be shown as literal text, never interpreted as markup.

**Events and health**

- **FR-050**: The bridge MUST persist a timeline of operational events covering at least: alert raised and resolved (every alert category that tracks an ongoing condition; the one-shot SMS and missed-call alerts are excluded because each message and call already has its own row on the SMS and Calls pages), tunnel recovery, dead-peer loss, uplink hold start and end, scheduled restart, module initialised and failed, and line discovery failure. Each event records time, line (when applicable), kind and a short human-readable detail.
- **FR-051**: Events MUST be recorded whether or not the matching Discord alert is enabled, suppressed or delivered.
- **FR-052**: Events MUST survive bridge restarts and MUST be removed after a configurable retention period, defaulting to 30 days.
- **FR-053**: The Events page MUST list events newest first, paged, filterable by line and kind. A resolution event MUST show how long the condition lasted.
- **FR-054**: The Events page MUST show current internal health: modules active and failed, and the pending-store-writes queue depth.

**Config**

- **FR-060**: The Config page MUST show the effective configuration loaded by the running bridge.
- **FR-061**: Every secret MUST be redacted before it leaves the bridge: passwords, tokens, webhook URLs, and any value that came from an `env:` reference. Redaction MUST be by default, so that a newly added secret field is redacted without anyone having to remember to list it.

**Footprint and presentation**

- **FR-070**: The feature MUST NOT add more than 1 MB to the container image and MUST NOT add a JavaScript build toolchain to the image build.
- **FR-071**: The UI MUST work without any internet access: everything it needs is served by the bridge itself.
- **FR-072**: The UI MUST be usable on a phone-width screen as well as a desktop.
- **FR-073**: The UI MUST NOT draw historical time-series graphs. Where history over time is useful, it MUST link to the Grafana dashboard instead, if a dashboard URL is configured.
- **FR-074**: Serving the UI MUST NOT measurably affect call setup or audio. Under normal viewing (a few open tabs), its CPU use MUST stay negligible on the Raspberry Pi deployment.

### Key Entities

- **Line**: one SIM/modem slot the bridge manages. It has a carrier, transport, own number, registration and health state, signal reading and active calls. This view is assembled from the existing agent reports, not stored anew.
- **Signal reading**: a per-modem measurement taken at a point in time. It holds RSSI, plus LTE RSRP/RSRQ/SINR when available. Only the latest reading is kept for display; history lives in metrics.
- **Call record**: the existing stored call, with line, remote party (the caller for inbound calls, the dialled number for outbound ones), start time, duration, status (answered/missed/failed) and transport, plus a new **direction** (inbound/outbound). Rows that existed before this feature are inbound.
- **SMS record**: the existing stored message, with line, sender, body, received time, transport and forwarding status.
- **Event**: a new persisted record with time, line (optional), kind, severity and detail. A resolution event is linked to the raise it resolves.
- **Operator credential**: the single secret that grants access to the UI.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An operator can tell whether every line is healthy within 10 seconds of opening the UI, without SSH.
- **SC-002**: An operator can find and copy a one-time code from an SMS received in the last hour in under 15 seconds.
- **SC-003**: A line health change (e.g. tunnel down) is visible in an open overview within 15 seconds of the bridge noticing it.
- **SC-004**: For an incident that happened overnight, the operator can establish which line, what kind and how long it lasted from the Events page alone, without container logs or Discord.
- **SC-005**: No unauthenticated request returns line, call, SMS, event or config data. This is verified for every page and data endpoint.
- **SC-006**: No secret value from the configuration, including resolved `env:` values, appears in any page or response. This is verified by searching all UI responses for every configured secret.
- **SC-007**: The container image grows by less than 1 MB.
- **SC-008**: With the UI open in three tabs throughout, call setup times and audio-quality figures on the Pi are within normal run-to-run variation of a run without the UI.
- **SC-009**: With the UI disabled, the bridge's behaviour and metrics output are unchanged apart from the new signal gauges.
- **SC-010**: Every page loads in under 2 seconds on the Pi over the LAN, with 10,000 stored calls and 10,000 stored SMS.

## Assumptions

- **Who uses it**: a single operator (or a small trusted group sharing one credential). Multiple user accounts, roles and audit logs are out of scope.
- **One bridge per UI**: each bridge deployment (e.g. the local Vodafone host and the Jio Pi) serves its own UI showing only its own lines. Aggregating or linking several bridges is out of scope.
- **Authentication**: one operator password set in configuration, supporting `env:`, used on a login page that starts a 7-day session. No sign-up, password reset or SSO. Changing the password takes effect at the next bridge restart, which also ends all sessions.
- **Transport security**: the UI is served over plain HTTP on a trusted LAN, on its own port (default 8080) on all interfaces. Operators who need remote access put it behind their own TLS reverse proxy or VPN. Built-in TLS is out of scope.
- **Off by default**: an upgrade with an unchanged configuration serves no UI.
- **Own-number masking**: the line's own number is masked by default because screens get shared. Caller and SMS sender numbers are shown in full, because masking them would defeat the purpose of the call and SMS pages.
- **Event retention** is 30 days by default. Call and SMS retention is unchanged by this feature.
- **Signal polling** defaults to every 60 seconds. On a busy or recovering modem a poll may be skipped rather than queued.
- **Language**: English only. Light and dark themes follow the browser's preference.
- **History**: graphs and trends are Grafana's job. The UI shows current state, lists and totals.
- **Out of scope (explicitly)**, all of which vocat has: AT terminal, USSD, SMS sending, eSIM/profile management, ICCID card policy, network/cell/band lock, operator scan, flight mode, module or bridge restart, re-registration, proxy routing, notification-channel configuration, editing configuration. A narrowly scoped opt-in action (e.g. USSD balance check, "re-register line") may be proposed as a separate future feature.
- **Dependencies**: the existing agent status reports, the existing metrics, the existing call/SMS store and the existing per-modem command path are reused. The alert path gains a hook to record events.
