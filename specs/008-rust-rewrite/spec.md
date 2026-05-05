# Feature Specification: Rust Rewrite (gsm-sip-bridge v5.0.0)

**Feature Branch**: `008-rust-rewrite`
**Created**: 2026-05-04
**Status**: Draft
**Input**: User description: "lets do a rewrite in rust language. for pjsip, which is a C++ library - lets use ffi. for all others, keep it as native to rust as much as possible. ask and clarifying questions."

## Clarifications

### Session 2026-05-04

- Q: What is the primary driver of the rewrite, and how will success be measured? → A: Broad modernization — pursue all of memory safety + maintainability, performance/footprint improvements, and faster feature velocity together. No single primary driver.
- Q: What feature scope must the first Rust release ship with? → A: Full feature parity with C++ v4.1.x at the first Rust release; no feature deferred to later iterations.
- Q: What compatibility surface must the Rust build preserve for existing deployments? → A: Clean break — operators perform a one-time manual migration of configuration and persisted data when upgrading. Configuration format, database schema, metric names, and CLI may all evolve at the rewrite boundary.

### Session 2026-05-05

- Q: How are sensitive values (SIP password, Discord webhook URL) supplied to the bridge? → A: Configuration values may be either literal strings or `env:VAR_NAME` references that resolve at startup; CLI MUST NOT accept secrets; logs MUST redact secret-bearing keys.
- Q: What is the absolute end-to-end audio latency target the Rust release must meet? → A: ≤200 ms p95 one-way (mouth-to-ear), measured end-to-end from GSM caller speech onset to SIP-callee playback (and vice versa), under the documented v4.1.x test rig.
- Q: What is the maximum number of concurrent EC20 modules the v5.0.0 release must support on a single host? → A: Up to 8 modules per host; configurations beyond 8 are out of scope for v5.0.0.
- Q: What retention policy applies to persisted call and SMS records? → A: Keep all records indefinitely; no built-in pruning. The release ships a documented manual prune procedure (SQL `DELETE` snippet + `VACUUM`) for operators who need to bound store size.
- Q: In what form does the v4.1.x → v5.0.0 migration ship — a tool, a document, or both? → A: Document only. Every migration step (configuration rewrite, database schema conversion via copy-pasteable SQL, Grafana dashboard import, Docker Compose swap) is performed by hand following the published guide. No migration CLI ships in v5.0.0.

## User Scenarios & Testing

### User Story 1 - Bridge GSM Calls to SIP With Identical Behavior to v4.1.x (Priority: P1)

An operator who runs the existing C++ bridge today installs the new Rust release on the same hardware (one or more Quectel EC20 modules connected to a Linux host, with a SIP PBX reachable). After applying the documented migration of their configuration file, they start the new bridge. Incoming GSM calls auto-answer, the caller hears a ringing-style beep while the SIP extension is dialed, audio flows bidirectionally between GSM and SIP for the duration of the call, and the call ends cleanly when either party hangs up. Behavior across multiple cards, network registration loss, and PBX retries is indistinguishable from v4.1.x from the operator's perspective.

**Why this priority**: This is the core product. Without it, the rewrite delivers no value. Every other story below assumes this is working.

**Independent Test**: With one or more EC20 modules connected and a SIP PBX configured, dial a GSM number that lands on a connected module. Verify (a) the GSM caller is answered within the configured beep duration, (b) a SIP INVITE reaches the PBX with the correct DID, (c) audio is bidirectional once the SIP party answers, (d) call termination by either party is propagated to the other within seconds, and (e) the operator can repeat the test simultaneously across all connected cards without cross-talk or interference.

**Acceptance Scenarios**:

