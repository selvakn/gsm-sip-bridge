# Feature Specification: Dual-Stack IPv6 for the Cellular-Internet Sidecar

**Feature Branch**: `035-dual-stack-ipv6`
**Created**: 2026-08-13
**Status**: Draft
**Input**: User description: "Add dual-stack IPv6 support to the cellular-internet sidecar, which today dials IPv4-only. The operator needs a global IPv6 address on the WWAN interface because carrier IPv4 is CGNAT and unreachable inbound; IPv6 provides inbound reach-back to the host for SSH/management. Must stay dual-stack so VoWiFi and IPv4-only sites keep working. IPv4 remains the health-gating uplink; IPv6 is best-effort. Reach-back target is the host itself. Provide a hook script invoked with the new address when the IPv6 address changes."

## Context

The `cellular-internet` sidecar (feature 032) brings up internet over the modem's
QMI data path and keeps it up, self-healing on drops. Today it dials an
**IPv4-only** packet session and configures only an IPv4 address, gateway, route,
and DNS on the WWAN interface. The bridge that shares the same modem uses the AT
port for `AT+CSIM`; the sidecar therefore touches **only** the QMI control device
and never the AT port.

The operator runs this on a mobile/remote host whose only uplink is the cellular
card. The carrier's IPv4 is behind CGNAT, so the host has **no inbound
reachability over IPv4** — it cannot be reached from the public internet for SSH
or management. The carrier does hand out a globally-routable IPv6 prefix, so a
global IPv6 address on the WWAN interface would restore inbound reach-back.

## Clarifications

### Session 2026-08-13

- Q: When the global IPv6 address is LOST (v6 drops while IPv4 stays up), should
  the address-change hook be invoked to signal the loss? → A: No — the hook fires
  only when a global address appears or changes to a different value, never on
  loss. Consumers (e.g. a DDNS updater) are expected to expire stale records via a
  short TTL rather than relying on a withdrawal signal.
- Q: While IPv6 is unavailable but IPv4 is up, how often should the sidecar retry
  establishing IPv6 in the background? → A: A capped backoff — start at the probe
  interval and grow to a bounded maximum — so a v6-incapable carrier is not
  hammered with a start attempt every probe interval, while a transient drop still
  recovers promptly.
- Q: Should dual-stack be ON by default for every sidecar deployment, or opt-in?
  → A: ON by default — request dual-stack everywhere and degrade to IPv4-only when
  no IPv6 is granted. A documented kill-switch disables it for deployments that
  need byte-identical IPv4-only behavior.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Inbound reach-back to the host over IPv6 (Priority: P1)

The operator is away from the site. The host's only uplink is the cellular card,
and its IPv4 is CGNAT so it cannot be reached from outside. The operator needs to
SSH into (and otherwise manage) the host from the public internet.

**Why this priority**: This is the entire reason for the feature. Without a
globally-routable address reachable from outside, the operator cannot administer a
remote headless host at all. Everything else supports this outcome.

**Independent Test**: With the sidecar running and the carrier granting IPv6,
confirm the WWAN interface holds a global (non-link-local, non-ULA) IPv6 address
and a default IPv6 route, and that an external host on the IPv6 internet can open
an SSH session to that address. Fully testable on its own once IPv6 is dialed.

**Acceptance Scenarios**:

1. **Given** the carrier grants an IPv6 prefix, **When** the sidecar dials,
   **Then** the WWAN interface is assigned a global IPv6 address and a default
   IPv6 route is installed, and the address is recorded in the status file.
2. **Given** a global IPv6 address is up on the WWAN interface, **When** an
   external client connects to that address on the host's SSH port, **Then** the
   connection reaches the host (the sidecar does not firewall inbound IPv6 to the
   host).
3. **Given** IPv6 is up, **When** the operator inspects the sidecar status,
   **Then** the current global IPv6 address is visible alongside the existing
   IPv4 state.

---

### User Story 2 - Dual-stack: VoWiFi and IPv4-only destinations keep working (Priority: P1)

The host must continue to place/receive VoWiFi calls and reach IPv4-only internet
destinations exactly as before. Adding IPv6 must not disturb the existing IPv4
data path or the modem's AT port.

