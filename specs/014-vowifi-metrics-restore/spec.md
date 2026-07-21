# Feature Specification: Restore Call and SMS Observability Under VoWiFi

**Feature Branch**: `014-vowifi-metrics-restore`
**Created**: 2026-07-21
**Status**: Draft
**Input**: User description: "after the vowifi integration, looks like the metrics integration is broken. I dont see any call or sms releated metrics in grafana — investigate and specify the fix"

## Background

Before VoWiFi, every inbound call and SMS was handled inside the single
long-running daemon process, which is also the process that owns the
Prometheus registry and serves the metrics endpoint that Prometheus scrapes.
Every call and SMS therefore showed up on the dashboard and in the SQLite
call/SMS history as a side effect of being handled at all.

With VoWiFi enabled, inbound calls and SMS no longer arrive over the
circuit-switched path. They arrive over the operator's IMS core through the
ePDG tunnel and are handled by two *separate* supervised processes (the
IMS-facing agent inside the tunnel's network namespace, and the PBX-facing
agent in the default namespace). Neither of those processes exposes a metrics
endpoint, neither records call activity, and the counters one of them does
increment accumulate in a registry nothing ever reads. The daemon that *is*
scraped keeps reporting its own health gauges normally — which is why the
symptom presents as "only call and SMS metrics disappeared" rather than
"metrics are down".

The same root cause also leaves VoWiFi calls absent from the persisted call
history: nothing on the VoWiFi path writes a call record, so the call table
and its read-only browser show only circuit-switched calls.

Observability is not a nice-to-have here: this deployment is remote and
unattended, and the dashboard plus the call/SMS history are the only way an
operator can tell whether the bridge is carrying traffic at all.

## Clarifications

### Session 2026-07-21

- Q: When the process that collects and exposes metrics is unavailable, what happens to observability events? → A: Bounded in-memory buffer in the agent, flushed on reconnect; drop and count only when the bound is exceeded
- Q: What module identity do VoWiFi calls and SMS carry in metrics and history? → A: The existing card identifier of the modem whose SIM VoWiFi uses — the same value the circuit-switched path emits
- Q: How do point-in-time indicators recover after the collecting process restarts? → A: Agents periodically re-report their current state; the collector converges within one report interval
- Q: Are both transports expected to carry traffic concurrently? → A: Count both, differentiated by the transport attribute. Concurrency is not expected in practice, but the observability layer must not assume exclusivity either way — stay generic
- Q: What transport value do call/SMS rows written before this feature carry? → A: Backfill them all as circuit-switched (which is what they factually are); the field is required for every row going forward

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Inbound VoWiFi calls appear on the dashboard (Priority: P1)

An operator watching the Grafana dashboard for a VoWiFi-enabled deployment
sees inbound calls appear as they happen — call counts by outcome, currently
active calls, and call duration distribution — using the same panels that
already exist for circuit-switched calls, with no panel edits.

**Why this priority**: This is the reported breakage and the largest blind
spot. Without it there is no way to know from the dashboard whether the
bridge answered a single call today.

**Independent Test**: Enable VoWiFi, place an inbound call to the SIM's
number, let it be answered and hung up. The dashboard's call panels move.
Delivers value on its own even if SMS and diagnostics remain unfixed.

**Acceptance Scenarios**:

1. **Given** VoWiFi is enabled and both agents are healthy, **When** an
   inbound call arrives, is answered at the PBX extension, and ends, **Then**
   the dashboard shows one additional answered call, the active-call panel
   rose to 1 for the duration of the call and returned to 0, and the call's
   duration is reflected in the duration distribution.
2. **Given** VoWiFi is enabled, **When** an inbound call arrives but cannot be
   bridged to the PBX (extension busy, no answer, or bridge setup failure),
   **Then** the dashboard shows one additional call with a non-answered
   outcome, and the active-call count returns to 0.
3. **Given** VoWiFi is disabled, **When** an inbound circuit-switched call is
   handled, **Then** all call metrics behave exactly as they did before this
   feature — same metric names, same panels, same values.
