# Feature Specification: Bounded modem I/O and stalled-line detection

**Feature Branch**: `039-at-stall-watchdog`
**Created**: 2026-08-17
**Status**: Draft
**Input**: User description: "Bound all modem AT I/O and detect a stalled IMS agent, so an unresponsive modem can never cause a silent multi-hour outage."

## Why this exists

On 2026-08-16 a live phone line stopped accepting calls for **2 hours 45 minutes**.
Callers heard "not reachable / switched off". Nobody found out until the line's owner
tried to ring it, because every health surface reported the line as healthy the whole
time: the container was `healthy`, the metrics said registered, and the status command
answered normally.

The physical trigger was minor and transient — the modem briefly stopped answering.
The damage was caused by the software's response to it: a routine hourly
re-registration issued a command to the SIM, waited for a reply that never came, and
waited **forever**. Because that wait happened on the one thread that also answers
incoming calls, the line went deaf at the same moment. The registration then quietly
expired, and the mobile network correctly concluded the phone was switched off.

Nothing noticed and nothing recovered. The supervisor only restarts a line whose
process has *exited*, and this process was alive — just permanently stuck.

This has since become considerably more likely. A recently merged change added a
routine sweep of the modem's own message storage that runs **every 20 seconds** on
every line with a real modem, using the same unbounded wait. What used to be a
once-an-hour exposure is now roughly 180 times an hour, and a sweep that gets stuck
also blocks the next re-registration behind it. That build is being deployed to the
live line, so detection and automatic recovery are needed first and independently.

## Clarifications

### Session 2026-08-17

- Q: If a stall is confirmed while a call is in progress, should recovery proceed, wait, or be judged differently? → A: Defer recovery while a call is in progress, but restart anyway once a hard ceiling is exceeded
- Q: Once escalation has given up on a line, does it stop trying or keep retrying? → A: Keep retrying on a slow cadence so the line self-heals if the hardware recovers; alert once, do not re-alert per retry
- Q: Does this cover both bearer types, or only the one that failed? → A: Both — they share the same modem access, contention and renewal machinery, so they share the defect
- Q: Should deadlines and progress budgets be tunable in configuration, or fixed? → A: Derived and fixed, with a single on/off switch for automatic recovery so a stall can be preserved for diagnosis
- Q: What recovers a line whose modem channel has been abandoned, given it keeps failing fast rather than appearing stalled? → A: Attempt to reopen the modem first; if it cannot be reopened because the abandoned work still holds it, trigger line recovery

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A stuck line recovers by itself (Priority: P1)

The line's owner is not at home. The modem stops answering. Instead of the line
silently going dead until someone happens to test it, the system notices within
minutes, restarts that line on its own, and the line is taking calls again shortly
after. If the same fault keeps recurring, the owner is alerted rather than left with
a line that restarts in a loop.

**Why this priority**: This alone converts a multi-hour silent outage into a
few-minute blip, and it does so for *any* future cause of a stuck line, not just the
one diagnosed here. It is also the only story that protects the line against the
newly-merged 20-second sweep, which is being deployed before the deeper fix lands.

**Independent Test**: Deliberately make the modem stop answering (hold its port from
another process, or remove the SIM) while a line is registered. The line must
recover to a call-answering state without any human action, and the reason must be
recoverable from the logs afterwards.

**Acceptance Scenarios**:

1. **Given** a registered line, **When** a routine message sweep stops responding
   partway through, **Then** the stall is detected within ~1 minute, the line is
   restarted automatically, and it returns to answering calls.
2. **Given** a registered line, **When** a re-registration stops responding partway
   through, **Then** the stall is detected within the operation's declared budget,
   the line is restarted, and the registration is re-established.
3. **Given** a stall has been detected, **When** the line restarts, **Then** the logs
   contain a single machine-readable marker naming the stalled operation, how long it
   was stuck, and the last command issued to the modem.
