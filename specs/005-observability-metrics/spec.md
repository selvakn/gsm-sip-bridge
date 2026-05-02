# Feature Specification: Observability Metrics and Dashboard

**Feature Branch**: `005-observability-metrics`
**Created**: 2026-05-02
**Status**: Draft
**Input**: User description: "this is a pretty major feature. lets implement a modern snmp for monitoring and tracing. it should log all activities, registration, calls, registration renewal activities, failures, retries, call start, call end, incoming caller ids, and all sensible things. in the docker compose setup, add a simple snmp collection and visualization tool to make it easy to test. configure the dashboard with the data being collected."

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Real-Time Operational Visibility (Priority: P1)

An operator deploys the GSM-SIP bridge and wants to know what is happening at any moment without reading raw log files. The system exposes structured metrics covering all operational activities: SIP registration state, GSM module status, active calls, call counts, error counts, and retry activity. The operator opens a pre-configured dashboard in a web browser and sees live panels showing system health, per-module status, call activity over time, and error rates. No manual configuration of the dashboard is needed -- it comes ready to use out of the box with the Docker Compose setup.

**Why this priority**: Without metrics being emitted by the application, there is nothing to collect or visualize. This is the foundational capability.

**Independent Test**: Start the system with Docker Compose. Open the dashboard URL in a browser. Verify panels show module status, SIP registration state, and zero call counters. Place a test call and verify the dashboard updates in real time.

**Acceptance Scenarios**:

1. **Given** the system is running with at least one active EC20 module, **When** the operator opens the dashboard, **Then** they see the current SIP registration status (registered/unregistered) and per-module GSM status (active/failed).
2. **Given** the system is idle with no active calls, **When** the operator views the dashboard, **Then** they see call counters at zero and no active call indicators.
3. **Given** a GSM call arrives and is bridged to SIP, **When** the operator views the dashboard during the call, **Then** they see the active call count increment, the calling number, and which module is handling the call.
4. **Given** a call ends (either party hangs up), **When** the operator views the dashboard, **Then** the active call count decrements, total completed calls increments, and the call duration is recorded.

---

### User Story 2 - Failure and Retry Monitoring (Priority: P2)

An operator needs to diagnose why calls are failing or modules are not working. The system tracks and exposes metrics for all failure scenarios: SIP registration failures, SIP call failures (busy, timeout, error), GSM module initialization failures, module retry attempts, and module recovery events. The dashboard shows error rates over time and highlights modules in a failed state. The operator can correlate failures with timestamps to investigate root causes.

**Why this priority**: Failure tracking is essential for production troubleshooting, but the core metrics pipeline (P1) must exist first.

**Independent Test**: Disconnect a module or misconfigure SIP credentials. Verify the dashboard shows failure counts incrementing and the failed module highlighted. Fix the issue and verify the dashboard reflects recovery.

**Acceptance Scenarios**:

1. **Given** a SIP registration attempt fails, **When** the operator views the dashboard, **Then** the SIP registration failure counter increments and the failure reason is visible.
2. **Given** a module fails initialization, **When** the operator views the dashboard, **Then** that module is shown in a failed state with its failure reason.
3. **Given** the retry mechanism recovers a previously failed module, **When** the operator views the dashboard, **Then** the retry success counter increments and the module transitions to active.
4. **Given** an outbound SIP call fails (busy, timeout, unreachable), **When** the operator views the dashboard, **Then** the SIP call failure counter increments with the failure category.

---

### User Story 3 - Call Activity History and Trends (Priority: P3)

An operator wants to understand call patterns over time: how many calls per hour, average call duration, peak usage periods, and which modules handle the most traffic. The dashboard provides time-series panels for call volume, duration distributions, and per-module call distribution. This data helps with capacity planning (deciding how many modules are needed) and identifying usage trends.

**Why this priority**: Historical trends are valuable for capacity planning but not required for basic operational monitoring.

**Independent Test**: Place 10+ calls over a period and verify the dashboard shows call volume trends, average duration, and per-module distribution.

**Acceptance Scenarios**:

1. **Given** the system has handled calls over a period of time, **When** the operator views the call volume panel, **Then** they see a time-series chart of calls per interval.
2. **Given** calls of varying duration have been completed, **When** the operator views the duration panel, **Then** they see the average and distribution of call durations.
3. **Given** multiple modules are active, **When** the operator views the per-module panel, **Then** they see how many calls each module has handled.

---

### User Story 4 - Docker Compose One-Command Setup (Priority: P4)

An operator or developer runs `docker compose up` and gets the entire observability stack running: the GSM-SIP bridge, a metrics collector, and a visualization dashboard with pre-configured panels. No manual setup, no importing dashboard configurations, no connecting data sources. Everything is wired together automatically.

**Why this priority**: The Docker Compose setup is the delivery mechanism for easy testing, but the metrics themselves (P1-P3) must exist first.

**Independent Test**: Run `docker compose up` on a fresh machine. Open the dashboard URL. Verify all panels are present and connected to the data source without any manual steps.

**Acceptance Scenarios**:

