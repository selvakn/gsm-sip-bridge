# Feature Specification: Complete release of per-line kernel resources on stop

**Feature Branch**: `041-shutdown-resource-cleanup`
**Created**: 2026-08-20
**Status**: Draft
**Input**: User description: "Container stop must release every kernel resource a VoWiFi/VoLTE line created, so that restarting the container does not fail with `could not create tun23-0 (xfrm if_id 23): RTNETLINK answers: File exists` for ~2.5 minutes."

## Why this exists

Restarting the container costs every phone line roughly **two and a half minutes of
being unreachable**, every single time. Rebooting the whole machine costs nothing —
the lines come up in about eleven seconds. That difference is the entire subject of
this feature.

What happens on a restart today: the new run tries to create each line's tunnel
interface and the kernel refuses it, because the *previous* run's interface still
exists inside a namespace nobody can see or address any more. It holds the line's
tunnel identifier hostage until the kernel gets around to reaping it. During that
window the line has no data path at all: its agent starts, fails to reach the
carrier, dies, restarts five seconds later, and repeats — roughly a dozen times per
line — while the carrier sees the same tunnel being set up and torn down repeatedly.
Measured 2026-07-31 over nine restarts: 163s and 195s for two lines when the
container was replaced immediately, against 11s when the same restart followed a
three-minute stop.

The reason the resources outlive the container is that **stop does almost nothing**.
The teardown sends every child process a polite termination signal, does not wait to
see whether any of them actually exited, and then removes the *names* of the per-line
network namespaces. It never brings the carrier tunnels down, never releases the
encryption state that pins the tunnel interface, never deletes the tunnel interface
or the virtual cable pair the line created, and never confirms that anything it asked
for happened. The container then exits and whatever is still running is force-killed
by the container runtime — which is the worst possible moment for it, because a
force-killed process leaves the maximum amount of kernel state behind.

A second, quieter consequence: the namespaces a line creates are named only *inside*
the container. When a container is force-killed or crashes, its namespaces survive
with no name that any later run — or any operator on the host — can refer to. There
is no way to clean up after that container at all; the documented remedy in
`docs/operations.md` is to wait, and failing that, reboot the host.

An earlier investigation (recorded in `docs/operations.md`) concluded that nothing
could shorten the wait. That conclusion was drawn from measurements of the *current*
teardown only — signal-without-waiting plus namespace-name removal — and none of the
missing steps above were part of what was measured. This feature exists to do the
teardown properly and re-measure.

## Clarifications

### Session 2026-08-20

- Q: Does VoLTE get the same teardown treatment as VoWiFi, or is this VoWiFi-only? → A: Both bearers, and VoLTE's existing cleanup is refactored into the same step vocabulary rather than left as a parallel implementation
- Q: What is the restart-cost criterion measured against, given carrier attach alone varies 30s-2min? → A: Against a same-host, same-session baseline taken after a 3-minute stop (SC-000), not an absolute number
- Q: What happens when the teardown runs out of its stop allowance mid-way? → A: A global deadline with a fallback path — abandon the remaining waits and flush, go straight to releasing the devices and namespaces, and report having done so
- Q: Does a resource we failed to release escalate beyond the log (alert, exit code)? → A: No — log only, on both the stop and the start-side reclamation paths; no alert and no change to the exit code

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A restart costs seconds, not minutes (Priority: P1)

An operator restarts or redeploys the container — for a config change, an upgrade, or
because something needed a kick. Every configured line comes back up promptly,
because the run that just ended gave back everything it had taken. There is no dead
window, no repeated agent restart loop, and the carrier sees one tunnel setup per
line rather than a dozen.

**Why this priority**: This is the entire complaint, it affects every deployment on
every restart, and it is the only story that removes the outage rather than making it
more explicable. It also removes a recurring source of carrier-visible connection
churn, which matters for a subscriber line whose carrier can throttle or block
abusive registration patterns.

**Independent Test**: Restart the container while all lines are registered, and time
how long each line takes to reach a call-answering state. Compare against the same
measurement taken after a three-minute stop (the known-good baseline) and after a
host reboot. The three numbers must be comparable.

**Acceptance Scenarios**:

1. **Given** a deployment with all lines registered, **When** the container is stopped
   and immediately started again, **Then** every line reaches a call-answering state
   within the same time it takes from a freshly rebooted host, and no line reports its
   tunnel identifier as already claimed.
