# Feature Specification: Per-Line Network Isolation for VoLTE

**Feature Branch**: `020-volte-line-netns`
**Created**: 2026-07-24
**Status**: Draft
**Input**: User description: "Run the host-side LTE bridge's carrier-facing leg for each VoLTE line inside its own network namespace, so a card's SIP/RTP traffic to its P-CSCF can never egress via another line's LTE interface. Mirrors specs/013-multi-card-vowifi's per-line netns isolation, applied to the VoLTE path now that multiple LTE modems run in one shared network namespace with only loopback-port isolation between lines."

## Context

The VoWiFi path (features 011-013) gives every line its own network namespace: each SIM's tunnel
gets its own XFRM interface, its own routing table, and its own carrier identity, with no possible
collision between lines because the kernel enforces the boundary, not application code.

The VoLTE path never gained that isolation. Feature 015 decided against a namespace for VoLTE
specifically because, at the time, there was exactly one LTE interface to bridge and no second
agent to bridge it to — its own research recorded the condition for revisiting: *"only if
multi-card VoLTE ... is taken on later."*

That condition has now been met. A prior change brought multiple LTE modems into one host-side IMS
service, sharing a single PBX registration. It solved the collision that was visible during
development — several lines' internal signalling threads racing for the same local port — by
giving each line its own loopback port trio. **It left the collision that is not visible during
development untouched**: every line's carrier-facing SIP and RTP traffic still binds an unspecified
local address and relies on the one shared host routing table to decide, by destination alone,
which physical LTE interface a packet actually leaves on. With two modems each installing their own
default route, nothing stops a line's traffic from silently resolving onto a different line's
interface — and it is invisible until it happens, because every line's software believes it is
using its own connection throughout. It is most likely when two SIMs share a carrier, since their
IMS cores can then be reachable at the same or an adjacent address from either interface — an
operator scenario this project has already flagged as unproven.

This feature closes that gap the same way feature 013 closed it for VoWiFi: each VoLTE line's
LTE interface and everything that talks over it move into a namespace of the line's own, so which
physical connection a line's traffic uses is guaranteed by the kernel, not by every call site
remembering to bind correctly.

## Clarifications

### Session 2026-07-24

- Q: Should isolation explicitly guarantee no collision with VoWiFi's own per-line namespaces when both subsystems run in the same container, with a dedicated test for that combination? → A: Yes — in scope. A VoLTE line's namespace must never collide with a concurrently-running VoWiFi line's namespace, and this combination is tested, not just assumed safe by naming convention.
- Q: Should isolation be unconditional, or configurable with an opt-out back to today's shared-namespace behavior? → A: Unconditional. No configuration flag disables it; it applies to every line, single or multi, with no fallback code path.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A Line's Traffic Can Never Leave on Another Line's Connection (Priority: P1)

With two or more LTE modems bridged as VoLTE lines, every packet a line sends toward its carrier —
registration, call signalling, and call audio — physically leaves the host only over that line's own
LTE interface, regardless of what address it is addressed to and regardless of whether another
line's connection could also reach that address. This holds even when both lines' SIMs are on the
same carrier and their carrier-assigned addresses are close enough that a shared routing table could
not tell them apart.

**Why this priority**: This is the defect the feature exists to close. Everything else is either
already true (per-line signalling isolation, from the prior multi-modem work) or a compatibility
constraint on this change.

**Independent Test**: Attach two LTE modems provisioned on the same carrier, bring both VoLTE lines
up, and capture traffic on each line's LTE interface independently while both are registered and a
call is placed on each. Confirm each interface carries only its own line's registration and call
traffic — never the other line's — for the full duration of both tests.

**Acceptance Scenarios**:

1. **Given** two VoLTE lines are attached to the same carrier, **When** both register, **Then** each
   line's REGISTER and any subsequent signalling is observed only on that line's own LTE interface.
2. **Given** two VoLTE lines are registered, **When** a call is placed on each concurrently, **Then**
   each call's audio is observed only on its own line's interface, with no cross-talk and no packets
   attributable to one line appearing on the other's interface.
3. **Given** two lines' carrier-assigned addresses are numerically close enough that a single shared
   routing table could route either line's traffic to either interface, **When** either line sends
   traffic, **Then** it still leaves only on its own interface.
4. **Given** one line's LTE attachment is re-established (the periodic carrier-side detach/reattach
   already handled per line), **When** the reattachment completes, **Then** that line's traffic still
   leaves only on its own interface — isolation is not a one-time startup guarantee.
5. **Given** VoWiFi is also enabled and running its own per-line namespaces in the same container,
   **When** one or more VoLTE lines come up alongside one or more VoWiFi lines, **Then** every VoLTE
   line's isolation resources are distinct from every VoWiFi line's, with no naming or resource
   collision between the two subsystems.

