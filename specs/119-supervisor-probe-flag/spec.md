# Feature Specification: Put the supervisor's P-CSCF probe behind a flag, off by default

**Feature Branch**: `119-supervisor-probe-flag`  
**Created**: 2026-10-21  
**Status**: Draft  
**Input**: User description: "Move the supervisor's 30s TCP probe to the P-CSCF's unprotected port 5060 behind a config flag instead of removing it, and disable it by default. Operators on single-P-CSCF carriers can re-enable it. Live-verify on Vodafone and Jio." (issue #120)

## Background

Every 30s the line supervisor opens and closes a TCP connection from inside the line's network namespace to the P-CSCF's unprotected SIP port. A handset never does this: after security agreement it signals only on the protected ports. A P-CSCF or session border controller may rate-limit, log or blacklist repeated empty connections from a registered device. The probe also checks a path the agent does not use, so it can pass while the real signalling path is broken, and the reverse.

Issue #120's soak measurements showed the probe was doing three jobs besides its stated one, each of which has since been replaced:

| Side job the probe did | Replacement | Spec |
|---|---|---|
| Kept the tunnel from ever going idle, so strongSwan's dead-peer detection never fired and could not misfire | Dead-peer delay tuned to a handset-like value, and dead-peer losses classified by inbound traffic | 098 |
| Triggered the lost-default-route repair | The route is checked structurally on every tick, for the P-CSCF's address family | 096, #133 |
| Triggered a tunnel rebuild when a single-P-CSCF carrier's only P-CSCF went dead | The agent's own verdict (every P-CSCF marked unavailable) triggers the rebuild | 097 |

The other two probes (the idle-tunnel keepalive thread and the container health check) were already removed or made opt-in by spec 094. This feature deals with the last one.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A registered line stops making handset-unlike connections (Priority: P1)

An operator running a VoWiFi line wants the bridge's idle traffic toward the carrier to look like a phone's. With the default configuration the supervisor makes no TCP connections to the P-CSCF's unprotected port at all.

**Why this priority**: This is the whole point of issue #120. It is the only story that changes default behaviour.

**Independent Test**: Start a line with the default configuration, capture traffic inside the tunnel for several supervisor ticks, and count TCP connection attempts to the P-CSCF's unprotected port. The count is zero.

**Acceptance Scenarios**:

1. **Given** a VoWiFi line registered with the default configuration, **When** ten minutes pass with no calls, **Then** the supervisor has made zero connections to the P-CSCF's unprotected port and the line stays registered.
2. **Given** the same line, **When** the operator reads the startup log, **Then** one line states that the supervisor's P-CSCF probe is off and names the setting that turns it on.

---

### User Story 2 - Recovery keeps working without the probe (Priority: P1)

An operator whose line loses its default route, or whose only P-CSCF stops answering, still gets automatic recovery with the probe off.

**Why this priority**: Turning the probe off is only acceptable if the faults it used to catch are still caught. Issue #120 lists exactly three such faults; each needs a check that it still recovers.

**Independent Test**: With the probe off, inject each fault in turn (delete the namespace default route; black-hole the only P-CSCF on a single-P-CSCF carrier; black-hole the current P-CSCF on a multi-P-CSCF carrier) and confirm the line returns to registered without operator action.

**Acceptance Scenarios**:

1. **Given** a registered line with the probe off, **When** its default route is deleted, **Then** the route is restored on the next supervisor tick without a tunnel rebuild, for both IPv4 and IPv6 P-CSCFs.
2. **Given** a registered line on a carrier with one P-CSCF and the probe off, **When** that P-CSCF stops answering, **Then** the agent's verdict triggers a tunnel rebuild and the line registers again within the time spec 097 measured for this fault.
3. **Given** a registered line on a carrier with several P-CSCFs and the probe off, **When** the current P-CSCF stops answering, **Then** the agent fails over to a sibling and the line registers again without a tunnel rebuild.
4. **Given** a registered line with the probe off and the idle tunnel quiet, **When** the carrier's ePDG ignores one dead-peer request while traffic still flows, **Then** the tunnel is not torn down.

---

### User Story 3 - An operator can switch the probe back on (Priority: P2)

An operator on a carrier where the replacements recover too slowly, or who simply wants the old behaviour, sets one option and gets today's probe back unchanged.

**Why this priority**: The probe is being demoted, not deleted, because issue #120 measured agent-only detection of a dead single P-CSCF at roughly 115s plus up to 30s of supervisor poll, against about 97s with the probe. Some operators will prefer the faster detection.

**Independent Test**: Set the option on, repeat the single-P-CSCF black-hole run, and compare the recovery path and timing with the pre-change build.

**Acceptance Scenarios**:

1. **Given** the option turned on, **When** the supervisor ticks, **Then** it probes the P-CSCF every tick exactly as before this change, including choosing a reachable sibling P-CSCF when the current one fails.
2. **Given** the option turned on and the current P-CSCF black-holed, **When** the probe fails for the usual number of strikes, **Then** the line is rebuilt with the `pcscf_unreachable` reason and the existing counters and alerts fire as before.

---

### Edge Cases

- The probe also picks a reachable sibling P-CSCF for the supervisor's own view of which P-CSCF to use. With the probe off, that choice has to come from the agent's verdict or the current selection, and must never be a stale or invented one.
- The `pcscf_unreachable` recovery reason exists only because of the probe. With the probe off it cannot occur, and its metric stays at zero rather than disappearing, so dashboards and alerts keep working.
- The probe flag must not change the idle-tunnel keepalive setting from spec 094, which is a separate option and stays off by default.
- Both transport engines (strongSwan and the SWu one) have a probe. The flag must say what it does for each, and if an engine does not use the probe the flag must be accepted and ignored there, not rejected.
- A configuration that turns the probe on while the underlay guard (spec 100) says the uplink is down must still hold the rebuild, as today.
- An operator who sets the option to an invalid value gets a startup error naming the option, not a silent default.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST provide one configuration option that turns the supervisor's periodic TCP connection to the P-CSCF's unprotected SIP port on or off.
- **FR-002**: The option MUST default to off.
- **FR-003**: With the option off, the supervisor MUST make no TCP connection to any P-CSCF's unprotected SIP port, on any tick, in any state, for any engine.
- **FR-004**: With the option on, the supervisor's behaviour MUST be identical to the behaviour before this change: same cadence, same strike counting, same sibling selection, same `pcscf_unreachable` rebuild, same logs, same metrics.
- **FR-005**: With the option off, the per-tick default-route check and its repair MUST still run, for IPv4 and IPv6 P-CSCFs.
- **FR-006**: With the option off, the rebuild triggered by the agent marking every P-CSCF unavailable MUST still run, including its deferral during an active call and its alert.
- **FR-007**: With the option off, strongSwan's dead-peer handling MUST be unchanged by this feature: no new timers and no changes to the dead-peer delay default.
- **FR-008**: With the option off, the supervisor MUST NOT treat the absence of a probe result as a failure, a success, or a strike.
- **FR-009**: The startup log MUST state, once per line, whether the probe is on or off and which option controls it, in the same style as the idle-keepalive line from spec 094.
- **FR-010**: The `pcscf_unreachable` tunnel-recovery reason and its metric MUST remain defined and reported at zero when the probe is off.
- **FR-011**: The option MUST be documented where the other `[vowifi]` options are, with its default, what it trades off, and the recommendation for single-P-CSCF carriers.
- **FR-012**: An invalid value MUST fail configuration loading with an error that names the option.
- **FR-013**: The existing template and example configuration files MUST list the option, commented out at its default.

### Key Entities

- **Supervisor probe**: the periodic TCP connect to a P-CSCF's unprotected SIP port made by the line supervisor on its steady-state tick. Its attributes are its on/off setting and, when on, its cadence, strike threshold and sibling-selection role.
- **Agent liveness verdict**: the IMS agent's own judgement, from its protected-path keepalive and its pool of marked P-CSCFs, that signalling is or is not working. It is the signal that replaces the probe.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On a default-configured line, a one-hour capture inside the tunnel shows zero connection attempts to the P-CSCF's unprotected SIP port, on both Vodafone and Jio.
- **SC-002**: On a default-configured line, a 4.5-hour idle soak on both carriers ends with the line registered and no loss of registration, no tunnel teardown and no more than the dead-peer events the pre-change build showed in spec 098's soak (zero false dead-peer teardowns).
- **SC-003**: With the probe off, a deleted default route is restored within one supervisor tick plus one second, on both carriers, with no tunnel rebuild.
- **SC-004**: With the probe off, a black-holed sole P-CSCF on Vodafone recovers to a registered line without operator action, within 315s of the fault (spec 098's worst-case figure for the agent's detection and rebuild path).
- **SC-005**: With the probe off, a black-holed current P-CSCF on Jio fails over to a sibling and re-registers within the 193s the agent's failover took in issue #120's measurement, with no tunnel rebuild.
- **SC-006**: With the option on, the same single-P-CSCF fault recovers in the same time and by the same path as the pre-change build, within measurement noise.
- **SC-007**: The full test suite, including a test that runs the supervisor with the probe off and asserts no probe is issued, passes with the option off and on.

## Assumptions

- The three replacements named in the Background (specs 096, 097, 098 and the IPv6 route fix #133/#135) are merged on the branch this builds on; this is the case on main as of `a254929`.
- The option lives in the existing `[vowifi]` configuration section next to `idle_keepalive`, and takes a boolean. Its exact name is a planning decision.
- Turning the probe off will make dead-single-P-CSCF detection slower than the old probe by roughly 20–50s, and that is accepted by the user as the price of looking like a handset. The option exists so an operator who disagrees can opt back in.
- The SWu (non-strongSwan) engine's probe, if it has one, follows the same option; if it has none the option has no effect there.
- Live verification uses the local Vodafone rig and the remote Pi Jio rig, using fault-injection recipes from the earlier soaks (netns default-route deletion, a /32 or /128 route to the veth peer for black-holing). No real subscriber identifiers appear in any committed result.
- Removing the probe's code outright is out of scope; a later change can do that once the default has run in the field.