2. **Given** a deployment with all lines registered, **When** the container is stopped,
   **Then** the stop completes on its own within the runtime's grace period and no
   line process is force-killed by the container runtime.
3. **Given** the container has exited, **When** the host is inspected immediately
   afterwards, **Then** none of the tunnel interfaces, virtual cable pairs, namespaces
   or encryption state that run created are still present.
4. **Given** a line whose startup never completed (its tunnel never came up), **When**
   the container is stopped, **Then** the partial resources that line did create are
   released just like a fully started line's.

---

### User Story 2 - A crashed container can still be cleaned up (Priority: P2)

The container is force-killed, panics, or the machine loses power to it — no orderly
stop happens at all. The next run finds the previous run's leftovers, recognises them
as its own, clears them, and starts clean. An operator can see the same leftovers
from the host with ordinary tooling instead of a purpose-built privileged probe.

**Why this priority**: An orderly stop is the common case, but the ungraceful one is
exactly when leftovers are guaranteed, and today it is unrecoverable short of a host
reboot. It is second because it depends on the same mechanism as P1 and is worthless
on its own if the graceful path is still broken.

**Independent Test**: Force-kill the container without a grace period, start it again,
and confirm the lines come up promptly and that no leftover resources from the killed
run remain afterwards.

**Acceptance Scenarios**:

1. **Given** a container that was force-killed while lines were up, **When** the
   container is started again, **Then** the new run identifies the previous run's
   per-line resources, releases them, and brings each line up without waiting for a
   kernel timeout.
2. **Given** a container that was force-killed, **When** an operator inspects the host
   before restarting, **Then** the leftover per-line namespaces are visible and
   removable using ordinary host tooling.
3. **Given** leftover resources that are **not** this deployment's, **When** a run
   starts, **Then** it leaves them alone and says so, exactly as it does today.

---

### User Story 3 - A teardown that cannot finish says so (Priority: P3)

Something refuses to let go — a process ignoring termination, a device the kernel will
not release, a namespace still referenced. The stop does not hang forever and does not
pretend it succeeded: it bounds each step, reports precisely which resource it could
not release, and continues with the rest of the teardown.

**Why this priority**: This is what converts the next incident from an hour of
guesswork into a log line. It has no value until there is a real teardown to observe,
so it comes last — but it is what makes the earlier stories' failures diagnosable
instead of silent, which is the exact defect pattern this codebase has been bitten by
repeatedly.

**Independent Test**: Hold one of the per-line resources open from outside (a process
parked in the namespace, a device kept referenced), stop the container, and confirm
the stop still completes within its bound, names the resource it could not release,
and releases everything else.

**Acceptance Scenarios**:

1. **Given** a line process that ignores the termination signal, **When** the container
   is stopped, **Then** the teardown escalates to an unignorable stop within a bounded
   time and proceeds.
2. **Given** a resource the kernel will not release, **When** the teardown tries to
   release it, **Then** the attempt is bounded, the failure is reported with the
   resource named, and the remaining teardown steps still run.
3. **Given** a teardown that already ran, **When** it runs again over the same
   resources, **Then** it completes without error and without side effects.

---

### Edge Cases

- **Foreign encryption state on the host.** The host may carry IPsec belonging to
  something other than this deployment, and the available bulk-clear operation cannot
  be filtered to only our own entries. The existing all-ours-or-nothing rule must be
  preserved: anything not positively identifiable as ours vetoes the clear, at stop
  just as at start.
- **Grace period exceeded.** The teardown must notice it is running out of allowance and
  spend what is left on the steps that release resources rather than on waiting (FR-019).
  If it is force-killed anyway, the deployment must be no worse off than today — and the
  next start must still recover, via the P2 path.
- **Two runs overlapping.** A stop that is still finishing while a new run has already
  started must not have the new run's freshly created resources torn down by the old
  run's teardown, and vice versa.
- **Only some lines started.** Lines that failed at any stage of startup — no modem,
  no SIM, tunnel never established — may still have created some of their resources.
  Every resource actually created must be released regardless of how far that line got.
- **Both bearer types.** VoLTE lines create their own namespaces and virtual cable
  pairs on the same host. They share the leak, so they share the fix — and they are
  described by the same teardown, not by a second one that happens to resemble it
  (FR-018). A VoLTE line has no tunnel to terminate and no encryption state of its own,
  so those steps simply do not arise for it; everything else applies unchanged.