4. **Given** a deployment where both the circuit-switched path and the VoWiFi
   path are live, **When** a single inbound call is handled, **Then** it is
   counted exactly once, and the transport it arrived on is distinguishable
   on the dashboard — with no assumption that only one transport can be
   carrying traffic.

---

### User Story 2 - Inbound VoWiFi SMS appear on the dashboard and in history (Priority: P1)

An operator sees SMS received over VoWiFi counted on the dashboard alongside
circuit-switched SMS, with the Discord forwarding outcome (delivered vs
failed) visible, and finds those messages in the persisted SMS history.

**Why this priority**: Equal to calls in the reported breakage. SMS
forwarding is a primary function of the deployment, and a silent forwarding
failure is currently invisible.

**Independent Test**: With VoWiFi enabled, send an SMS to the SIM's number.
The received-SMS panel increments, the forwarding-outcome panel shows the
result, and the message is in the SMS history table.

**Acceptance Scenarios**:

1. **Given** VoWiFi is enabled and SMS forwarding is configured, **When** an
   SMS arrives over the IMS path and is forwarded successfully, **Then** the
   dashboard shows one additional received SMS and one additional successful
   forward.
2. **Given** VoWiFi is enabled and the forwarding destination is unreachable,
   **When** an SMS arrives, **Then** the dashboard shows one additional
   received SMS and one additional failed forward, and the message is still
   present in the SMS history with a failed forwarding status.
3. **Given** VoWiFi is disabled, **When** an SMS arrives over the
   circuit-switched path, **Then** SMS metrics behave exactly as before.

---

### User Story 3 - VoWiFi calls are in the persisted call history (Priority: P2)

An operator browsing the call history (via the read-only database browser or
direct queries) finds VoWiFi calls recorded with caller, start time,
duration, outcome, and PBX destination — the same fields circuit-switched
calls already carry.

**Why this priority**: Metrics answer "how many"; the history answers "who
called and when", which is what the operator actually needs when following up
on a specific call. It is separable from the dashboard work and slightly less
urgent than it.

**Independent Test**: Place an inbound VoWiFi call, then query the call
history and confirm a matching row exists with correct caller and duration.

**Acceptance Scenarios**:

1. **Given** VoWiFi is enabled, **When** an inbound call is answered and
   ends, **Then** a call record exists with the caller's number, the start
   time, a duration matching the answered portion of the call, an answered
   outcome, and the PBX destination that was dialed.
2. **Given** VoWiFi is enabled, **When** an inbound call is never answered,
   **Then** a call record exists with a non-answered outcome and zero
   duration.
3. **Given** a deployment carrying both transports, **When** the history is
   queried, **Then** each record identifies which transport carried it.
4. **Given** a deployment upgraded from a build that predates this feature,
   **When** the history is queried, **Then** all pre-existing records are
   still present and are identified as circuit-switched, and no record has a
   blank transport.

---

### User Story 4 - VoWiFi-specific health is visible (Priority: P3)

An operator can see, without reading logs or shelling into the container,
whether the IMS registration is currently alive, whether the ePDG tunnel is
up, and why recent calls failed to bridge.

**Why this priority**: This is new visibility rather than restored
visibility, and the operator can survive without it while the P1 items are
being fixed — but it is what turns "no calls today" from ambiguous into
diagnosable.

**Independent Test**: Break the tunnel (or let the registration lapse) and
confirm the corresponding health indicator changes state on the dashboard
within one scrape interval.

**Acceptance Scenarios**:

1. **Given** VoWiFi is enabled, **When** the IMS registration is active,
   **Then** a registration-state indicator reads "registered", and it reads
   "not registered" whenever the registration has lapsed or been rejected.
2. **Given** VoWiFi is enabled, **When** the ePDG tunnel drops and later
   reconnects, **Then** a tunnel-state indicator reflects both transitions.
3. **Given** an inbound call is declined because the bridge could not be set
   up, **When** the operator looks at the dashboard, **Then** the failure is
   attributed to a reason category (bridge setup failure, ring timeout,
   caller cancelled, PBX declined) rather than being an undifferentiated
   failure count.

---

### Edge Cases

