# Feature Specification: Automatic VoLTE P-CSCF Priming

**Feature Branch**: `080-volte-pcscf-auto-prime`
**Created**: 2026-09-17
**Status**: Draft
**Input**: User description: "this feature, prime with when cache is missing or the pcscf is not set. clarify any gaps" (continuing from the prior conversation's discussion of turning docs/operations.md's "VoWiFi priming dance" runbook into an automatic, in-process workflow instead of a manual operator procedure)

## Clarifications

### Session 2026-09-17

- Q: Does automatic priming apply only to the `supervise`-orchestrated
  daemon startup path, or also to standalone CLI invocations
  (`volte-register`, `volte-listen`, `volte-call`)? → A: Scope to
  `supervise`'s orchestrated startup only. Standalone CLI commands keep
  today's behavior — they fail with the existing "run VoWiFi once" message
  if no cache/override exists; they are diagnostic tools whose users expect
  explicit, predictable behavior rather than an implicit tunnel side-effect.
- Q: In multi-line VoLTE deployments (auto-discovering several modems), which
  one does automatic capture use? → A: Always the first discovered VoLTE
  line (index 0) — matches the existing single-line default cache path and
  needs no new selection logic, since every line already reads the same
  shared cache value today regardless of which modem produced it.

## User Scenarios & Testing *(mandatory)*

<!--
  IMPORTANT: User stories should be PRIORITIZED as user journeys ordered by importance.
  Each user story/journey must be INDEPENDENTLY TESTABLE - meaning if you implement just ONE of them,
  you should still have a viable MVP (Minimum Viable Product) that delivers value.

  Assign priorities (P1, P2, P3, etc.) to each story, where P1 is the most critical.
  Think of each story as a standalone slice of functionality that can be:
  - Developed independently
  - Tested independently
  - Deployed independently
  - Demonstrated to users independently
-->

### User Story 1 - First VoLTE deployment on a new SIM/carrier (Priority: P1)

An operator enables VoLTE on a fresh deployment (new SIM, new container, or a
carrier that has never been primed before) without supplying an explicit
P-CSCF address. Today they must separately enable VoWiFi, restart, confirm a
capture file appeared, then flip back to VoLTE and restart again. With this
feature, the operator enables VoLTE only, and the system captures the address
it needs on its own before VoLTE registration is attempted.

**Why this priority**: This is the exact manual procedure documented in
docs/operations.md ("The VoWiFi priming dance") that this feature exists to
eliminate. Without it, VoLTE cannot register on any carrier tested so far
(Jio, Vodafone), so this is the mandatory happy path.

**Independent Test**: Start the bridge with `[volte].enabled = true`, no
`[[volte.line]].pcscf` override, and no pre-existing capture file. Observe
that the system captures a usable address on its own and VoLTE registration
proceeds without any manual VoWiFi configuration step.

**Acceptance Scenarios**:

1. **Given** VoLTE is enabled, no override is configured, and no capture file
   exists at the configured cache path, **When** the bridge starts, **Then**
   the system automatically captures a P-CSCF address before attempting VoLTE
   registration, and the operator observes this happening (not a silent
   registration failure).
2. **Given** the automatic capture succeeds, **When** VoLTE registration then
   runs, **Then** it uses the freshly captured address exactly as if an
   operator had run the manual dance and pointed `[volte].pcscf_source_path`
   at the result.
3. **Given** the automatic capture is in progress, **When** other unrelated
   bridge functions are already running (e.g., the circuit-switched
   GSM-to-SIP path), **Then** those functions are not disrupted by the
   capture activity.

---

### User Story 2 - Cache lost after a redeploy (Priority: P2)

