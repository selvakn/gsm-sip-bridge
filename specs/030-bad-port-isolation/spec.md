# Feature Specification: Isolate a hanging serial port from wedging discovery

**Feature Branch**: `030-bad-port-isolation`  
**Created**: 2026-08-08  
**Status**: Draft  
**Input**: User description: "Isolate a hanging serial port from wedging discover's startup scan. Source: docs/plans/ec20-bad-port-isolation.md"

## Clarifications

### Session 2026-08-08

- Q: On ongoing rescans, what should happen to a port that keeps timing out (so the daemon does not accumulate one leaked blocked resource per rescan on a wedged port)? → A: Quarantine a port in-memory after 3 consecutive probe timeouts; later rescans never re-probe it for the remainder of the process lifetime (cleared on restart).
- Q: Where does the ~3s abandon timeout wrap, given a modem probe opens several candidate ports then re-opens for SIM status? → A: Bound each individual port open/probe operation independently (per-port, not per-modem or per-scan).
- Q: How does a USB-topology exclusion entry match a port? → A: Exact-equality OR leading path-prefix (a coarser fragment excludes every interface under that device); exact device paths match by exact equality. Additionally, the timeout/abandon log MUST include the port's USB interface (topology) path so an operator can copy it straight into the exclusion config.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Startup and rescans survive a wedged serial port (Priority: P1)

A specific modem unit exposes a serial interface where any operation (even a
bare port open) hangs the operating system's serial driver forever —
uninterruptible from user space. Today, when the discovery scan reaches that
port, the entire daemon startup wedges: no modem is served, including healthy
ones on other units. This has already taken down a deployed service on an
unrelated restart while such a unit was attached.

With this feature, discovery bounds how long it will wait on any single port.
If a port does not respond within the allowed time, the scan abandons that
port, records why, and continues probing the remaining ports and modems. The
daemon comes up and serves every healthy modem regardless of one misbehaving
port — and it protects against *any* future misbehaving port, not just the one
unit already diagnosed.

**Why this priority**: This is the load-bearing fix. It is the only change that
closes the "whole daemon wedges" failure, requires no configuration to be
effective, and guards against ports never seen before. Without it the daemon
remains one bad port away from total startup failure.

**Independent Test**: Attach (or simulate) a port that never returns from
open/read alongside at least one healthy modem. Confirm discovery completes
within a bounded time and reports the healthy modem normally, while the
unresponsive port is logged as abandoned and excluded from results.

**Acceptance Scenarios**:

1. **Given** a healthy modem and a port that never responds are both present,
   **When** the daemon starts discovery, **Then** discovery completes within a
   bounded time, the healthy modem is reported usable, and the unresponsive
   port is logged as abandoned and reported as unresolved.
2. **Given** an ongoing-rescan cycle during normal operation, **When** a port
   stops responding mid-life, **Then** the rescan still completes and the other
   modems remain served.
3. **Given** a port that is slow but healthy (responds just under the allowed
   time), **When** discovery probes it, **Then** it is resolved normally and
   **not** falsely abandoned.

---

### User Story 2 - Operator excludes a known-bad port from probing (Priority: P2)

An operator has already diagnosed a specific port on a specific unit as the
one that hangs. They want that port to never be opened or probed again —
without resorting to a host-level driver unbind that does not survive
unplug/replug or reboot.

With this feature, the operator lists the port in the daemon's own
configuration. Discovery skips it entirely: it is never opened, never probed,
and never reported as a usable modem. Because the entry can be written as a
stable USB-topology position (not just a device path that renumbers), the
exclusion survives replug and reboot and lives inside the container's config
rather than in host infrastructure.

**Why this priority**: This is the operator escape hatch for a
known-bad unit, and it is what prevents the daemon from repeatedly paying to
probe — and repeatedly leaking an unreclaimable, kernel-blocked resource on —
a port already known to be bad on every lifetime rescan. It is not sufficient
alone (it only protects units already diagnosed and configured), which is why
Story 1 is the higher priority.

**Independent Test**: Add a port to the exclusion configuration, run discovery,
and confirm the port is never opened (no probe attempt logged for it) and does
not appear in results, while all other ports are probed normally.