4. **Given** stalls recur on the same line, **When** the recurrence threshold is
   reached, **Then** the existing SIM-recovery escalation runs and the owner receives
   one alert — not one alert per restart.
5. **Given** a healthy line under normal load for a week, **When** no fault occurs,
   **Then** zero automatic restarts are triggered.

---

### User Story 2 - No modem operation can hang forever (Priority: P2)

Every command sent to the modem either completes or gives up within a stated time.
A modem that goes quiet produces an error that the surrounding logic already knows
how to retry, instead of freezing the line.

**Why this priority**: This removes the root cause rather than containing it. It is
P2 only because Story 1 must be protecting the live line first; without Story 2 the
line still suffers a short restart on every modem hiccup.

**Independent Test**: Point the software at a modem that accepts commands and never
replies. Every operation must return an error within its stated deadline, and the
process must stay responsive throughout.

**Acceptance Scenarios**:

1. **Given** a modem that never replies, **When** any command is issued, **Then** it
   fails within its declared deadline rather than waiting indefinitely.
2. **Given** a command that timed out, **When** the modem's late reply arrives
   afterwards, **Then** the next command returns *its own* reply and never the stale
   one.
3. **Given** a command that timed out, **When** further commands are attempted,
   **Then** they fail promptly rather than queueing behind the stuck one.
4. **Given** the line answers calls and sweeps messages concurrently, **When** one of
   them is waiting on the modem, **Then** the other is not blocked indefinitely
   waiting for its turn.

---

### User Story 3 - Health surfaces tell the truth (Priority: P3)

An operator can tell whether a line can actually receive a call, from one command or
one dashboard, and be right. A line whose registration has lapsed reports as
unhealthy, and says so specifically enough to act on.

**Why this priority**: The outage lasted hours because every signal said "healthy".
Even with Stories 1 and 2, honest reporting is what makes the next unknown failure
diagnosable in seconds instead of hours.

**Independent Test**: Force a line into an expired-registration state and confirm the
status command, the metrics, and the container health check all report it as unable to
receive calls, with a reason that distinguishes expiry from other causes.

**Acceptance Scenarios**:

1. **Given** a registration whose lifetime has elapsed, **When** an operator queries
   the line's status, **Then** it reports that the line cannot answer, and the stated
   reason identifies expiry specifically.
2. **Given** an expired registration, **When** metrics are collected, **Then** they
   show the line as not registered and expose how long ago the lifetime lapsed.
3. **Given** an expired registration on a resolved line, **When** the container's
   health is evaluated, **Then** the container reports unhealthy.
4. **Given** a line whose work has stalled, **When** metrics are collected, **Then**
   the stall is visible as a duration before it becomes severe enough to restart.
5. **Given** a line that has never been configured with a modem, **When** health is
   evaluated, **Then** it is not falsely reported as faulty.

---

### User Story 4 - Renewal timing follows the network, not a guess (Priority: P4)

The line renews its registration based on the lifetime the network actually granted,
comfortably before it lapses, and without renewing needlessly often.

**Why this priority**: Today the code assumes a one-hour lifetime. If the network ever
grants less, the line lapses before it renews — the same outage from a different
cause. Lower priority only because the current network does grant an hour.

**Independent Test**: Have the registrar grant a range of lifetimes, including a very
short one and none at all, and confirm renewal happens before expiry in every case and
no more than once per half-lifetime.

**Acceptance Scenarios**:

1. **Given** the network grants a lifetime shorter than the default, **When** the line
   registers, **Then** renewal is scheduled from the granted value, not the default.
2. **Given** a very short granted lifetime, **When** the line runs, **Then** it renews
   before expiry and does **not** renew repeatedly on every idle cycle.
3. **Given** the network states no lifetime, **When** the line registers, **Then** the
   existing default is used and behaviour is unchanged.

---

### User Story 5 - The system stops leaking processes (Priority: P5)

A long-running line does not accumulate dead processes.