**Why this priority**: A regression here breaks the primary product (the bridge /
VoWiFi). IPv6 is additive; it must never come at the cost of the working IPv4
path. Equal top priority with Story 1 because shipping Story 1 while breaking this
is a net loss.

**Independent Test**: With the sidecar running, confirm the IPv4 address, default
route, and DNS are configured as before and that IPv4-only destinations resolve
and are reachable through the cellular link, and that the bridge/VoWiFi behaves
identically to a build without this feature. Testable independently of whether
IPv6 is present.

**Acceptance Scenarios**:

1. **Given** the sidecar dials, **When** both address families are granted,
   **Then** the interface holds both an IPv4 and a global IPv6 address, an IPv4
   default route and an IPv6 default route, and both families route.
2. **Given** IPv4 is up but the carrier grants no IPv6, **When** the sidecar runs,
   **Then** IPv4 internet works unchanged and the sidecar reports healthy.
3. **Given** the sidecar is running, **When** the bridge uses the modem's AT port
   for `AT+CSIM`, **Then** IPv6 setup never contends for or opens the AT port.

---

### User Story 3 - IPv6 is best-effort and never blocks the bridge (Priority: P1)

IPv4 is the health-gating uplink. If IPv6 cannot be brought up, or drops while
IPv4 stays up, the container must remain healthy and the bridge must not be
blocked from starting or running. IPv6 is retried in the background.

**Why this priority**: The operator explicitly requires that a v6 problem never
disturbs VoWiFi. The container healthcheck gates bridge startup (feature 032), so
coupling health to IPv6 would let a carrier v6 outage take down calling. Equal top
priority because it is a hard safety constraint on the other two stories.

**Independent Test**: Simulate a carrier that grants IPv4 but refuses or later
drops IPv6; confirm the container healthcheck stays healthy the whole time, the
bridge is never blocked, and the sidecar keeps retrying IPv6 in the background
without disturbing the IPv4 session.

**Acceptance Scenarios**:

1. **Given** IPv4 is up and IPv6 fails to come up, **When** the healthcheck runs,
   **Then** it reports healthy (health follows IPv4 reachability only).
2. **Given** IPv6 was up and then drops while IPv4 stays up, **When** this is
   detected, **Then** the sidecar re-establishes IPv6 in the background without
   tearing down or interrupting the IPv4 session.
3. **Given** IPv6 cannot be established, **When** the sidecar continues running,
   **Then** it periodically retries IPv6 and logs the attempts, and status
   reflects "IPv4 up, IPv6 unavailable" distinctly.

---

### User Story 4 - Address-change hook for external reachability tooling (Priority: P2)

The carrier's IPv6 prefix typically changes on each redial/reattach, so the
reachable address is not stable. The operator wants the sidecar to notify their
own tooling (e.g. a dynamic-DNS updater) whenever the global IPv6 address changes,
by running a configurable script with the new address as an argument.

**Why this priority**: Reach-back (Story 1) is only usable in practice if the
current address is discoverable from outside; a changing address otherwise means
the operator can't find the host. It's P2 because the address is still recorded in
status/logs without the hook — the hook automates discovery rather than enabling
it.

**Independent Test**: Configure a hook script that records its argument; force an
IPv6 address change (redial with a new prefix); confirm the hook is invoked
exactly once per change with the new global address as its argument, and is not
invoked when the address is unchanged.

**Acceptance Scenarios**:

1. **Given** a hook script is configured, **When** the global IPv6 address is
   first assigned, **Then** the hook runs once with that address as an argument.
2. **Given** a hook script is configured and an address is already active, **When**
   a redial assigns a different global IPv6 address, **Then** the hook runs again
   with the new address.
3. **Given** a hook script is configured, **When** a redial yields the same global
   IPv6 address as before, **Then** the hook is not invoked.
4. **Given** no hook script is configured, **When** the address changes, **Then**
   the sidecar behaves exactly as today (no hook, just status/log updates).
5. **Given** a configured hook script fails or is slow, **When** it is invoked,
   **Then** the failure/slowness does not disrupt the IPv4 or IPv6 sessions or the
   healthcheck (the hook is isolated from the supervise loop).