An operator recreates the container (a new image, a `docker compose up -d
--force-recreate`, or any redeploy that does not preserve the previous
container's filesystem). The previously captured address is gone. Today this
silently reads as a carrier-side registration failure and the operator has to
recognize the real cause and redo the manual dance. With this feature, the
next startup notices the cache is gone and re-captures it automatically.

**Why this priority**: This is called out in docs/operations.md as "the
single most common way this dance goes wrong." Removing the manual recovery
step here is most of the day-2 operational value of this feature, distinct
from the first-time setup in User Story 1.

**Independent Test**: Prime a deployment successfully, then delete the
capture file (simulating a redeploy that wipes it) without touching any other
configuration, restart the bridge, and confirm it re-captures the address and
VoLTE registration succeeds again without operator intervention.

**Acceptance Scenarios**:

1. **Given** a previously-captured address existed but is no longer present
   at startup, **When** the bridge starts with VoLTE enabled and no override,
   **Then** the system re-captures it automatically, the same as a first-time
   setup.
2. **Given** the capture file is present but does not contain a usable
   address (e.g., corrupted or empty), **When** the bridge starts, **Then**
   the system treats this the same as a missing cache and re-captures.

---

### User Story 3 - Operator has already pinned a permanent address (Priority: P3)

An operator who has already captured a stable address for their SIM/carrier
pins it permanently (the "Making it permanent" step in docs/operations.md).
With this feature, the system must recognize a valid, usable address is
already available — whether from an explicit override or an existing valid
cache — and skip the capture step entirely, so pinned deployments see no
behavior change and no unnecessary extra startup activity.

**Why this priority**: Lower priority than Stories 1 and 2 because it is a
"do no harm" requirement for an already-working, already-documented
configuration, not new capability. But it must hold, or every restart of an
already-stable deployment would pay an unnecessary and disruptive capture
cost.

**Independent Test**: Configure an explicit `[[volte.line]].pcscf` override
(or leave a valid, pre-existing capture file in place), start the bridge, and
confirm no capture activity is attempted — VoLTE registration proceeds
directly using the already-available address.

**Acceptance Scenarios**:

1. **Given** an explicit P-CSCF override is configured, **When** the bridge
   starts, **Then** the system skips capture entirely and uses the override,
   exactly as it does today.
2. **Given** no override is configured but the cache file already contains a
   valid, usable address, **When** the bridge starts, **Then** the system
   skips capture entirely and uses the cached address.

### Edge Cases

- What happens when the automatic capture cannot obtain an address at all
  (e.g., no usable modem/SIM detected, no network reachable, carrier
  authentication fails)? The operator must be able to see clearly, from the
  bridge's own operator-facing output, that VoLTE could not start because
  priming failed — not a generic or misleading registration error. The
  system retries on the same cadence already used for other VoLTE/VoWiFi
  startup failures (FR-008) rather than giving up permanently.
- What happens if the capture step itself takes an unusually long time (SIM
  or network conditions that make the underlying capture mechanism slow)? The
  system must not appear to hang indefinitely with no operator-visible signal
  that priming is in progress.
- What happens if a valid, previously-captured address is for the wrong
  SIM/carrier (e.g., the SIM was physically swapped without wiping the
  cache)? Out of scope for this feature per the Assumptions below — this
  mirrors the existing manual-pin behavior, which already carries the same
  risk and is documented as such.
- What happens when an operator has explicitly and permanently enabled
  VoWiFi (`[vowifi].enabled = true`) at the same time as VoLTE? This is
  unchanged by this feature: the existing startup check that refuses to run
  both permanently at once still applies. Automatic priming only ever runs
  as a transient, internal action when VoWiFi is not otherwise persistently
  enabled.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST determine, at VoLTE startup, whether a usable
  P-CSCF address is already available (an explicit operator override, or a
  cache file containing a valid address) before attempting any capture
  activity.
- **FR-002**: When the bridge's own orchestrated startup sequence brings up
  VoLTE and no usable address is available (no override configured, and the
  cache file is missing, empty, or does not contain a valid address), the
  system MUST automatically perform the capture procedure that today
  requires an operator to manually enable VoWiFi and restart the container.
- **FR-002a**: Standalone diagnostic invocations of VoLTE functionality
  (outside the orchestrated startup sequence) are OUT OF SCOPE for automatic
  capture and MUST keep today's behavior: fail with the existing guidance to
  supply `--pcscf` or run the VoWiFi path once, rather than triggering an
  implicit capture.
- **FR-002b**: When VoLTE auto-discovers more than one line (multiple
  modems), automatic capture MUST use the first discovered line as its
  source — consistent with the existing single-line default cache path and
  with every line already reading the same shared cached value today,
  regardless of which modem originally produced it.
- **FR-003**: On a successful automatic capture, the system MUST persist the
  captured address to the same cache location VoLTE already reads today, so
  that the existing manual "pin it permanently" and multi-line
  file-per-carrier behaviors continue to work unchanged.
- **FR-004**: After a successful automatic capture, the system MUST proceed
  to normal VoLTE registration using the newly captured address, with no
  further operator action required.
- **FR-005**: The system MUST NOT perform automatic capture when a usable
  address is already available (override or valid cache), so that already-
  primed or explicitly-pinned deployments see no new startup activity.
- **FR-006**: The system MUST leave the existing refusal to run VoWiFi and
  VoLTE simultaneously as persistently-enabled configurations unchanged;
  automatic capture is an internal, transient action and is never itself
  reported to the operator as "VoWiFi is enabled."
- **FR-007**: The system MUST make the fact that automatic capture is
  running, and its outcome (success or failure), visible in the bridge's
  normal operator-facing startup output, so a capture failure is
  distinguishable from a carrier-side VoLTE registration failure.
- **FR-008**: When automatic capture fails, the system MUST retry it using
  the same restart-loop cadence already used elsewhere for VoLTE/VoWiFi
  startup failures (a fixed delay between attempts, retried indefinitely)
  rather than introducing a new, separate failure-handling behavior —
  capture failures are treated as just one more reason VoLTE startup did not
  yet succeed this cycle.
- **FR-009**: Once an address has been successfully captured and the cache
  contains a valid address, the system MUST NOT re-run capture on its own —
  a valid cached address is trusted until it goes missing or becomes invalid
  again, exactly as with today's manual dance. (A carrier silently
  reassigning its P-CSCF without invalidating the cache is out of scope, per
  the Assumptions.)