**Why this priority**: Real but slow: 462 dead processes accrued in under four hours.
Left alone it eventually exhausts the process table and causes unrelated, confusing
failures.

**Independent Test**: Run a line for 24 hours and confirm the process count is flat.

**Acceptance Scenarios**:

1. **Given** a line running for 24 hours, **When** processes are counted, **Then** the
   count is stable with no accumulation of dead entries.
2. **Given** the supervisor is reaping abandoned processes, **When** it does so,
   **Then** it never disturbs the processes it is deliberately managing, and its own
   liveness checks continue to report correctly.

---

### Edge Cases

- **Stall during an active call**: recovery is deferred while a call is in progress, so
  a call whose audio is unaffected is not dropped by a restart. Because a stalled line
  may be unable to observe the call ending, the deferral is bounded by a hard ceiling
  after which the line is recovered regardless.
- **Stall in an unwatched background task**: the message sweep runs detached and is
  the most frequent user of the modem. Its stalls must be detected directly, not
  only when a later re-registration blocks behind it.
- **Repeated stalls**: recovery must not become a restart loop. Escalation and an
  eventual give-up-and-alert must bound it. After giving up, the line keeps retrying
  on a slow cadence so it returns to service by itself if the hardware recovers,
  without generating repeated alerts.
- **Abandoned work that never finishes**: an operation given up on may still be holding
  the modem, so subsequent attempts to use it must fail fast rather than queue. Because
  such a line keeps failing quickly rather than appearing frozen, lack-of-progress
  monitoring alone would never rescue it: the system must instead attempt to reopen the
  modem, and treat an inability to reopen as grounds for recovering the line.
- **A modem that talks but never finishes a reply**: endless output that never
  terminates must be bounded in both time and volume.
- **System clock changes**: a clock adjustment must not be mistaken for a stall.
- **Lines with no modem at all**: these must be exempt from modem-related faults and
  never reported as unhealthy for lacking a modem.
- **Restart while the network is unavailable**: a restarted line that cannot register
  must retry with backoff and report honestly, not appear healthy.

## Requirements *(mandatory)*

### Functional Requirements

**Bounded modem interaction**

- **FR-001**: Every operation against the modem MUST have an explicit deadline and
  MUST return either a result or an error before that deadline elapses.
- **FR-002**: The system MUST NOT allow any modem operation to block the handling of
  incoming calls indefinitely.
- **FR-003**: After an operation gives up, the next operation MUST NOT receive the
  abandoned operation's late reply.
- **FR-004**: After an operation gives up, further operations on that modem MUST fail
  promptly rather than waiting behind the abandoned one.
- **FR-005**: Contention for a single modem between concurrent activities MUST be
  bounded, so no activity waits for it indefinitely.
- **FR-006**: A modem producing unterminated output MUST be bounded in both elapsed
  time and accumulated volume.

**Stall detection and recovery**

- **FR-007**: The system MUST detect when a line has stopped making progress and MUST
  recover it automatically without human intervention.
- **FR-008**: Detection MUST cover background modem activity as well as the line's
  main work.
- **FR-009**: Each monitored activity MUST have its own progress budget, derived from
  the operations it performs, so that fast and slow activities are judged
  appropriately.
- **FR-010**: The system MUST confirm a lack of progress before acting, so a single
  transient observation cannot trigger recovery.
- **FR-011**: Recovery MUST emit exactly one machine-readable record identifying the
  stalled activity, its duration, and the last modem command issued.
- **FR-012**: Recovery MUST reuse the existing repeated-fault escalation, treating an
  unresponsive modem as the same class of fault as a failing SIM command.
- **FR-013**: The owner MUST be alerted on repeated stalls, and MUST NOT be alerted on
  every individual recovery.
- **FR-014**: Progress monitoring MUST be immune to system clock adjustments.
- **FR-015**: Stall detection MUST be deployable independently of the bounded-modem
  work, because it ships to the live line first.