- **Agent restarts.** Both agents are supervised and restart automatically
  after a crash. Counters must not reset to zero on the dashboard, and must
  not jump backwards in a way that makes rate calculations produce nonsense.
- **Daemon restarts independently of the agents.** A call in progress when the
  daemon restarts must not leave the active-call gauge stuck above zero
  forever, nor leave registration and tunnel state blank until the next call
  happens — the agents' recurring state report re-establishes the truth.
- **Events generated while the collecting process is unavailable.** If the
  process that owns the metrics registry is down or unreachable when a call
  or SMS event occurs, the call or SMS itself must still complete normally —
  observability must never be able to fail a call. Events from a brief
  outage are delivered once it returns; a long outage overflows the buffer
  and the overflow is counted rather than silently lost.
- **Both transports live simultaneously.** Circuit-switched and VoWiFi paths
  can both be active; no call or SMS may be counted twice, and no call may be
  attributed to the wrong transport. Concurrent traffic on both is not
  expected in normal operation, but nothing in the observability layer may
  depend on that being true.
- **VoWiFi enabled but never registers.** With the tunnel or registration
  permanently failing, the dashboard must clearly show "not registered"
  rather than an absence of data indistinguishable from "no traffic".
- **A call that is answered and immediately dropped.** Sub-second calls must
  still produce a record and land in the shortest duration bucket, not be
  skipped.
- **Sustained call volume.** A burst of calls and SMS must not cause
  observability work to delay call setup or audio.

## Requirements *(mandatory)*

### Functional Requirements

**Restoring call metrics**

- **FR-001**: The system MUST count every inbound call carried over the
  VoWiFi path, categorised by outcome (answered, not answered, failed), on
  the same metric the circuit-switched path already uses.
- **FR-002**: The system MUST report the number of calls currently active on
  the VoWiFi path, returning to zero when no call is in progress.
- **FR-003**: The system MUST record the duration of each answered VoWiFi
  call into the same duration distribution the circuit-switched path uses.
- **FR-004**: The system MUST count the PBX-side leg of each VoWiFi call by
  outcome, equivalently to how outbound SIP legs are counted on the
  circuit-switched path.
- **FR-005**: All call metrics MUST identify which transport carried the
  call, so an operator can view them combined or separately.

**Restoring SMS metrics**

- **FR-006**: The system MUST count every SMS received over the VoWiFi path
  on the same metric the circuit-switched path already uses.
- **FR-007**: The system MUST count the forwarding outcome (delivered vs
  failed) of every VoWiFi-received SMS on the same metric the
  circuit-switched path already uses, such that the count is visible to the
  operator rather than accumulating where nothing reads it.
- **FR-008**: SMS metrics MUST identify which transport carried the message.

**Call history**

- **FR-009**: The system MUST persist a call record for every inbound VoWiFi
  call, carrying the same fields circuit-switched call records carry: caller
  identity, start time, duration, outcome, and PBX destination.
- **FR-010**: VoWiFi call and SMS records MUST be written to the same tables
  and the same database file as circuit-switched records, so a single query
  or browser view covers both.
- **FR-011**: Each persisted call and SMS record MUST identify the transport
  that carried it.
- **FR-011a**: VoWiFi calls and SMS MUST be attributed, in both metrics and
  persisted records, to the same module identity the circuit-switched path
  uses for the modem whose SIM carried them — so a per-module view shows that
  card's complete traffic across both transports, and the identity stays
  stable across restarts.
- **FR-011b**: Subscriber identifiers (IMSI) MUST NOT appear in metric
  attributes or persisted records.
- **FR-011c**: Call and SMS records that predate this feature MUST be
  backfilled as circuit-switched, and the transport field MUST be populated
  for every record thereafter, so that no record has an absent or unknown
  transport.
- **FR-011d**: Upgrading an existing deployment MUST preserve all existing
  call and SMS history and MUST NOT require the operator to migrate, reset,
  or recreate the database by hand.

**VoWiFi-specific health**

- **FR-012**: The system MUST expose the current IMS registration state as a
  point-in-time indicator, and MUST count registration attempts by outcome.
