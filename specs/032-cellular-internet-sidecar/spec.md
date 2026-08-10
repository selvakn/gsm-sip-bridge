# Feature Specification: Cellular-internet sidecar container

**Feature Branch**: `032-cellular-internet-sidecar`
**Created**: 2026-08-10
**Status**: Draft
**Input**: User description: "Bundle the quectel-CM / qmicli or libqmi (all the tools needed for bringing internet) as a separate docker image, that can be managed independently. In situations where internet is powered by the same card, the gsm bridge container will wait for this container to be ready (internet to be available) before it will be started."

## Overview

Some deployments have no wired/Wi-Fi uplink and must obtain their internet
connectivity from the **same cellular card** that also carries calls. Because
the bridge handles calls over VoWiFi — which uses the modem only as a SIM/APDU
reader and rides the host's ordinary default route for its ePDG tunnel — one
card can serve both internet and calls at once. Today, bringing that internet
up is an undocumented, manual host step, and nothing coordinates the ordering
between "internet is up" and "the bridge starts trying to reach its PBX / ePDG".

This feature packages the cellular-internet dialer as a **separate, independently
managed container** (a "sidecar"). When a deployment opts into same-card
internet, the bridge container does not start until the sidecar reports that
internet is genuinely reachable. Deployments that already have an uplink are
unaffected and never run the sidecar.

## Clarifications

### Session 2026-08-10

- Q: How should the bridge wait for internet to be ready? → A: Docker Compose
  healthcheck on the sidecar + `depends_on: condition: service_healthy` on the
  bridge.
- Q: What counts as "internet is ready"? → A: An actual reachability probe
  through the cellular link (not merely link-up / IP-assigned).
- Q: How is the sidecar enabled in deployments? → A: Opt-in Compose profile,
  **off by default**; deployments with their own uplink never run it.
- Q: Which modems must the sidecar support? → A: QMI-capable modems only
  (EC20 / Qualcomm), driven over QMI so the modem's AT port stays free for the
  bridge's `AT+CSIM`. Non-QMI (EC200U/UNISOC) is out of scope.
- Q: How far should observability go — integrate internet-down into the bridge's
  Discord alerts / Prometheus, or sidecar-local only? → A: Sidecar-local only
  (status query + logs); no Discord/Prometheus integration. Rationale: when
  internet is down a Discord alert cannot be delivered anyway, and the sidecar
  is deliberately kept lightweight.
- Q: Startup-gate timing when the sidecar isn't yet healthy? → A: Bridge waits
  **indefinitely** for a genuinely-reachable uplink (never half-starts); the
  sidecar healthcheck allows a **90s first-connect grace period** for cellular
  attach and probes every **10s** thereafter.
- Q: What reachability-probe method defines "internet is ready"? → A: A **DNS
  resolution** of a stable hostname via a public resolver, with the probe target
  **operator-configurable**. Chosen because it survives carriers/APNs that block
  ICMP and proves routing *and* name resolution (which the PBX/ePDG hostnames
  need).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Same-card internet comes up and gates the bridge (Priority: P1)

An operator deploying to a site with no wired/Wi-Fi uplink enables the internet
sidecar. On `docker compose up`, the sidecar dials the cellular data connection
and only reports healthy once traffic actually reaches the public internet. The
bridge container waits for that healthy signal, then starts — so it never
begins registering to its PBX or building its ePDG tunnel before a working
uplink exists.

**Why this priority**: This is the core value — without ordered startup the
bridge races an absent uplink, produces misleading registration/tunnel failures,
and relies on retry loops to eventually recover. It is the minimum viable slice.

**Independent Test**: On a host whose only uplink is the cellular card, bring
the stack up and confirm the bridge process does not launch until the sidecar is
healthy, and that once it launches, PBX registration / ePDG establishment
succeed on the first attempt.

**Acceptance Scenarios**:

1. **Given** a host with no other uplink and the sidecar profile enabled,
   **When** the stack is started, **Then** the sidecar becomes healthy only
   after a reachability probe through the cellular link succeeds, and the bridge
   container starts only after the sidecar is healthy.
2. **Given** the cellular link cannot be established (no signal / SIM not
   provisioned for data), **When** the stack is started, **Then** the sidecar
   stays unhealthy and the bridge container does not start, and the operator can
   see from the sidecar's status/logs that internet was never reached.
3. **Given** the bridge and sidecar are both running healthy, **When** an
   operator inspects the running stack, **Then** internet traffic and VoWiFi
   calls are both served over the one card simultaneously.

---

### User Story 2 - The sidecar is independently managed (Priority: P2)