**Acceptance Scenarios**:

1. **Given** a port listed in the exclusion configuration by exact device path,
   **When** discovery runs, **Then** that port is never opened or probed and is
   absent from reported modems.
2. **Given** a port listed by USB-topology fragment, **When** the unit is
   replugged and its device path renumbers, **Then** the same topology entry
   still matches and the port stays excluded.
3. **Given** an empty or absent exclusion configuration, **When** discovery
   runs, **Then** behavior is identical to today — nothing is skipped.
4. **Given** an exclusion entry that matches no currently-attached port,
   **When** discovery runs, **Then** it is harmless — no error, all present
   ports probed normally.

---

### User Story 3 - Operator can see which ports were abandoned or skipped and why (Priority: P3)

When a scan does not resolve an expected modem, the operator needs to tell
apart "a port timed out and was abandoned" from "a port was deliberately
skipped by configuration" from "a modem answered but its SIM was unusable."

With this feature, the log output names the specific port and the reason in
each case, so an operator can diagnose a missing line and decide whether to add
a new entry to the exclusion configuration.

**Why this priority**: Diagnosability makes the first two stories operable, but
the daemon is already protected without it; it is a usability layer on top.

**Independent Test**: Trigger each outcome (abandoned-by-timeout,
skipped-by-config, resolved) and confirm the logs distinguish them by port and
reason.

**Acceptance Scenarios**:

1. **Given** a port is abandoned on timeout, **When** the operator reads the
   logs, **Then** they find a warning naming the port and stating it was
   abandoned after the timeout.
2. **Given** a port is skipped by configuration, **When** the operator reads
   the logs, **Then** they find a message naming the port and stating it was
   skipped by the exclusion list.

### Edge Cases

- **Multiple bad ports at once**: each is independently bounded and abandoned;
  total added scan time is proportional to the number of bad ports, and the
  scan still completes.
- **The bad port is the only AT-capable interface on its modem**: that modem is
  reported unresolved (no usable line), while all other modems are unaffected.
- **Exact-path exclusion goes stale after replug**: a device-path entry may
  start pointing at a different port after renumbering; the topology-fragment
  form avoids this and is the recommended way to pin a known-bad port.
- **An abandoned probe's underlying wait cannot be reclaimed**: the operating
  system may hold that resource blocked for the process's lifetime; the feature
  contains the cost (the scan proceeds) but does not claim to free it. In-memory
  quarantine after 3 consecutive timeouts (FR-013) caps this at a small bounded
  number of blocked resources per bad port per process lifetime, and the
  exclusion list is the persistent way to stop creating them at all on a
  known-bad port.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Discovery MUST complete even when one or more serial ports never
  return from an open or read operation, including uninterruptible
  driver-level hangs that a user-space read timeout cannot break.
- **FR-002**: Each individual port open/probe operation MUST be bounded by a
  maximum wait, applied independently per port (not once per modem or once per
  scan). On expiry, discovery MUST abandon that port and continue with the
  remaining ports. A modem exposing several ports therefore has each of its
  ports bounded on its own, and the 3-consecutive-timeout quarantine (FR-013)
  is tracked per port.
- **FR-003**: Abandoning one port MUST NOT delay or block the probing of any
  other port or modem.
- **FR-004**: The bounded wait MUST be long enough that a slow-but-healthy port
  is not falsely abandoned, and short enough that a full scan with one bad port
  does not take unreasonably long (default target: approximately 5 seconds per
  port — the SIM-status probe bounds an open plus `AT+CPIN?` and `AT+CIMI`, each
  of which can block up to the per-line read timeout). A configured value below
  a safe floor MUST be clamped up (not honored), since too low a value would
  abandon every port and quarantine every modem.
- **FR-005**: The system MUST provide operator-controlled configuration listing
  ports to exclude from probing entirely.