- **FR-029**: When a stall is confirmed while a call is in progress, recovery MUST be
  deferred until the call ends, and MUST proceed regardless once a defined ceiling on
  that deferral is exceeded. A deferred recovery MUST be reported as deferred rather
  than appearing as an absence of any fault.
- **FR-030**: After escalation has exhausted its remedies for a line, the system MUST
  continue attempting recovery on a slow cadence rather than abandoning the line, so
  that a line whose hardware later recovers returns to service without human
  intervention.
- **FR-031**: Exhausted escalation MUST alert the owner once per incident; the slow
  retries that follow MUST NOT generate further alerts for that same incident.
- **FR-032**: Bounded modem access, stall detection and recovery, health reporting, and
  renewal timing MUST apply to every bearer that uses the shared modem and registration
  machinery, not only to the bearer on which the original failure was observed.
- **FR-033**: Deadlines and progress budgets MUST be derived from the operations they
  bound rather than being independently configurable, and it MUST be verifiable that
  every budget exceeds the worst legitimate duration of the activity it governs.
- **FR-034**: Automatic recovery MUST be disableable by a single setting, so a stalled
  line can be preserved for diagnosis.
- **FR-035**: When automatic recovery is disabled, stalls MUST still be detected,
  reported, and visible in the health surfaces; disabling recovery MUST NOT restore the
  original condition in which a stalled line appears healthy.
- **FR-036**: After an operation has been abandoned, the system MUST attempt to reopen
  the modem before concluding the line is unusable. If the modem reopens, the line MUST
  resume without being recovered; if it cannot be reopened because the abandoned work
  still holds it, the line MUST be recovered.
- **FR-037**: A line that is failing every modem operation MUST be recovered even
  though it is not failing to make progress, so that fast-failing and frozen lines are
  both rescued.

**Honest health reporting**

- **FR-016**: A line whose registration lifetime has elapsed MUST be reported as
  unable to receive calls, wherever its status is reported.
- **FR-017**: The reported reason MUST distinguish an elapsed registration from other
  causes of being unable to answer.
- **FR-018**: Collected metrics MUST expose the remaining (or elapsed) registration
  lifetime for each line.
- **FR-019**: Collected metrics MUST expose how long a line has been failing to make
  progress, at a lower threshold than the one that triggers recovery.
- **FR-020**: The container's health status MUST become unhealthy when a configured,
  resolved line's registration has elapsed.
- **FR-021**: Liveness reporting MUST reflect whether the line is actually making
  progress, and MUST NOT report a stalled line as live.
- **FR-022**: Lines that have no modem MUST NOT be reported as faulty on modem-related
  grounds.

**Renewal timing**

- **FR-023**: Renewal MUST be scheduled from the registration lifetime the network
  granted, falling back to the existing default only when none is stated.
- **FR-024**: The margin by which renewal precedes expiry MUST scale to the granted
  lifetime, such that a short lifetime cannot cause continuous re-registration.
- **FR-025**: For any granted lifetime, renewal MUST occur before expiry and MUST NOT
  occur more than once per half-lifetime.

**Process hygiene**

- **FR-026**: The system MUST NOT accumulate dead processes over time.
- **FR-027**: Reaping abandoned processes MUST NOT interfere with processes the
  supervisor deliberately manages, nor with its own liveness checks.
- **FR-028**: The recurring reachability check MUST NOT create abandoned processes.

### Key Entities

- **Line**: One SIM/modem pair with its own registration, health, and recovery
  lifecycle. Recovery acts on a single line without affecting others. A line reaches
  the operator's network over either of two bearers, both of which share the same
  modem access, contention and renewal machinery, and both of which are in scope.
- **Registration**: The line's presence on the operator's network, with a granted
  lifetime after which the network treats the phone as switched off.
- **Monitored activity**: A unit of work whose progress is tracked and which has a
  budget — for example registering, sweeping messages, or handling a call.
- **Stall record**: The machine-readable evidence written when recovery is triggered:
  which activity, how long, and the last modem command.