- **Repeat stop.** A stop signal arriving twice, or arriving while startup is still in
  progress, must not produce a partial or duplicated teardown.
- **A host with no leftovers.** Startup on a clean host must not be slowed down or
  made noisier by the new reclamation path.

## Requirements *(mandatory)*

### Functional Requirements

#### Ordered, confirmed teardown

- **FR-001**: On stop, the system MUST confirm that each line's processes have actually
  exited before it releases any resource those processes are using.
- **FR-002**: The system MUST escalate to an unignorable stop for any line process that
  has not exited within a bounded time after being asked to stop.
- **FR-003**: On stop, the system MUST bring each line's carrier tunnel down
  deliberately — asking the tunnel to be terminated rather than relying on the tunnel
  process being killed.
- **FR-004**: On stop, the system MUST release the encryption state associated with its
  own lines while the container is still running, subject to FR-011.
- **FR-005**: On stop, the system MUST explicitly delete each line's tunnel interface
  and each line's virtual cable pair — for lines of either bearer type — rather than
  relying on namespace destruction to reap them.
- **FR-006**: The system MUST perform these steps in an order where each resource is
  released only after everything that references it has been released, ending with the
  line's namespace.
- **FR-007**: The teardown MUST cover every resource a line actually created, including
  those created by lines whose startup did not complete.
- **FR-008**: The teardown MUST be idempotent — running it over already-released
  resources completes normally and changes nothing.
- **FR-018**: Both bearer types MUST be described by one teardown, not by two parallel
  ones. The existing VoLTE cleanup is expressed in the same vocabulary as the rest, so
  that every ordering, bounding, reporting and reclamation guarantee in this section
  holds for a VoLTE line by construction rather than by being reimplemented for it.
  Externally observable VoLTE behaviour — which cleanup runs, inside which namespace,
  in which order relative to its processes — MUST be preserved.

#### Bounds and reporting

- **FR-009**: Every teardown step MUST be individually bounded in time; no step may
  block the rest of the teardown indefinitely.
- **FR-010**: The teardown as a whole MUST be able to complete within the container
  runtime's configured stop allowance, and the deployment MUST configure an allowance
  large enough for the worst-case line count it supports.
- **FR-011**: The system MUST NOT release encryption state it cannot positively
  identify as its own; when it declines, it MUST say so and continue with the rest of
  the teardown.
- **FR-012**: The system MUST report each resource it failed to release, naming the
  resource and the reason, and MUST report the outcome of the teardown as a whole. The
  same applies to resources the start-side reclamation could not release.
- **FR-019**: The teardown MUST track its remaining allowance as a whole, not only per
  step. When what remains is no longer enough to release the resources, the system MUST
  abandon the outstanding waits and encryption-state release, proceed directly to
  deleting the devices and namespaces, and report that it did so and why.

  The step order is a dependency order, not a priority order: the deletes come last
  because everything referencing a device must go first, yet those deletes are the only
  steps that actually give a tunnel identifier back. Without this requirement the design's
  own worst case — one process refusing to exit — spends the entire allowance waiting and
  is force-killed before it releases anything.
- **FR-020**: Reporting is the whole of the escalation. A resource that could not be
  released MUST NOT raise an alert on the critical-alert channel and MUST NOT change the
  process exit code, on either the stop or the start path.

#### Recovering from an ungraceful exit

- **FR-013**: The per-line namespaces a run creates MUST remain addressable after that
  run's container has gone, so a later run or an operator can release them.
- **FR-014**: On start, the system MUST detect per-line resources left by a previous run
  of this same deployment and release them before creating its own, without waiting for
  a kernel timeout.
- **FR-015**: Reclamation on start MUST NOT touch resources that are not this
  deployment's, and MUST leave the existing behaviour for foreign encryption state
  unchanged.
- **FR-016**: The system MUST NOT release resources belonging to a *concurrently
  running* instance of itself; only resources of a run that has ended are eligible.

#### Documentation

- **FR-017**: The operations guide MUST be updated to replace the current "wait it out,
  nothing shortens this" guidance with what the system now does on stop, what an
  operator should see, and what to check when a resource is reported as unreleasable.

### Key Entities

- **Line resources**: everything one phone line causes to exist on the host for the
  duration of a run — a network namespace, a tunnel interface bound to a per-line
  tunnel identifier, a virtual cable pair joining the namespace to the container, and
  the encryption state that carries the line's traffic. Created at startup, and the
  complete set that must be given back at stop.