---

### Edge Cases

- **Carrier grants no IPv6 at all**: sidecar stays IPv4-healthy, retries IPv6
  periodically in the background, reports IPv6 unavailable. (Story 3)
- **Carrier grants IPv6 but firewalls inbound**: out of the sidecar's control; the
  sidecar still brings up the address/route and does not itself block inbound. The
  spec does not promise the carrier permits inbound — only that the sidecar does
  not.
- **IPv6-only grant (no IPv4)**: IPv4 is the health-gating uplink; if the carrier
  grants IPv6 but not IPv4, the container is treated as unhealthy exactly as a
  no-IPv4 grant is today. IPv6-only operation is out of scope for this feature.
- **Prefix/address changes on redial**: existing address is replaced with the new
  one, and the change hook fires (if configured).
- **Modem re-enumeration / QMI endpoint wedged**: the existing IPv4 self-heal
  (redial, proxy recycle) governs recovery; IPv6 is re-established as part of the
  same redial, never on its own separate teardown of a healthy IPv4 session.
- **Hook script missing/not executable**: logged as a warning; does not affect
  connectivity.
- **IPv6 lost while IPv4 stays up**: status flips to `ipv6_state=unavailable` and
  the loss is logged, but the change hook is NOT fired (it fires only on
  appear/change). Background retry resumes under the capped backoff; consumers of
  the old address rely on TTL expiry.
- **Persistently v6-incapable carrier**: background retry backs off to the capped
  maximum interval so the modem/logs are not hammered; the container stays
  IPv4-healthy indefinitely.
- **Stale IPv6 default route after a redial that yields no IPv6**: any previous
  IPv6 address/route for this interface is cleaned up so no black-hole v6 route
  lingers.
- **Link-local / ULA only**: a non-global address does not count as reach-back;
  status reflects "no global IPv6" and the hook does not fire for it.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The sidecar MUST request a dual-stack (IPv4 + IPv6) cellular data
  session from the modem over the QMI control device, without ever opening the
  modem's AT port.
- **FR-002**: When the carrier grants IPv6, the sidecar MUST configure the WWAN
  interface with the granted global IPv6 address (and prefix) and install a
  default IPv6 route so the host can reach and be reached over the IPv6 internet.
- **FR-003**: The sidecar MUST continue to configure the IPv4 address, default
  route, and DNS on the WWAN interface exactly as today, so VoWiFi and IPv4-only
  destinations keep working unchanged.
- **FR-004**: The container healthcheck MUST gate solely on IPv4 reachability
  through the cellular link. A missing, failed, or dropped IPv6 session MUST NOT
  make the container unhealthy and MUST NOT block the bridge from starting or
  running.
- **FR-005**: The sidecar MUST treat IPv6 as best-effort: when IPv6 is
  unavailable, it MUST keep IPv4 up, retry IPv6 establishment in the background
  using a **capped backoff** (first retry no sooner than the probe interval,
  growing to a bounded maximum interval), and log the attempts, without tearing
  down or interrupting the IPv4 session. A transient drop MUST recover promptly
  (the backoff resets once IPv6 is up), while a persistently v6-incapable carrier
  MUST NOT be retried more often than the capped maximum.
- **FR-006**: The sidecar MUST NOT install any firewall rule that blocks inbound
  IPv6 traffic to the host; a granted global IPv6 address MUST be usable for
  inbound reach-back (e.g. SSH) to the host.
- **FR-007**: The sidecar MUST record the current global IPv6 address (or its
  absence) in the human-readable status file, distinct from the existing IPv4
  state, and MUST log IPv6 up/down transitions.
- **FR-008**: The sidecar MUST support an optional operator-supplied hook: when
  the global IPv6 address first appears or changes to a different value, it MUST
  invoke the configured hook exactly once with the new global IPv6 address as an
  argument. It MUST NOT invoke the hook when the address is unchanged, MUST NOT
  invoke the hook when the address is lost/withdrawn (loss is reflected only in
  status/logs; consumers expire stale records via TTL), and MUST do nothing
  (beyond status/log) when no hook is configured.