1. **Given** the Rust bridge is running with one functional module and SIP registered, **When** an incoming GSM call arrives, **Then** the bridge auto-answers, plays a beep to the caller, dials the configured SIP destination (or passes the GSM caller's number as the DID when no fixed destination is set), and bridges audio bidirectionally once the SIP party answers.
2. **Given** the Rust bridge is running with three functional modules, **When** three calls arrive simultaneously on three different modules, **Then** each call is bridged independently with no audio cross-talk and no degradation of the others when one hangs up.
3. **Given** an active bridged call, **When** the GSM caller hangs up, **Then** the bridge tears down the SIP leg within seconds and is ready to accept the next call on that module.
4. **Given** an active bridged call, **When** the SIP party hangs up, **Then** the bridge sends the GSM-side hangup command and the module returns to idle.
5. **Given** the Rust bridge is starting up, **When** some connected modules fail to initialize (e.g., SIM not registered), **Then** the bridge starts with the remaining functional modules, logs the failure reason for each failed module, and exits only if zero modules are functional.
6. **Given** the Rust bridge is running with one or more failed modules, **When** the underlying issue is resolved (e.g., SIM registers on network) within the next retry window, **Then** the failed module rejoins the active pool automatically without restarting the process.
7. **Given** the SIP PBX is unreachable when a GSM call arrives, **When** the SIP dial times out, **Then** the GSM caller hears an error indication, the call is recorded as failed, and the module returns to idle for the next call.
8. **Given** an active bridged call on the documented test rig, **When** mouth-to-ear one-way latency is measured in either direction across many talk-spurts, **Then** the p95 measurement is at most 200 ms.

---

### User Story 2 - Capture Incoming SMS Reliably and Forward to Discord (Priority: P2)

An operator with a Discord webhook configured receives a notification within seconds whenever an SMS arrives on any connected module. The message body, sender number, receiving module identifier, and arrival timestamp are visible in the Discord channel. Even when Discord is unreachable, the SMS is preserved locally and visible after the fact via the persisted store. SMS arrival never disrupts an in-progress call on the same module.

**Why this priority**: SMS forwarding is the second most-used feature in v4.1.x, important enough to be in v1 (per the full-parity decision) but the system still delivers its primary value (call bridging) without it.

**Independent Test**: Send an SMS to the SIM in any connected module while the bridge is running. Verify (a) within seconds, a Discord embed appears with sender, body, timestamp, and module ID, (b) a row is written to the persisted SMS store before the SMS is deleted from the SIM, and (c) if the same test is run with the Discord webhook intentionally unreachable, the row is still written to the store with a failure-status indicator and the SIM is still cleared.

**Acceptance Scenarios**:

1. **Given** the bridge is running with a valid Discord webhook configured, **When** an SMS arrives on a connected module, **Then** within seconds Discord shows an embed containing sender number, message body, arrival timestamp, and module identifier.
2. **Given** an active bridged call on a module, **When** an SMS arrives on the same module, **Then** the SMS is captured and forwarded without disrupting the call audio or signaling.
3. **Given** the Discord webhook is unreachable, **When** an SMS arrives, **Then** the message is still persisted to the local store with a failure indicator and the SIM is cleared, and the bridge continues operating normally.
4. **Given** SMS forwarding is disabled in configuration, **When** an SMS arrives, **Then** it is persisted to the local store but no Discord call is made and no error is raised.

---

### User Story 3 - Observe System Health Through Metrics and a Dashboard (Priority: P2)

An operator with Prometheus scraping the bridge sees current and historical health: how many modules are active, how many calls are in flight, SIP registration state, call duration percentiles, audio error rates, SMS throughput, and uptime. A Grafana dashboard renders all of this at a glance. Metrics are exposed in the Prometheus exposition format on a dedicated HTTP endpoint and never block call processing. Because the rewrite is a clean break, the operator accepts that metric names and labels may differ from v4.1.x and that the Grafana dashboard JSON ships with the new release.

**Why this priority**: Operations teams rely on this for production. Critical for v1 since we are committing to full parity, but secondary to call/SMS functionality from a value standpoint.

**Independent Test**: With the bridge running, fetch the metrics endpoint and verify (a) all documented metric names are present and have plausible values, (b) the Grafana dashboard included in the release renders without missing-data warnings against a fresh Prometheus scrape, and (c) high call volume on the bridge does not cause metrics scraping to fail or call processing to slow down measurably.

**Acceptance Scenarios**:

1. **Given** the bridge is running, **When** a metrics request is made to the configured port, **Then** the response is in Prometheus exposition format and includes counters and gauges for calls (incoming/answered/missed), SIP registration state, module health (active/failed counts), audio errors, SMS throughput and outcomes, call duration distribution, and uptime.
2. **Given** the released Grafana dashboard JSON has been imported and Prometheus is scraping the bridge, **When** the dashboard is opened, **Then** every panel resolves to data without "no data" or "unknown metric" warnings.
3. **Given** five concurrent calls are in flight, **When** Prometheus scrapes the metrics endpoint, **Then** the scrape returns within the configured Prometheus scrape timeout and no audio errors are introduced by the scrape.

---

### User Story 4 - Persist Calls and SMS for After-the-Fact Inspection (Priority: P2)

An operator can query a local store at any time and see every incoming GSM call (with caller ID, module, timestamp, duration, status, SIP destination dialed) and every incoming SMS (with sender, body, timestamp, module, forwarding status). The store survives process restarts and Discord outages. Data older than the running release is accessible after the operator follows the documented one-time migration to convert v4.1.x records to the new schema.

**Why this priority**: Persistence underpins both observability and compliance/auditing. Independent of metrics, the operator can inspect the raw record of what the bridge has done.

**Independent Test**: Run the bridge, complete a successful call and a missed call, receive an SMS, then stop the bridge. Verify the on-disk store contains the expected rows, restart the bridge, and verify the rows are still present and a new round of activity appends correctly.

**Acceptance Scenarios**:

1. **Given** the bridge is running, **When** an incoming GSM call is answered and bridged, **Then** a record is written that includes the module identifier, caller number, started-at timestamp, duration, final status (answered/missed/failed), and SIP destination dialed.
2. **Given** the bridge restarts after a crash, **When** it comes back up, **Then** all previously persisted call and SMS records are readable and the store is in a consistent state ready for new writes.
3. **Given** the operator has v4.1.x records in the legacy database, **When** they run the documented migration steps (copy-pasteable SQL from the migration guide) once before starting the new release, **Then** all legacy records are accessible in the new schema with no data loss.

---

### User Story 5 - Migrate From v4.1.x in a Single Documented Step (Priority: P3)

An operator currently running v4.1.x in production reads a migration guide that lists every breaking change (configuration format change, database schema change, metric renames, CLI changes, Docker Compose changes), executes the documented steps once, and then runs the new release. The guide explicitly enumerates every breaking change so the operator can audit it before committing. After migration, the bridge resumes operation against the same physical hardware, same SIP credentials, and same persisted history.

**Why this priority**: Migration is a one-time operator concern, not an ongoing user journey. It must be solid for the upgrade event but is not part of normal operation.

**Independent Test**: Take a working v4.1.x deployment (config file, populated database, Grafana dashboard, running Docker Compose stack). Follow the migration guide step-by-step entirely by hand (no migration CLI ships in v5.0.0). Start the new release. Verify the bridge runs with the same physical setup and the operator can see their historical calls/SMS in the new store.

**Acceptance Scenarios**:

1. **Given** a v4.1.x deployment with populated configuration and database, **When** the operator follows the published migration guide end-to-end (rewriting the configuration by hand, running the supplied SQL against their database, importing the new Grafana dashboard, and swapping the Docker Compose service definition), **Then** the new release starts successfully against the same hardware and the operator can see both pre- and post-migration records in a single store.
2. **Given** the migration guide, **When** the operator reads it, **Then** every breaking change between v4.1.x and v5.0.0 is enumerated explicitly with before/after examples (configuration keys, database tables/columns, metric names, CLI flags, Docker Compose service definition), and every database conversion step is provided as copy-pasteable SQL.
3. **Given** the operator wants to roll back, **When** they keep the v4.1.x binary and original configuration available, **Then** they can revert by re-applying the v4.1.x configuration without the migration steps having corrupted any pre-migration files in place (the migration steps produce new artifacts side-by-side rather than overwriting).

---

### Edge Cases

- USB enumeration order changes between boots: stable hardware identifiers (derived from each module's USB serial number) must remain consistent across reboots so module IDs in logs, metrics, and the database stay meaningful.
- ModemManager is running on the host: the bridge must detect this at startup and warn the operator, since ModemManager probing corrupts AT command sessions on `ttyUSB*` ports.
- A module is unplugged while a call is in progress: the bridge must terminate the affected call cleanly, mark the module as failed, and continue serving other modules.
- The SIP server momentarily drops registration mid-call: the in-flight call should not be affected; subsequent registration retries should reflect in metrics and resume registration without operator intervention.
- The persisted store file is missing or corrupt at startup: the bridge must either initialize a fresh store with the current schema or refuse to start with a clear error message identifying the problem (no silent data loss).
- The Discord webhook is rate-limited (HTTP 429): SMS persistence must still succeed; webhook delivery should retry per the rate-limit response and never block SMS capture for subsequent messages.
- Two SMS messages arrive on the same module within milliseconds: both must be captured, persisted, forwarded, and deleted from the SIM in order.
- A migration is interrupted partway through: the operator must be able to resume or restart it without ending up in a half-migrated state where the new release cannot start.
- Audio underrun/overrun on a single module: the affected module's call may degrade, but other modules' calls and the rest of the system must be unaffected; the error must be counted in metrics.
- The PBX's SIP TLS certificate expires or is misconfigured (when transport=tls is used): registration fails with a clear, operator-actionable log message rather than a silent loop.
- A configuration value uses an `env:VAR_NAME` reference but the variable is unset at process start: the bridge must refuse to start with an error naming the missing variable, rather than running with an empty credential.

## Requirements

### Functional Requirements

#### Implementation Direction (binding constraints from the user)

- **FR-001**: The bridge MUST be implemented in Rust as a from-scratch rewrite of the v4.1.x C++17 codebase, replacing it entirely (no shared C++ binaries in production after the rewrite ships).
- **FR-002**: PJSIP MUST be integrated through Rust FFI (foreign-function interface) over the PJSIP library. PJSIP itself MUST NOT be reimplemented in Rust.
- **FR-003**: All functionality other than PJSIP integration MUST be implemented using native Rust libraries wherever a viable option exists, avoiding additional FFI bindings to C/C++ unless an underlying system library leaves no Rust-native alternative (e.g., ALSA, libusb, or other Linux audio/USB system APIs that have no pure-Rust substitute).

#### Feature Parity (from the v4.1.x README, in scope at the first Rust release)

- **FR-010**: The bridge MUST auto-answer incoming GSM calls on every connected Quectel EC20 module that initialized successfully.
- **FR-011**: The bridge MUST place an outbound SIP call to either the configured fixed SIP destination, or — when no fixed destination is configured — to the GSM caller's number as the DID, allowing the PBX inbound route to decide the final destination. The outbound SIP INVITE MUST carry the GSM caller's number in a `P-Asserted-Identity` header and an `X-GSM-Caller-ID` header so the PBX can use it for routing or display. Leading `+` characters MUST be stripped from the caller ID when constructing the SIP request URI (many PBXes reject E.164 `+` prefixes in the user part).
- **FR-012**: The bridge MUST bridge audio bidirectionally between the GSM module's USB audio device and the SIP RTP media for the duration of the call. Specifically, the PJSIP `on_call_media_state` callback MUST call `pjsua_conf_connect(call_slot, 0)` and `pjsua_conf_connect(0, call_slot)` to wire the call's conference slot to the sound device slot (slot 0). The sound device MUST be set to the correct ALSA device (matched by card name, not index) before the outbound call is placed.
- **FR-013**: The bridge MUST play a configurable ringing-style beep tone to the GSM caller while the SIP extension is being dialed and stop it when the SIP party answers. Implementation uses PJSIP's `pjmedia_tonegen` connected to the conference bridge (slot 0 → sound device). The tone starts on SIP call states CALLING/EARLY and stops on CONFIRMED or DISCONNECTED, driven by the `on_call_state` callback.
- **FR-014**: The bridge MUST support up to 8 connected EC20 modules concurrently on a single host, each capable of bridging its own simultaneous call. Each module MUST handle its calls in isolation: a fault, hang-up, or restart on one module MUST NOT affect others. Configurations with more than 8 modules are out of scope for v5.0.0.
- **FR-015**: At startup, the bridge MUST scan the USB bus for devices matching the Quectel EC20 vendor/product identifier, derive a stable identifier for each module from its USB hardware serial number (so the same physical module always gets the same ID across boots and USB enumeration order), and map each module's AT command port to its USB audio device.
- **FR-016**: The bridge MUST start successfully when at least one module is functional. It MUST log per-module failure reasons in a startup summary and MUST exit with an error only if zero modules are functional.
- **FR-017**: The bridge MUST retry initialization of failed modules at a configurable interval (default 30 seconds) for the lifetime of the process; recovered modules MUST rejoin the active pool without a process restart.
- **FR-018**: The bridge MUST detect ModemManager running on the host and log a clearly worded warning at startup explaining that it can corrupt AT sessions on `ttyUSB*` ports.
- **FR-019**: The bridge MUST detect when a module becomes unresponsive during operation (serial errors, NO CARRIER without prior call, USB device removed) and tear down any in-flight call on that module cleanly while continuing to operate other modules.
- **FR-020**: The bridge MUST register with the configured SIP server using a single shared registration across all modules, supporting UDP, TCP, and TLS transports.
- **FR-021**: The bridge MUST handle SIP failures (busy, unreachable, dial timeout) by terminating the GSM leg with a clearly distinguishable error indication and recording the call's final status in the persisted store. Additionally, when the SIP peer disconnects mid-call (detected via `on_call_state` callback reaching `PJSIP_INV_STATE_DISCONNECTED`), the bridge MUST signal the GSM module worker to issue `AT+CHUP` and return the module to idle. All PJSIP API calls from Rust threads MUST be preceded by `pj_thread_register()` since tokio may schedule work on threads unknown to PJSIP's internal thread checker.
- **FR-022**: The audio pipeline MUST connect ALSA capture and playback to the SIP media path with end-to-end one-way audio latency (mouth-to-ear, in either direction) of at most 200 ms at p95, measured under the documented v4.1.x test rig. The pipeline MUST NOT use unbounded buffering. Audio MUST flow through a lock-free single-producer single-consumer pathway between the audio I/O and the SIP media handler so that backpressure or stalls on one side cannot block the other indefinitely.
- **FR-030**: The bridge MUST monitor incoming SMS on every connected module. When an SMS arrives, the bridge MUST read it from the module, persist it to the local store, delete it from the SIM card, and (if a Discord webhook URL is configured) post it to that webhook as a rich notification containing sender number, message body, arrival timestamp, and receiving module identifier.
- **FR-031**: SMS persistence to the local store MUST happen before deletion from the SIM. SMS forwarding to Discord MUST NOT block SMS capture, persistence, or call processing on any module.
- **FR-032**: When the Discord webhook is unreachable or returns an error, the bridge MUST record the forwarding outcome in the persisted store and continue operating without disrupting any other functionality.
- **FR-040**: The bridge MUST persist every incoming GSM call (module identifier, caller number, started-at timestamp in UTC, duration in seconds, final status — answered/missed/failed, SIP destination dialed) and every incoming SMS (module identifier, sender, body, arrival timestamp, forwarding status — pending/sent/failed/skipped) to a single local datastore that survives process restarts and supports concurrent read/write across the bridge's threads.
- **FR-041**: The persisted store MUST be inspectable with a widely available command-line tool so operators can run ad-hoc queries without writing custom code.
- **FR-042**: The bridge MUST keep all call and SMS records indefinitely. There is no built-in time- or size-based pruning in v5.0.0. The release MUST ship a documented manual prune procedure in operator documentation that includes copy-pasteable SQL for time-based deletion and reclaiming space (e.g., `VACUUM`), so an operator who needs to bound store size can do so without writing custom tooling.
- **FR-050**: The bridge MUST expose a metrics endpoint over HTTP in the Prometheus exposition format, on a port configurable via environment variable (default 9091). Metrics MUST cover GSM call counts and outcomes, SIP call counts and outcomes, SIP registration state, module initialization and retry counts, audio error counts, SMS receive and forward counts and outcomes, persisted-store write outcomes, active-call gauge per module, registration-state gauge, active/failed module gauges, process uptime, and call duration as a histogram with buckets ranging from one second to thirty minutes.
- **FR-051**: Metrics scraping MUST be non-blocking with respect to call processing: a slow or stuck scraper MUST NOT degrade audio or signaling.
- **FR-052**: The release MUST ship a Grafana dashboard JSON file that renders against the new metric names and labels and covers, at minimum: system overview, GSM and SIP call rates, active calls per module, call duration percentiles, SIP registration timeline, module health and retry counts, audio and SIP error rates, and SMS forwarding outcomes.
- **FR-060**: The release MUST ship a Docker Compose stack that runs the bridge, Prometheus, Grafana, and a database browser, configured for host networking and pre-provisioned with the included Grafana dashboard, equivalent in scope to the v4.1.x stack.
- **FR-061**: The release MUST include the udev rule (or its replacement) that prevents ModemManager from claiming `ttyUSB*` ports of EC20 modules, so the v4.1.x deployment guidance for that step continues to apply with at most a path/name change.
- **FR-062**: The release MUST provide a CLI sufficient to run the bridge against a configuration file, enable verbose SIP and AT logging, and override module auto-discovery to operate against a single explicitly specified serial port and audio device for debugging.

#### Clean-Break Constraints (from the user)

- **FR-070**: Configuration format MAY change away from the v4.1.x INI structure. Whatever format is chosen, every v4.1.x configuration key MUST have a documented equivalent in the new format so operators can migrate by hand.
- **FR-071**: The persisted-store schema MAY change. Whatever schema is chosen, the release MUST ship — within the migration guide — a documented manual procedure that converts a populated v4.1.x database to the new schema with no data loss. The procedure MUST consist of copy-pasteable SQL (or equivalent) that an operator can run against their existing database without writing custom code. No migration CLI ships in v5.0.0.
- **FR-072**: Metric names and labels MAY change. Whatever names are chosen, the release MUST ship the new Grafana dashboard pre-configured for them and the migration guide MUST document the rename mapping for any operator who has built custom dashboards or alerts.
- **FR-073**: CLI flags and the binary name MAY change. The migration guide MUST document the before/after flag mapping. The release MUST NOT silently overwrite a v4.1.x binary or its configuration on the same host.
- **FR-074**: A migration guide MUST exist that enumerates every breaking change between v4.1.x and the Rust release with before/after examples, in a single document the operator can audit before committing to the upgrade.

#### Secret Handling

- **FR-075**: Configuration values for known-sensitive keys (SIP password, Discord webhook URL, and any future credential-bearing key) MUST accept either a literal string or an `env:VAR_NAME` reference that the bridge resolves at startup from the process environment.
- **FR-076**: The CLI MUST NOT accept any sensitive value as an argument or flag. Operators supply secrets only via the configuration file (literal or `env:` reference).
- **FR-077**: At startup, when an `env:VAR_NAME` reference cannot be resolved (variable unset or empty) for any required sensitive key, the bridge MUST refuse to start with a clearly worded error identifying the missing variable; it MUST NOT fall back to an empty string or a default.
- **FR-078**: Logging (including verbose SIP and AT modes) and metrics labels MUST redact any value that originated from a sensitive key, regardless of whether the value was supplied as a literal or via an `env:` reference. The redacted form MUST be deterministic (e.g., a fixed placeholder) and MUST NOT leak length or content.

#### Cross-Cutting Quality (from the modernization driver)

- **FR-080**: The implementation MUST minimize use of `unsafe` Rust. Where `unsafe` is unavoidable (PJSIP FFI boundary, ALSA system calls, USB system calls), each `unsafe` block MUST encapsulate a small, audited region with safe wrappers exposed to the rest of the codebase.
- **FR-081**: The implementation MUST prefer well-maintained Rust crates from the ecosystem over hand-rolled equivalents for general-purpose concerns (configuration parsing, HTTP server for metrics, HTTP client for Discord, persisted-store access, structured logging, command-line argument parsing, async runtime if used).
- **FR-082**: Logging MUST be structured (key-value or equivalent) and configurable to verbose mode for SIP and AT command tracing as in v4.1.x.

### Key Entities

- **Module**: A connected Quectel EC20 modem, uniquely identified by a stable hardware-derived ID. Tracked attributes: serial port path, ALSA audio device, registration state on the GSM network, current call (if any), health status (active/failed and reason).
- **Call Record**: One incoming GSM call, scoped to the module that received it. Tracked attributes: module ID, caller number, started-at timestamp, duration, final status (answered/missed/failed), SIP destination dialed.
- **SMS Record**: One incoming SMS message, scoped to the module that received it. Tracked attributes: module ID, sender number, body, arrival timestamp, forwarding status (pending/sent/failed/skipped).
- **SIP Account**: The single shared SIP server registration used by every module's bridged calls. Tracked attributes: server, port, username, transport, current registration state.
- **Bridge Configuration**: The operator-supplied settings that govern the running bridge, covering at least SIP credentials, optional fixed SIP destination, SIP dial timeout, SMS enable flag, Discord webhook URL, persisted-store path, and metrics port. Sensitive values (SIP password, Discord webhook URL) may be provided as literal strings or as `env:VAR_NAME` references resolved at startup.
- **Migration Guide**: The operator-facing document that maps every v4.1.x configuration key, database table/column, metric name, CLI flag, and Docker Compose service to its v5.0.0 equivalent, with before/after examples, and supplies copy-pasteable SQL for the database schema conversion. This document is the sole migration deliverable in v5.0.0 — no migration CLI is shipped.

## Success Criteria

### Measurable Outcomes

- **SC-001**: 100% of v4.1.x acceptance scenarios across the existing six specs (`001-gsm-audio-echo` through `006-sms-discord-forward`) pass when re-executed against the Rust release, with no regressions in observable behavior.
- **SC-002**: An operator can complete the manual migration from a working v4.1.x deployment (configuration file, populated database, Docker Compose stack, Grafana dashboard) to a running v5.0.0 deployment in under 30 minutes by following only the published migration guide, without needing to read source code.
- **SC-003**: End-to-end one-way audio latency (mouth-to-ear, either direction) on the Rust release is at most 200 ms at p95 measured on the documented v4.1.x test rig, and is no worse than the v4.1.x baseline measured on the same hardware.
- **SC-004**: Steady-state CPU utilization and resident memory of the Rust release on a representative load (three concurrent bridged calls plus SMS at typical operator volume) are no worse than the v4.1.x baseline on the same hardware.
- **SC-005**: A continuously running Rust release achieves 30 days of production operation with zero memory-safety incidents (no segfaults, no use-after-free, no data-race-induced crashes) attributable to the bridge process.
- **SC-006**: The Grafana dashboard shipped with the release renders every panel without "no data" or "unknown metric" warnings on a fresh install, end-to-end, within 5 minutes of bringing up the Docker Compose stack.
- **SC-007**: At least one new feature that was previously deferred or considered risky in the C++ codebase ships within 90 days of the rewrite reaching production, demonstrating the feature-velocity benefit.
- **SC-008**: A new contributor can clone the repository, install dependencies, build the bridge, run the test suite, and exercise a single-card debug run end-to-end in under 60 minutes by following only the README and migration guide, on a fresh Linux machine that meets the documented prerequisites.
- **SC-009**: The fraction of source lines inside `unsafe` blocks (excluding generated FFI bindings) is below 5% of the total Rust source, and every `unsafe` block has a comment justifying why it is needed and what invariant it upholds.
- **SC-010**: With 8 EC20 modules connected and 8 concurrent bridged calls in flight on the documented test rig, the bridge sustains all 8 calls for at least 5 minutes with no audio underrun/overrun on any module and the latency target from SC-003 still holds across all 8 calls.

## Assumptions

- The deployment target remains Linux (Debian/Ubuntu and equivalent distributions); other host operating systems are out of scope for v5.0.0 just as they were for v4.1.x.
- Hardware target remains the Quectel EC20 module connected over USB with USB Audio Class enabled per the v4.1.x one-time setup; this rewrite does not introduce support for other modems.
- PJSIP is available on the host as a system package or built from source per the v4.1.x prerequisite; the rewrite does not change PJSIP version pinning policy beyond what the planning phase decides.
- ALSA is the audio subsystem; PipeWire/PulseAudio compatibility is not promised beyond what ALSA's own compatibility shims already provide.
- A single SIP server registration shared across all modules continues to be the supported topology; multi-account SIP is explicitly out of scope for v5.0.0 but is the kind of feature whose addition the rewrite is intended to make easier (per SC-007).
- Operators upgrading from v4.1.x are willing to perform a one-time manual migration at the cutover (per Q3 answer); rolling upgrades or blue/green migration tooling are out of scope.
- "Lock-free" in the audio pipeline means the data path between ALSA I/O and the SIP media handler has no mutexes on the per-frame hot path, consistent with the v4.1.x SPSC ring buffer design; the chosen Rust implementation is left to the planning phase but the property is required.
- The persisted store remains a single embedded file-based database (consistent with the v4.1.x SQLite choice); switching to a server-based database is out of scope for v5.0.0.
- Metric and dashboard naming conventions in v5.0.0 may follow OpenTelemetry / Prometheus semantic-convention norms for the project namespace (e.g., a single, consistent prefix per metric); the specific naming scheme is decided in planning.
- The Rust toolchain version is pinned via the standard project mechanism; the chosen MSRV is decided in planning but is at least new enough to use stable async features if async is adopted.