1. **Given** a fresh clone of the repository, **When** the operator runs `docker compose up`, **Then** the bridge, metrics collector, and dashboard all start and connect automatically.
2. **Given** the stack is running, **When** the operator opens the dashboard URL, **Then** they see a pre-configured dashboard with all panels (system health, call activity, errors, per-module status) populated with live data.
3. **Given** the stack has been stopped and restarted, **When** the dashboard is accessed, **Then** previously collected metrics are available (metrics survive restarts within reason).

---

### Edge Cases

- What happens when the metrics endpoint is unavailable or the collector is down? The bridge continues operating normally. Metrics are fire-and-forget; monitoring infrastructure failures MUST NOT affect call handling.
- What happens when the system starts with no modules connected? The dashboard shows zero modules, zero calls, and a clear indication that no GSM modules are detected.
- What happens when hundreds of calls are processed? The metrics system handles high cardinality without degrading the bridge's call-handling performance.
- What happens when the dashboard is opened before any calls have occurred? All panels render correctly with zero values and appropriate "no data" states; no errors or broken panels.
- What happens when a module transitions rapidly between active/failed states (flapping)? Each state transition is captured as a metric event, and the dashboard reflects the current state accurately.

## Requirements *(mandatory)*

### Functional Requirements

**Metrics Emission**

- **FR-001**: System MUST emit a metric for each SIP registration attempt, including the outcome (success/failure) and failure reason if applicable.
- **FR-002**: System MUST emit a metric for each SIP registration renewal, including the outcome.
- **FR-003**: System MUST emit a metric for each incoming GSM call, including the module identifier and the caller ID (phone number).
- **FR-004**: System MUST emit a metric when a call is answered (GSM auto-answer).
- **FR-005**: System MUST emit a metric when an outbound SIP call is initiated, including the destination.
- **FR-006**: System MUST emit a metric when a call is fully bridged (audio connected bidirectionally).
- **FR-007**: System MUST emit a metric when a call ends, including the duration, which party hung up (GSM or SIP), and the module identifier.
- **FR-008**: System MUST emit a metric for each SIP call failure, categorized by type (busy, timeout, unreachable, other).
- **FR-009**: System MUST emit a metric for each module initialization attempt, including the module identifier and outcome (success/failure with reason).
- **FR-010**: System MUST emit a metric for each module retry attempt and its outcome.
- **FR-011**: System MUST emit a metric for the current number of active calls (gauge).
- **FR-012**: System MUST emit a metric for the current number of active modules and failed modules (gauge).
- **FR-013**: System MUST emit a metric for system uptime.

**Operational Safety**

- **FR-014**: Metrics emission MUST NOT block or delay call handling. If the metrics infrastructure is unavailable, the bridge MUST continue operating normally.
- **FR-015**: Metrics emission MUST add less than 1ms of overhead per event to the call handling path.

**Dashboard**

- **FR-016**: The Docker Compose setup MUST include a metrics collector and a visualization dashboard, pre-configured and ready to use.
- **FR-017**: The dashboard MUST include panels for: system overview (uptime, module count, SIP registration), call activity (active calls, total calls, calls per minute), error rates (SIP failures, module failures), and per-module breakdown.
- **FR-018**: The dashboard MUST auto-refresh and show near-real-time data (within 15 seconds of an event).
- **FR-019**: The dashboard MUST be accessible via a web browser at a documented URL and port.
- **FR-020**: The dashboard configuration MUST be version-controlled in the repository and automatically loaded on startup. No manual import or configuration required.

### Key Entities

- **Metric Event**: A structured data point representing a single operational event. Has a name (e.g., "call_started"), a timestamp, a set of labels (e.g., module_id, caller_id), and a value. Types include counters (monotonically increasing), gauges (current value), and histograms (distributions).
- **Dashboard Panel**: A visual element on the dashboard that queries and displays one or more metrics over time. Panels are pre-defined in a configuration file shipped with the repository.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An operator can determine system health (modules active, SIP registered, active calls) from the dashboard within 10 seconds of opening it.
- **SC-002**: All call lifecycle events (ring, answer, bridge, end) appear on the dashboard within 15 seconds of occurring.
- **SC-003**: The metrics system adds less than 1ms overhead per event to call handling, measured by comparing call setup time with and without metrics enabled.
- **SC-004**: Running `docker compose up` on a fresh clone produces a working dashboard with all panels within 60 seconds.
- **SC-005**: The dashboard correctly reflects the state of 4+ concurrent modules handling calls simultaneously.
- **SC-006**: The bridge continues handling calls normally when the metrics collector is stopped.

## Assumptions

- "Modern SNMP" is interpreted as structured metrics and observability (not the SNMP protocol). The system uses a push-based or pull-based metrics approach standard in modern infrastructure monitoring.
- The metrics endpoint is exposed locally; no authentication is needed for the metrics endpoint in the Docker Compose development setup. Production hardening of the monitoring stack is out of scope.
- The Docker Compose setup is for development and testing purposes. Production deployment of the monitoring stack is out of scope.
- Metrics data retention is handled by the collector's defaults. Long-term storage and archival are out of scope.
- The caller ID (phone number) is included in call metrics for operational tracing. No PII masking is applied in the development setup; production PII compliance is deferred to a future feature.
- The dashboard is English-only. Localization is out of scope.
- The existing structured logging (stdout with timestamps) continues to function alongside the new metrics. Metrics complement but do not replace logging.