- **FR-013**: The system MUST expose the current ePDG tunnel state as a
  point-in-time indicator.
- **FR-014**: The system MUST count calls that failed to bridge, attributed
  to a bounded set of reason categories, with the category set small enough
  that it cannot grow without bound across deployments.

**Delivery and correctness constraints**

- **FR-015**: All call and SMS metrics MUST be reachable by the existing
  monitoring configuration without requiring the operator to add or edit
  scrape targets.
- **FR-016**: Existing dashboard panels MUST continue to function without
  being rewritten; any change to the dashboard MUST be additive.
- **FR-017**: A single call or SMS MUST be counted exactly once, regardless of
  how many processes were involved in handling it.
- **FR-017a**: Each transport MUST be counted independently and remain
  separable by the transport attribute. The observability layer MUST NOT
  assume the two transports are mutually exclusive, nor that either is
  active — it MUST behave correctly whether one, both, or neither is
  carrying traffic.
- **FR-018**: Recording or reporting a call, SMS, or health event MUST NOT be
  able to fail, delay, or degrade the call or message it describes.
- **FR-019**: When the process that collects and exposes metrics is
  unavailable, events MUST be held in a bounded buffer and delivered once it
  becomes reachable again, so that counts survive a routine restart of the
  collecting process.
- **FR-019a**: The buffer MUST have a fixed upper bound that cannot grow with
  outage duration. Once the bound is reached, the oldest events MUST be
  discarded, and the number of discarded events MUST itself be visible to the
  operator.
- **FR-019b**: Buffered events MUST NOT survive a restart of the agent
  holding them; buffering covers the collecting process being briefly
  unavailable, not durable delivery.
- **FR-020**: Counters MUST NOT reset or move backwards when a supervised
  agent process restarts.
- **FR-021**: Each agent MUST re-report its current state (active calls,
  registration state, tunnel state) on a fixed recurring interval,
  independently of whether anything changed.
- **FR-021a**: After the collecting process restarts, every point-in-time
  indicator MUST reflect the true current state within one report interval,
  without waiting for the next call, SMS, or registration event.
- **FR-021b**: An agent that stops reporting MUST be distinguishable by the
  operator from an agent reporting that nothing is happening.
- **FR-022**: All behaviour above MUST apply whether VoWiFi is enabled or
  disabled; with VoWiFi disabled, observability MUST be equivalent to today's
  behaviour.

### Key Entities

- **Call event**: A single inbound call's lifecycle — arrival, bridge
  outcome, answer, and end. Carries caller identity, transport, PBX
  destination, outcome category, and duration.
- **SMS event**: A single received message — sender, body, arrival time,
  transport, and forwarding outcome.
- **Registration state**: Whether the bridge currently holds a valid
  registration with the operator's IMS core, plus the outcome of each
  registration attempt.
- **Tunnel state**: Whether the ePDG tunnel currently carries traffic.
- **Transport**: The path a call or message arrived on — circuit-switched or
  VoWiFi. Applies as an attribute to call events, SMS events, and their
  persisted records.
- **Module identity**: The card a call or message is attributed to. Shared
  across transports for a given modem, since the SIM VoWiFi uses lives in
  that same modem.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On a VoWiFi-enabled deployment, an operator watching the
  dashboard sees an inbound call reflected in the call panels within 30
  seconds of the call ending, without editing any panel.
- **SC-002**: 100% of inbound VoWiFi calls that reach the bridge produce
  exactly one call count and exactly one persisted call record.
- **SC-003**: 100% of SMS received over VoWiFi produce exactly one received
  count, one forwarding-outcome count, and one persisted record.
- **SC-004**: An operator can determine, from the dashboard alone and without
  reading logs, whether the bridge is currently able to receive calls (tunnel
  up and registered) — verified by an operator completing that judgement in
  under 60 seconds.
- **SC-005**: For every inbound call that fails to bridge, the dashboard
  attributes the failure to a reason category; no failures land in an
  unattributed bucket.
- **SC-006**: With VoWiFi disabled, every existing metric reports identical
  values for the same traffic, and its series are unchanged apart from a
  constant transport dimension fixed to the circuit-switched value; every
  existing dashboard panel renders identically.