---

### User Story 2 - Existing Single-Line and Multi-Line Behavior Is Unchanged (Priority: P2)

An operator running one VoLTE line today, or several lines on different carriers today, sees no
behavioral difference: registration, inbound and outbound calls, text messages, attachment-loss
handling, and status/metrics reporting all continue to work exactly as before. The isolation this
feature adds is not observable except as the absence of the cross-line failure mode in User Story 1.

**Why this priority**: This is a hardening change to an already-working multi-line feature. It must
not regress the thing it is protecting.

**Independent Test**: Run the existing single-line and multi-line VoLTE test and quickstart
procedures unmodified against the namespaced implementation and confirm every existing pass/fail
criterion still holds, including call answer latency, attachment-loss-during-call protection, and
per-line status reporting.

**Acceptance Scenarios**:

1. **Given** exactly one LTE modem is attached, **When** the system starts, **Then** VoLTE behavior
   (registration, inbound/outbound calls, SMS, status) is indistinguishable from before this feature.
2. **Given** several lines on different carriers (the case that already worked before this feature),
   **When** the system runs, **Then** all previously-passing multi-line acceptance criteria still
   pass.
3. **Given** a call is in progress on one line when that line's periodic LTE reattachment would
   otherwise fire, **When** reattachment is deferred, **Then** the call survives exactly as it does
   today.

---

### User Story 3 - One Line's Network Failure Does Not Affect Another Line (Priority: P3)

A problem confined to one line's network path — its interface losing carrier, its namespace's setup
failing, its route not appearing in time — is reported for that line alone. Other lines'
registrations, in-progress calls, and ability to accept new calls are unaffected.

**Why this priority**: Namespace isolation should strengthen fault isolation between lines, not
introduce a new shared point of failure. This is a safety property to verify, not new capability to
build — feature 013 established the same expectation for VoWiFi and it should hold here too.

**Independent Test**: With two lines running, simulate one line's interface failing to come up
(or losing carrier) and confirm the other line's registration and any in-progress call are
unaffected, and that the failure is attributed to the correct line in logs and status.

**Acceptance Scenarios**:

1. **Given** two lines are running, **When** one line's LTE interface loses carrier, **Then** only
   that line's registration is affected; the other line's registration and any in-progress call
   continue.
2. **Given** two lines are running, **When** one line's namespace/interface setup fails at startup,
   **Then** the other line still comes up and serves calls, and the failure is reported against the
   correct card identifier.

---

### Edge Cases

- **Two SIMs on the same carrier**: the scenario this feature is specifically motivated by (see
  Context) — must work with no cross-line leakage, and is the primary case User Story 1 tests
  against.
- **A line's namespace or interface teardown is interrupted** (crash, forced restart): the next
  startup must reach a clean state without manual cleanup, matching how VoWiFi's tunnel namespace
  and VoLTE's displaced-data-context restoration already handle interrupted teardown.
- **A call is in progress when a line's namespace needs to be recreated** (e.g. after an interface
  failure): the in-progress call on an *unaffected* line must not be disturbed; the affected line's
  call may be lost, but must be reported as such rather than silently.
- **Exactly one line configured**: isolation still applies (no special-cased "no namespace when
  there's only one modem" branch to keep behavior uniform and avoid a second, less-tested code path).
- **Container restart with lines already namespaced**: startup must be idempotent — a namespace left
  over from an unclean shutdown must not prevent the line from coming back up.
- **The shared PBX-facing telephone leg**: must continue to reach every line's carrier-facing half
  across the new namespace boundary exactly as reliably as it does today across the loopback split
  the prior multi-modem work introduced.
- **VoWiFi and VoLTE both enabled in the same container**: the documented normal deployment shape
  (see `docker/docker-compose.yml`). VoLTE's per-line isolation resources MUST be distinct from
  VoWiFi's own per-line namespaces — the two subsystems' isolation must not assume the other does
  not exist.

## Requirements *(mandatory)*

### Functional Requirements

**Isolation**

- **FR-001**: The system MUST ensure that a VoLTE line's carrier-facing traffic (registration
  signalling, call signalling, and call audio) can physically leave the host only via that line's own
  LTE interface, independent of the destination address and independent of what any other line's
  interface could also reach.
- **FR-002**: This guarantee MUST hold structurally — i.e. it must not depend on every current and
  future piece of code that opens a carrier-facing connection remembering to target the correct
  interface.
- **FR-003**: The isolation MUST apply for the lifetime of a line, including across the periodic LTE
  reattachment already handled per line, not only at startup.