An operator restarts, updates, or inspects the internet sidecar without touching
the bridge, and vice versa. The sidecar owns the cellular data lifecycle
(dial, monitor, self-heal on drop) as its own unit with its own logs and status.

**Why this priority**: Decoupling is the whole point of a separate image —
internet provisioning evolves and fails independently of telephony, and an
operator must be able to reason about and recover each separately. Valuable, but
only meaningful once Story 1 exists.

**Independent Test**: Restart only the sidecar container and confirm the
cellular data connection is re-established without restarting or reconfiguring
the bridge; view sidecar-only logs describing dial/connect/probe state.

**Acceptance Scenarios**:

1. **Given** a running healthy stack, **When** the operator restarts only the
   sidecar, **Then** the sidecar re-dials and returns to healthy on its own, and
   the bridge is not required to restart to regain internet.
2. **Given** the cellular data connection drops while running, **When** the drop
   occurs, **Then** the sidecar detects it, its health reflects the loss, and it
   attempts to restore the connection without operator intervention.
3. **Given** an operator wants to diagnose connectivity, **When** they query the
   sidecar, **Then** they can see whether the link is up, whether the internet
   probe passes, and recent connect/disconnect events — from the sidecar alone.

---

### User Story 3 - Deployments without same-card internet are unaffected (Priority: P3)

An operator with an existing wired/Wi-Fi uplink deploys or upgrades the stack
and the sidecar plays no part: it is not started, imposes no startup dependency,
and does not touch the modem's data path.

**Why this priority**: Protects the large majority of existing deployments from
regressions. Important as a guarantee, but it is the "do nothing" path.

**Independent Test**: With the sidecar profile disabled, bring the stack up and
confirm the sidecar container is absent, the bridge starts with no added wait,
and behavior is byte-for-byte today's.

**Acceptance Scenarios**:

1. **Given** the sidecar profile is not enabled, **When** the stack is started,
   **Then** no sidecar container is created and the bridge starts with no
   internet-readiness dependency.
2. **Given** an existing deployment upgrades to the version that adds this
   feature, **When** they do not opt in, **Then** their startup ordering and
   behavior are unchanged.

---

### Edge Cases

- **Sidecar never becomes healthy**: the bridge must remain un-started (not
  crash-loop, not start half-configured); the unhealthy sidecar and the reason
  (no signal / no data provisioning / probe failing) must be visible to the
  operator.
- **Internet drops after the bridge already started**: the startup gate does not
  retroactively stop the bridge; the sidecar self-heals the link and existing
  bridge retry/recovery behavior handles the transient loss. (In-flight calls
  may drop — the same as any uplink loss.)
- **AT-port contention**: the sidecar must not hold the modem's AT command port,
  because the bridge needs it for `AT+CSIM`. Driving data over QMI keeps the AT
  port free.
- **Modem is not QMI-capable**: on an unsupported (non-QMI) modem the sidecar
  must fail clearly at startup rather than silently degrade or fight the bridge
  for the AT port.
- **Reachability probe endpoint is itself down**: the probe must not produce a
  false "internet down" when only a single chosen target is unreachable
  (use a resilient probe target/strategy).
- **Modem reboots / re-enumerates**: the sidecar must re-establish data once the
  device reappears, returning to healthy without manual steps.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST provide the cellular-internet dialer as a separate,
  independently deployable container image, distinct from the bridge image.