- **FR-009**: The hook invocation MUST be isolated from the connectivity
  supervise loop: a hook that fails, hangs, or is slow MUST NOT disrupt the IPv4
  or IPv6 sessions, delay redials, or affect the healthcheck.
- **FR-010**: On redial or teardown, the sidecar MUST clean up any prior IPv6
  address and default route it added for the interface, so no stale/black-hole
  IPv6 route survives a redial that yields a different address or no IPv6.
- **FR-011**: Dual-stack MUST be **enabled by default**, degrading safely to
  today's IPv4-only behavior on carriers/modems that do not grant IPv6, with no
  operator action required. A documented kill-switch MUST let an operator force
  byte-identical IPv4-only behavior (no IPv6 dialing, no `ip -6` changes, empty
  v6 status fields) for deployments that require it.
- **FR-012**: A non-global IPv6 address (link-local or ULA only) MUST NOT be
  reported as reach-back and MUST NOT trigger the change hook.

### Key Entities *(include if feature involves data)*

- **IPv6 session state**: whether a dual-stack/IPv6 session is currently up, the
  current global IPv6 address and prefix length, and the timestamp of the last
  IPv6 up/down transition. Distinct from the existing IPv4 state.
- **Sidecar status record**: the existing human-readable status file, extended
  with the IPv6 address and an IPv6 up/unavailable indicator alongside the current
  IPv4 fields.
- **Address-change hook**: an optional operator-configured executable path plus
  the "last address the hook was notified about", used to fire the hook only on
  genuine changes.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On a carrier/modem that grants IPv6, an operator can SSH into the
  remote host from the public IPv6 internet using the address the sidecar reports,
  with no manual network configuration on the host beyond starting the sidecar.
- **SC-002**: With both families granted, IPv4-only internet destinations and
  VoWiFi calling behave identically to a build without this feature (no measurable
  regression in call setup or IPv4 reachability).
- **SC-003**: When the carrier grants IPv4 but no IPv6 (or IPv6 later drops while
  IPv4 stays up), the container remains healthy 100% of the time and the bridge is
  never blocked, while the sidecar keeps retrying IPv6 in the background.
- **SC-004**: When the global IPv6 address changes, the configured hook is invoked
  exactly once per distinct address with that address as its argument, and is not
  invoked for unchanged addresses.
- **SC-005**: The current global IPv6 address (or a clear "unavailable" state) is
  observable in the sidecar status within one probe interval of a change.
- **SC-006**: Deployments on IPv6-incapable carriers/modems continue to run with
  no operator action and no new failures (default-safe degradation), and the
  background IPv6 retry backs off to no more frequent than the capped maximum
  interval rather than attempting a start every probe interval.
- **SC-007**: An operator can force byte-identical IPv4-only behavior via a single
  documented kill-switch (no IPv6 dialing, no `ip -6` changes, empty v6 status
  fields).

## Assumptions

- The carrier assigns the WWAN interface a globally-routable IPv6 address via the
  cellular data session, and that address is reachable inbound from the public
  IPv6 internet (any carrier-side inbound filtering is outside the sidecar's
  control and out of scope).
- The modem is a Quectel EC20/EC25-class device that supports an IPv6 (or
  dual-stack) packet data session over QMI. IPv6 support on other modem classes is
  out of scope, consistent with the existing QMI-only scope of the sidecar.
- The container runs with host networking and the privileges it already has today,
  so a global IPv6 address on the WWAN interface makes the host itself directly
  reachable (the reach-back target is the host, not a forwarded service).
- Address stability is not guaranteed by the carrier; discovery of the current
  address by external parties is handled by operator tooling driven from the
  change hook and/or the status file, not by the sidecar itself (no built-in DDNS
  client, no DNS credentials in the sidecar).
- The existing IPv4 dial/teardown/self-heal lifecycle (feature 032) remains the
  backbone; IPv6 is layered onto the same lifecycle rather than run as an
  independent second supervisor.
- IPv6-only operation (IPv6 granted, IPv4 absent) is out of scope; IPv4 remains
  the required, health-gating uplink.