- **FR-004**: The isolation MUST apply identically whether a line is the only line configured or one
  of several.
- **FR-004a**: A VoLTE line's isolation resources MUST NOT collide with a concurrently-running
  VoWiFi line's own per-line namespace when both subsystems are enabled in the same container.
- **FR-004b**: Isolation MUST be unconditional — the system MUST NOT expose a configuration option
  to disable it and fall back to today's shared-namespace behavior; there is exactly one code path,
  used whether one line or several are configured.

**Compatibility**

- **FR-005**: Existing single-line VoLTE behavior (registration, inbound/outbound calls, SMS,
  attachment-loss-during-call protection, status/metrics reporting) MUST be unchanged from an
  operator's perspective.
- **FR-006**: Existing multi-line VoLTE behavior established by the prior multi-modem work (shared
  PBX registration, per-line signalling isolation, per-line failure isolation, per-card
  identification in logs/metrics/SMS) MUST continue to hold unchanged.
- **FR-007**: The shared telephone-facing leg MUST remain able to reach every line's carrier-facing
  half across the new isolation boundary, with no reduction in reliability versus the existing
  loopback-based bridge.

**Fault isolation**

- **FR-008**: A failure confined to one line's network setup (interface not appearing, route not
  established, namespace setup failing) MUST NOT affect any other line's registration or
  in-progress call.
- **FR-009**: Such a failure MUST be reported against the correct line's card identifier, consistent
  with how other per-line failures are already reported.

**Lifecycle**

- **FR-010**: A line's network isolation resources MUST be established as part of that line's normal
  bring-up and MUST be torn down (or safely reusable) as part of its normal teardown.
- **FR-011**: Startup MUST be idempotent with respect to these resources: a prior unclean shutdown
  MUST NOT prevent a line from coming up cleanly on the next start.
- **FR-012**: Teardown and cleanup behavior MUST be at least as robust as the existing displaced-data
  -context restoration a line's teardown already performs.

### Key Entities

- **VoLTE Line**: Unchanged in identity from the prior multi-modem feature — one modem, one LTE
  attachment, one card identifier. Gains an isolated network context as an attribute of its runtime
  resources.
- **Line Network Context**: New. The isolated environment (interface, routing) a line's
  carrier-facing traffic runs inside. One per line, created at bring-up, independent of every other
  line's context, and not shared with the telephone-facing leg.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With two VoLTE lines provisioned on the same carrier, independent capture on each
  line's interface over a full registration-and-call cycle shows zero packets attributable to the
  other line, on either interface.
- **SC-002**: Single-line VoLTE call-answer latency and audio quality are unchanged (within existing
  measurement tolerance) from before this feature.
- **SC-003**: All previously-passing multi-line VoLTE acceptance criteria (from the prior multi-modem
  feature) continue to pass unmodified.
- **SC-004**: Forcing one line's network path to fail leaves every other line's registration and any
  in-progress call unaffected, with the failure attributed to the correct line within existing
  status/log reporting.
- **SC-005**: An unclean shutdown followed by a restart brings every line back up without manual
  intervention.
- **SC-006**: An operator can independently verify, using per-line network diagnostics, that a given
  line's traffic is confined to its own connection — the isolation is externally checkable, not just
  asserted by the software.
- **SC-007**: With VoWiFi and VoLTE both enabled and each running one or more lines in the same
  container, every line of either subsystem comes up with no isolation-resource collision and no
  degradation to any line's isolation guarantee from the other subsystem's presence.

## Assumptions

- **Isolation is achieved the same way VoWiFi already achieves it** — an OS-level network namespace
  per line — rather than by disciplined use of socket options or policy routing at every call site,
  because the latter has to be re-verified at every future call site forever and the former is a
  guarantee by construction. (This is a scope assumption for planning, not an implementation
  decision this spec makes on its own — `/speckit-plan` confirms it against the codebase's existing
  zero-`unsafe` and simplicity constraints.)
- **The shared telephone-facing leg keeps its current shape** (one process/thread pair reused by
  every line); only the carrier-facing half of each line moves into isolation, mirroring VoWiFi's
  Agent A (isolated) / Agent B (shared) split.
- **Line count and carrier behavior are unchanged** by this feature — it isolates the connections
  the prior multi-modem feature already establishes; it does not change how many lines are
  supported or how a line attaches to its carrier.
- **Live verification requires two physical modems**, ideally on the same carrier to exercise the
  worst case, matching the hardware-dependent verification boundary every prior VoLTE/VoWiFi
  multi-line feature has drawn.
- **No change to the PBX-facing SIP trunk or its registration** — this feature is entirely about the
  carrier-facing side of each line.