- **FR-002**: The sidecar MUST bring up cellular data over the modem's QMI
  interface (leaving the modem's AT command port free for other consumers).
- **FR-003**: The sidecar MUST expose a health state that reports healthy only
  when an **actual internet-reachability probe** through the cellular link
  succeeds — not merely when the interface is up or has an IP. The probe MUST be
  a **DNS resolution** of a stable hostname via a public resolver (so it survives
  carrier/APN ICMP blocking and proves both routing and name resolution), and the
  probe target MUST be operator-configurable.
- **FR-004**: When the same-card-internet deployment mode is enabled, the bridge
  container MUST NOT start until the sidecar reports healthy, and MUST wait
  **indefinitely** for that healthy signal (it never starts against an absent
  uplink). The sidecar's health MUST tolerate a slow first cellular attach via a
  first-connect grace period of ~90s, and MUST re-evaluate reachability on a
  recurring interval of ~10s thereafter.
- **FR-005**: The same-card-internet mode MUST be **opt-in and disabled by
  default**; when it is not enabled, no sidecar container runs and the bridge
  starts with no added startup dependency (existing behavior preserved).
- **FR-006**: The sidecar MUST manage the cellular data connection lifecycle
  independently of the bridge — it MUST be startable, restartable, and
  inspectable without restarting the bridge, and the bridge MUST likewise be
  manageable without restarting the sidecar.
- **FR-007**: The sidecar MUST detect a dropped cellular data connection and
  attempt to restore it automatically, reflecting the current state in its
  health.
- **FR-008**: The sidecar MUST make its connectivity status observable to an
  operator (link up/down, probe pass/fail, recent connect/disconnect events)
  from the sidecar alone. This observability MUST be **sidecar-local** (a
  queryable status and container logs); the sidecar MUST NOT depend on the
  bridge's Prometheus metrics or Discord alerting — an internet-down alert could
  not be delivered over the very link that is down, and the sidecar is kept
  intentionally lightweight.
- **FR-009**: Once the sidecar is healthy, the resulting internet path MUST be
  usable by the bridge and other host consumers concurrently with VoWiFi calls
  on the same card (internet and calls decoupled, no mutual interference).
- **FR-010**: On a modem that does not support the required QMI data path, the
  sidecar MUST fail with a clear, actionable error at startup rather than
  degrade silently or contend for the AT port.
- **FR-011**: The internet-provisioning configuration (e.g. which APN/data
  profile to dial, which modem device to use, the reachability-probe target)
  MUST be operator-configurable on the sidecar, separate from the bridge's
  configuration.
- **FR-012**: The feature MUST be documented as a deployment runbook so an
  operator can enable same-card internet, verify readiness gating, and confirm
  internet + calls run together on one card.

### Key Entities

- **Internet sidecar**: the independently managed unit responsible for
  establishing and maintaining cellular data connectivity and for reporting
  whether the internet is actually reachable. Owns the modem's data path; does
  not own the AT command path.
- **Internet-readiness signal**: the health state derived from a live
  reachability probe that the bridge's startup depends on.
- **Same-card-internet deployment mode**: the opt-in configuration that both
  activates the sidecar and makes the bridge's startup depend on the readiness
  signal. Off by default.
- **Bridge container**: the existing telephony unit; in same-card mode it is a
  dependent consumer of the readiness signal, otherwise unchanged.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On a host whose only uplink is the cellular card, with the mode
  enabled, the bridge process does not begin startup until internet is actually
  reachable — in 100% of cold starts.
- **SC-002**: When internet cannot be established, the bridge does not start and
  an operator can determine the reason from the sidecar's status/logs alone,
  without inspecting the bridge.
- **SC-003**: With the mode disabled, startup ordering and behavior are
  identical to the prior release (no added wait, no sidecar container) — verified
  against an existing deployment.
- **SC-004**: An operator can restart the internet sidecar and have connectivity
  fully restored without restarting the bridge, and vice versa.
- **SC-005**: With both running, a reachability probe to the public internet and
  a live VoWiFi call both succeed on the same card at the same time.
- **SC-006**: After an unattended cellular data drop, connectivity is restored by
  the sidecar without operator action.

## Assumptions

- Calls are carried over **VoWiFi** in these deployments — the path that shares a
  card with internet. VoLTE (which seizes the modem's single data path) is out of
  scope here; see the mutual-exclusion note below.
- The target modem is **QMI-capable (EC20 / Qualcomm)**. Non-QMI modems
  (EC200U / UNISOC) are explicitly out of scope for the sidecar.
- The deployment/orchestration substrate is **Docker Compose** (the project's
  existing deployment model), which provides the healthcheck + `depends_on`
  ordering primitive this feature relies on.
- Internet provisioning is intentionally **not** absorbed into the bridge; it
  remains a separate concern owned by the sidecar. The bridge continues to treat
  its uplink as ambient host connectivity.
- The reachability probe is a DNS resolution of a stable hostname against a
  public resolver (operator-configurable target), chosen so it survives ICMP
  blocking and proves both routing and name resolution.
- Bundling the internet tooling in a **separate** image means the bridge image's
  size is unchanged; the tooling ships only in the sidecar image, run only by
  opted-in deployments.

## Dependencies

- Requires a SIM provisioned for cellular **data** (an internet APN), in addition
  to whatever provisioning the calling path needs.
- Requires the host to grant the sidecar access to the modem's QMI device while
  leaving the AT command path available to the bridge.
- Same-card internet + calls is only coherent with the VoWiFi calling path
  (VoWiFi and VoLTE remain mutually exclusive on one SIM).

## Out of Scope

- VoLTE + internet on one card (the bridge's VoLTE path seizes the modem's single
  host data path; simultaneous internet would require multi-PDN muxing not in
  this project).
- Non-QMI (AT-only / UNISOC) modem support in the sidecar.
- Making the bridge itself dial or manage the internet connection.
- Orchestration substrates other than Docker Compose.
- Integrating internet-down into the bridge's Discord alerting or Prometheus
  metrics (sidecar observability is local-only, see FR-008).