- **SC-007**: An agent process restart during a 24-hour observation window
  produces no backwards jump or reset in any counter on the dashboard.
- **SC-009**: After the collecting process is restarted, every health
  indicator on the dashboard shows the true current state within one report
  interval, with no manual intervention and without a call or SMS having to
  occur first.
- **SC-010**: An agent that has stopped is visibly distinguishable on the
  dashboard from an idle agent within one report interval.
- **SC-008**: Call setup time and audio quality are unchanged relative to the
  pre-change build under a burst of 10 calls and 10 SMS within one minute.

## Assumptions

- **Metric naming**: VoWiFi call and SMS activity reuses the existing metric
  names with an added transport attribute, rather than introducing parallel
  VoWiFi-specific metric names. This keeps existing dashboard panels working
  unmodified (they gain an extra series rather than needing a rewrite) and
  keeps combined totals a single query. New metrics are introduced only for
  the VoWiFi-specific health in FR-012 through FR-014, which have no
  circuit-switched equivalent.
- **Single scrape target**: The recommended shape is that the VoWiFi agents
  report their events to the process that already owns the metrics endpoint,
  keeping one scrape target. This is the assumed direction because one of the
  agents runs inside a network namespace and because multiple targets would
  force aggregation rewrites across every existing panel — but the final
  mechanism is a planning decision, and any mechanism satisfying FR-015
  through FR-021 is acceptable.
- **Module identity for VoWiFi**: The SIM that VoWiFi authenticates with
  physically lives in the same modem the circuit-switched path uses, so
  VoWiFi traffic is attributed to that same card identity (FR-011a) rather
  than to a separate synthetic one. Per-module panels therefore show a card's
  complete traffic, with the transport attribute separating the two paths.
  This also keeps the IMSI out of metric attributes and database rows
  (FR-011b).
- **Loss policy**: Observability events are buffered, not durable. A bounded
  in-memory buffer (FR-019, FR-019a, FR-019b) covers the routine case — the
  collecting process restarting for a few seconds while its supervisor brings
  it back — so calls and SMS completing during that window are still counted.
  Beyond the bound, the oldest events are discarded and the discard count is
  itself exposed. Rationale: the operator's need is trend visibility, not
  billing-grade accounting; an unbounded queue in a memory-constrained
  container is a worse failure than a gap in a graph, but dropping events
  during a five-second restart would lose exactly the calls that matter most.
  The persisted call/SMS history is written independently by the agent that
  owns the database connection, so history is unaffected by the collecting
  process being down.
- **Existing history schema is reused**: VoWiFi records go into the existing
  call and SMS tables. The transport field (FR-011) is an additive schema
  change applied in place on upgrade, with existing rows backfilled as
  circuit-switched (FR-011c) — accurate by construction, since the VoWiFi path
  has never written a record. Existing queries stay valid, and the field is
  populated on every row so consumers never have to special-case a missing
  value.
- **Scope**: Inbound calls and SMS only, matching what the VoWiFi bridge
  currently supports. Outbound VoWiFi calls are out of scope.
- **Transport independence**: The observability layer treats the two
  transports as independent sources that each may or may not be carrying
  traffic. In practice a given deployment carries inbound traffic on one at a
  time, but that is an operational observation, not a constraint this feature
  encodes or relies on.
- **Dashboard changes are additive**: Existing panels are left alone; the
  VoWiFi-specific health of User Story 4 is delivered as new panels.

## Dependencies

- The VoWiFi bridge as shipped (both agents, the tunnel, and the supervising
  entrypoint) — this feature instruments what exists rather than changing how
  calls are carried.
- The existing metrics endpoint, monitoring stack configuration, and
  provisioned dashboard.
- The existing call/SMS database and its read-only browser.

## Out of Scope

- Changing how VoWiFi calls or SMS are carried, bridged, or transcoded.
- Alerting rules or notification routing built on top of these metrics.
- Outbound call observability.
- Historical backfill of calls and SMS carried over VoWiFi before this
  feature ships — those are unrecoverable.