- **Tunnel identifier**: a small integer, fixed per line, that a line's tunnel
  interface claims for as long as that interface exists. Claimed against the container's
  own namespace even though the interface lives inside the line's namespace, which is
  why a leftover interface is invisible yet blocking.
- **Teardown plan**: the ordered sequence of steps that releases a run's line resources,
  derived from what that run actually started.
- **Stop allowance**: the time the container runtime grants between asking the container
  to stop and force-killing it.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-000** *(baseline, measured not asserted)*: On the host under test, in the same
  session, restart-to-call-answering is measured after a **3-minute stop** — the state in
  which no previous run's resources can still be held. This is the known-good baseline
  every restart criterion below is stated against. Measured 11 seconds on 2026-07-31.
- **SC-001**: After an **immediate** stop-and-start, every configured line reaches a
  call-answering state within **10 seconds of the SC-000 baseline** on the same host —
  i.e. an immediate restart carries no meaningful penalty over a well-separated one.
  Today that penalty is 150-185 seconds. Stated relative rather than absolute because
  carrier attach alone varies from 30s to ~2min (`docker/docker-compose.yml`, the
  healthcheck's `start_period` justification), so an absolute number would mostly measure
  the carrier that day rather than whether the resources were released.
- **SC-002**: Across **10 consecutive** immediate restarts, no line reports its tunnel
  identifier as already claimed at any point.
- **SC-003**: Within **5 seconds** of the container exiting, none of the namespaces,
  tunnel interfaces, virtual cable pairs or encryption entries that run created remain
  on the host.
- **SC-004**: The stop completes without the container runtime force-killing anything,
  in **10 out of 10** restarts.
- **SC-005**: Agent restart-loop messages in the first five minutes after a restart fall
  from roughly **12 per line** to **at most 1 per line**.
- **SC-006**: Carrier-visible tunnel setups per line across a restart fall from roughly
  **8** to **at most 2**.
- **SC-007**: After a force-kill with no grace period, the next start brings every line
  to a call-answering state within **30 seconds of the SC-000 baseline** — a wider margin
  than SC-001 because this path pays for reclamation the killed run never did.
- **SC-008**: On a host with no leftovers, startup time to first line registered is
  unchanged within **5 seconds** of the current baseline.
- **SC-009**: Every teardown run produces a record from which an operator can tell,
  without further investigation, which resources were released and which were not.
- **SC-010**: With a line process deliberately made unkillable-in-time, the teardown still
  releases every tunnel identifier before the allowance expires, and reports which waits
  it abandoned to do so.

## Assumptions

- The deployment is the privileged, host-networked container described by the project's
  compose file, which is the only environment where per-line namespaces and tunnel
  interfaces are created. Operators restart it with the normal compose commands.
- Both tunnel engines are in scope, but the primary engine is the one used in
  production; the fallback engine gets the same treatment where the concept applies.
- The existing all-ours-or-nothing rule for foreign encryption state is correct and is
  carried over to the stop path unchanged, rather than re-litigated here.
- Making a run's namespaces addressable after its container is gone implies exposing
  them at host scope. Only one instance of this deployment runs on a host, so this is
  accepted; FR-016 covers the case where that assumption is violated.
- A leftover namespace found at start is **deleted and recreated** rather than adopted.
  Adoption would also avoid the wait, but it would inherit whatever addresses, routes
  and stale state the previous run left inside, which is a larger and riskier change.
- The 2.5-minute figure and the "nothing shortens this" conclusion in
  `docs/operations.md` come from measurements of the *current* teardown only. This
  feature is expected to invalidate that conclusion, but the claim is not considered
  settled until re-measured on the live hardware — the deployment's real behaviour, not
  a test suite, is the acceptance evidence for SC-000 through SC-007 and SC-010.
- Kernel-level cleanup semantics cannot be exercised in the test environment, so
  automated tests verify the *plan* — that the right steps are emitted in the right
  order, bounded, and reported — while the outcomes above are verified live.
- Accepted cost of FR-020 (log-only escalation): a *persistently* failing start-side
  reclamation stays invisible until someone times a restart, because the symptom — a slow
  restart — is exactly the symptom this feature removes. If that turns out to happen in
  practice rather than in theory, the alert belongs on the start path, where there is no
  deadline to spend, not on the stop path.