- **Fault incident**: An accumulating count of related faults on one line, driving
  escalation from restart, to SIM recovery, to alerting the owner.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: No single modem fault leaves a line unable to receive calls for more
  than **10 minutes** without automatic recovery (baseline: 2h45m).
- **SC-002**: A stall occurring during routine background modem activity is detected
  and recovery begun within **60 seconds**.
- **SC-003**: A line reported as able to receive calls can, in fact, receive one —
  **100%** agreement between reported health and actual reachability, verified by
  fault injection across expiry and stall cases.
- **SC-004**: **Zero** modem operations exceed their declared deadline under fault
  injection; a non-responding modem produces errors, never a hang.
- **SC-005**: **Zero** stale replies are returned to the wrong operation after a
  timeout, across repeated timeout-then-late-reply cycles.
- **SC-006**: **Zero** automatic restarts occur on a healthy line over a **7-day**
  soak.
- **SC-007**: Process count is flat over **24 hours**, with no accumulation of dead
  entries (baseline: 462 in 3.8 hours).
- **SC-008**: Renewal precedes expiry for every granted lifetime of **60 seconds or
  more**, with no more than one renewal per half-lifetime.
- **SC-009**: An operator can determine why a line cannot receive calls from a
  **single** status query, without reading logs or converting timestamps by hand.
- **SC-010**: Repeated stalls on one line produce **one** alert per incident, not one
  per recovery.
- **SC-011**: A line whose modem fault clears after escalation has given up returns to
  service within **30 minutes** of the hardware recovering, with no human intervention.
- **SC-012**: A line whose modem operation was abandoned either resumes without a
  restart (when the modem can be reopened) or is recovered — in no case does it remain
  indefinitely failing every operation.

## Assumptions

- **Restarting the line is the last-resort recovery mechanism**, because work already
  abandoned may still hold the modem and cannot be forcibly reclaimed. It is not the
  first response: recovery is deferred while a call is in progress (FR-029), and a
  reopen of the modem is attempted first (FR-036), so a line only restarts when the
  cheaper remedies do not apply or do not work. The existing supervisor restarts a line
  within ~5 seconds of it stopping, and a restarted line was observed re-registering in
  ~150 seconds.
- **A brief restart is preferable to a silent outage.** Recovery is tuned
  conservatively: budgets derived from the operations involved with roughly 25%
  margin, plus confirmation before acting, so that false restarts are effectively
  impossible even at the cost of slower detection.
- **Alerting reuses the existing escalation and notification path** rather than
  introducing a parallel one.
- **The current build ships to the live line before this work completes**, which is
  why stall detection must stand alone (FR-015).
- **Existing modem-access behaviour is retained.** Bounding is achieved by giving
  callers a deadline on work performed elsewhere, not by re-implementing low-level
  device handling, and not by relaxing the project's prohibition on unsafe code.
- **One process per line** already, so restarting a line does not disturb others.
- **Lines without a modem** (SIM in a separate reader) have no modem to bound or
  monitor and are out of scope for the modem-specific requirements.
- **Both bearers are in scope** (FR-032). They share the modem access, contention and
  renewal machinery, so the defect and the fix are common to both; only the bearer on
  which the outage was observed can be verified against live hardware, so the other is
  covered by automated tests.
- **Renewal margin defaults** stay as they are for the lifetime the current network
  grants; only shorter grants change behaviour.
- Verification depends on the existing hardware line for fault injection; automated
  tests must cover the non-responding and late-reply cases without hardware.

## Out of Scope

- Redesigning how the line answers calls or handles media.
- Re-implementing low-level serial device configuration.
- Alerting channels beyond the one already in use.
- Operator-tunable deadlines and budgets; these are derived and fixed (FR-033), with
  only an on/off switch for automatic recovery (FR-034).
- Diagnosing why this particular modem stops answering; this feature makes the
  software survive it, not prevent it.