- **FR-006**: The exclusion configuration MUST accept both exact device paths
  (e.g. `/dev/ttyUSB1`) and USB-topology fragments (e.g. `5-1.2.1.2:1.1`) that
  remain stable across replug and reboot. Matching MUST be: exact device paths
  match a port's device path by exact equality; a topology fragment matches a
  port when it equals OR is a leading path-prefix of that port's USB interface
  path — so a full interface fragment (`5-1.2.1.2:1.1`) excludes exactly that
  interface, while a coarser device fragment (`5-1.2.1.2`) excludes every
  interface under that device. Substring matching that is not anchored at the
  start of the path MUST NOT be used.
- **FR-007**: An excluded port MUST NOT be opened or probed during startup
  discovery or during ongoing lifetime rescans.
- **FR-008**: An empty or absent exclusion configuration MUST preserve today's
  exact discovery behavior.
- **FR-009**: The system MUST NOT silently default to excluding any interface
  by number or type (for example, a GNSS/NMEA-typical interface). Every
  exclusion MUST be explicit operator configuration.
- **FR-010**: Both the bounded-wait behavior and the exclusion list MUST apply
  to startup discovery and to ongoing lifetime rescans alike.
- **FR-011**: An abandoned or excluded port MUST NOT be reported as a usable
  modem or line.
- **FR-012**: Log output MUST let an operator distinguish, by port, a port
  abandoned on timeout from a port skipped by the exclusion list, each with its
  reason. The abandon-on-timeout log MUST include the port's USB interface
  (topology) path — not only its device path — so the operator can copy that
  value directly into an exclusion entry (FR-006).
- **FR-013**: A port that times out on 3 consecutive probe attempts MUST be
  quarantined in memory and MUST NOT be re-probed by later rescans for the
  remainder of the process lifetime. The quarantine is cleared on process
  restart and is NOT persisted to configuration (persistent exclusions remain
  explicit operator configuration per FR-005 and FR-009).

### Key Entities *(include if feature involves data)*

- **Port exclusion entry**: an operator-provided matcher for a serial port to
  skip, expressed either as an exact device path or as a stable USB-topology
  fragment.
- **Probe outcome**: the per-port result of discovery — resolved (usable),
  unresolved-abandoned (timed out), or skipped (matched the exclusion list).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With a port that never responds attached alongside at least one
  healthy modem, discovery completes and reports every healthy modem, instead
  of never completing (today's outcome).
- **SC-002**: When one unresponsive port is present, the total added discovery
  time attributable to it is bounded (on the order of the configured per-port
  wait), not unbounded.
- **SC-003**: With a known-bad port listed in the exclusion configuration,
  discovery never opens or probes that port — zero probe attempts against it —
  across both startup and repeated rescans.
- **SC-004**: An operator can exclude a specific port using configuration that
  survives unplug/replug and reboot, without editing host-level driver/unbind
  rules.
- **SC-005**: With an empty or absent exclusion configuration, discovery
  results are identical to the pre-feature behavior for a fleet with no bad
  ports.

## Assumptions

- The default per-port bounded wait is approximately 5 seconds. It was raised
  from an initial 3s because the SIM-status probe bounds an open plus two AT
  reads (`AT+CPIN?`, `AT+CIMI`), whose combined worst case on a slow-but-healthy
  modem approaches that budget; too tight a value would falsely abandon a
  working modem. The value is configurable, with a floor below which it is
  clamped up. A false timeout on the SIM-read phase (as opposed to the initial
  AT open) does NOT count toward quarantine, so transient SIM slowness cannot
  blackhole a healthy modem.
- The exclusion list accepts both exact device paths and USB-topology
  fragments; the topology form is the recommended way to pin a known-bad port
  because it survives replug/reboot.
- A probe abandoned because of a driver-level hang leaves an underlying wait
  that the operating system may never release for the process's lifetime. This
  is an accepted, contained cost (already true today, only now it no longer
  takes the whole scan down with it). The exclusion list is the mechanism to
  avoid creating new such waits on a port already known to be bad.
- A host-level driver unbind (e.g. a udev rule blocklisting the port by USB
  topology so it never enumerates) remains a possible, permanent, parallel
  mitigation. It is infrastructure, not carried by this feature, and is out of
  scope here.
- The reproduction of the actual driver-level hang requires the specific
  physical unit that first exhibited it; automated tests validate the
  abandon-and-continue mechanism against a fake never-responding port, not the
  specific hardware trigger.