- **FR-010**: While automatic capture is in progress, the system MUST NOT
  block or delay any other bridge function that does not itself depend on
  VoLTE being registered (e.g., the circuit-switched GSM-to-SIP path).
  VoLTE's own registration waits on capture completing; unrelated
  functionality starts and runs independently, as it does today.

### Key Entities

- **P-CSCF cache**: The on-disk record of a previously captured P-CSCF
  address that VoLTE registration reads. Already exists today; this feature
  changes only when it gets (re)populated, not its format or location.
- **Capture attempt**: A single automatic run of the priming procedure,
  producing either a usable address or a reported failure.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An operator can take a brand-new deployment from "VoLTE
  enabled, nothing else configured" to a successfully registered VoLTE line
  without performing any manual VoWiFi enable/restart/verify/disable
  sequence.
- **SC-002**: After a redeploy that wipes the previously captured address, a
  VoLTE deployment recovers to a successfully registered state on its own,
  without an operator recognizing the specific cause and manually re-running
  the priming procedure.
- **SC-003**: A deployment that already has a valid override or cached
  address shows no observable change in startup behavior or startup time
  compared to today.
- **SC-004**: When automatic capture cannot succeed, an operator can tell
  from the bridge's own output, within the same startup attempt, that VoLTE
  did not register because priming failed — without needing to cross-
  reference the operations runbook to distinguish it from a carrier-side
  registration failure.

## Assumptions

- The mechanism used to capture a P-CSCF address is the existing VoWiFi/ePDG
  tunnel capture already implemented and documented in
  docs/operations.md — this feature automates *when* it runs, not *how* the
  capture itself works.
- The cache file's format and location (`[volte].pcscf_source_path`,
  `[vowifi].pcscf_source_path`, one file per line) are unchanged; this
  feature only changes what populates the file automatically.
- A captured address remaining valid for the life of a SIM/carrier pairing
  (as already assumed by the existing "pin it permanently" option) continues
  to be assumed; detecting a carrier silently reassigning its P-CSCF without
  the cache becoming invalid is out of scope.
- If the SIM/carrier changes without the cache being cleared, the system is
  not expected to detect the mismatch — this is the same limitation the
  existing manual-pin option already has.
- Automatic capture reuses the same modem/SIM already available to the
  bridge; no new hardware or network access is assumed.
