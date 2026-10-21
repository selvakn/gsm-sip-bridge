# Feature Specification: Bound the tunnel-initiation work so a silent ePDG cannot freeze the shared IPsec daemon

**Feature Branch**: `118-bound-swanctl-initiate`
**Created**: 2026-10-21
**Status**: Draft
**Input**: GitHub issue #139 — "Unresponsive ePDG causes unbounded concurrent `swanctl --initiate` calls during VoWiFi startup". The issue's proposed resolution and acceptance criteria were deliberately not used; the requirements below come from an independent investigation and a local reproduction (see Background).

## Background (investigation findings)

On the strongSwan engine, one IPsec daemon is shared by every VoWiFi line. To bring a tunnel up, the supervisor asks that daemon to initiate the line's connection. Today each such request is a detached background client with **no deadline**. A request without a deadline occupies one of the daemon's fixed pool of worker threads (16) until the negotiation either succeeds or is destroyed. Because lines retry indefinitely (so that a recovering carrier is picked up again), a negotiation with a silent ePDG is retried in place and never destroyed — so the request never returns.

Separately, the startup loop issues a fresh request roughly every 30 seconds even while the line's negotiation is already in flight. Steady-state supervision (since #145) already leaves an in-flight negotiation to the daemon; the startup loop does not.

**Local reproduction (2026-10-21)**, using the shipped image's daemon against a peer that silently drops everything:

| Run | Requests | Outcome |
| --- | --- | --- |
| As shipped (no deadline) | one per cycle | After the 12th request the daemon's log stopped advancing and `swanctl --list-sas` timed out on every probe from then on; waiting clients grew by one per cycle (22 after 22 cycles). Killing every client did **not** unfreeze the daemon. |
| With a 20 s deadline on each request | one per cycle | Waiting clients levelled off at 3; the log advanced throughout and `--list-sas` answered on every probe across 22 cycles. |

The 12 matches the pool size (16) minus the daemon's own long-lived jobs. The startup loop cannot see this state: it only checks that the daemon process exists and reads its log, so a wedged daemon is reported as "still establishing" forever.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A silent ePDG leaves the line down but the system alive (Priority: P1)

An operator starts the bridge (or the bridge re-establishes a tunnel) while a carrier's ePDG is not answering at all — blackholed route, wrong address, carrier outage. The affected line cannot register, which is expected. But the bridge must stay responsive: status queries answer, the IPsec daemon keeps logging and processing, and resource use does not grow with the length of the outage.

**Why this priority**: This is the reported failure. Today a ~6-minute ePDG silence at startup permanently wedges the daemon, so the line can never recover and every other line sharing the daemon is exposed. It is the core defect.

**Independent Test**: Start a line against a peer that silently drops all IKE traffic; leave it for at least 30 minutes; sample the number of live initiation clients and the daemon's log/status responsiveness throughout. Counts stay flat and responsiveness holds.

**Acceptance Scenarios**:

1. **Given** a line whose ePDG silently drops all traffic from before startup, **When** the bridge runs for 30 minutes, **Then** the number of live initiation clients and waiter threads for that line never exceeds a small fixed bound and does not grow with elapsed time.
2. **Given** the same condition, **When** a status probe of the daemon's security associations is made at any point during the 30 minutes, **Then** it answers within its deadline.
3. **Given** the same condition, **When** the daemon's log is watched, **Then** it keeps advancing (the daemon is still doing its negotiation retries).

---

### User Story 2 - The line recovers by itself when the ePDG comes back (Priority: P1)

The ePDG starts answering again after a long silence. The line must establish its tunnel and proceed to IMS registration without anyone restarting the service.

**Why this priority**: Bounding resources is only useful if the line is not abandoned; the design must keep retrying for the life of the service. Unlimited recovery is the existing, intended behaviour and must not regress.

**Independent Test**: Run User Story 1's setup for several minutes, then restore reachability; observe the tunnel come up and registration begin with no restart.

**Acceptance Scenarios**:

1. **Given** a line that has been retrying against a silent ePDG for longer than the daemon's worker pool would previously have lasted, **When** the ePDG begins answering, **Then** the tunnel establishes and the line attempts IMS registration without a service restart.
2. **Given** an outage of any length, **When** reachability returns, **Then** the line is not marked permanently failed and no fixed retry count has been exhausted.

---

### User Story 3 - One negotiation per line, handed to the daemon while it is in flight (Priority: P2)

While a line's negotiation is in flight and the daemon is actively working on it, the startup loop leaves it alone, exactly as steady-state supervision already does. It starts a new request only when there is no negotiation, or the existing one has shown no activity for a defined period.

**Why this priority**: Even with deadlines, overlapping requests for the same line are wasteful and make the daemon's behaviour harder to reason about. It also aligns the two supervision phases on one rule.

**Independent Test**: With a fake engine reporting a negotiation in flight, run repeated startup ticks and assert no new request is issued until the stuck-activity threshold; with no negotiation reported, assert one is issued per cadence.

**Acceptance Scenarios**:

1. **Given** a line with a negotiation in flight that shows recent daemon activity, **When** the startup cadence fires repeatedly, **Then** no additional initiation request is started.
2. **Given** a request from this line that has not yet finished, **When** the cadence fires, **Then** a second concurrent request for the same line is not started.
3. **Given** a negotiation with no activity beyond the stuck threshold, **When** the cadence fires, **Then** it is terminated and re-initiated once.

---

### User Story 4 - A wedged daemon is detected during startup (Priority: P2)

If the IPsec daemon's control interface stops answering while a line is still establishing, the supervisor must classify it as unhealthy and recover (as steady state already does), rather than reporting the line healthy because the process still exists.

**Why this priority**: It is the safety net. Even if some future path (or an external cause) wedges the daemon, startup must not wait forever blind.

**Independent Test**: Make the control interface unresponsive while a line is establishing; assert the line reaches the defined unhealthy state within a bounded time and the recovery action is taken.

**Acceptance Scenarios**:

1. **Given** a line in the establishing phase and a daemon whose control interface does not answer within the probe deadline, **When** the startup loop next checks health, **Then** it records a control-interface-unresponsive state distinct from "daemon not running" and "still negotiating".
2. **Given** that state persists beyond its allowance, **When** recovery runs, **Then** it uses the same shared-daemon recovery as steady state, which accounts for other lines.

---

### User Story 5 - A silent ePDG on one line does not hurt the others (Priority: P2)

Several lines share one daemon. One carrier's ePDG goes silent; the other lines' tunnels, registrations and calls continue without interruption, and the silent line's retries do not consume the shared capacity the others need.

**Why this priority**: The shared daemon is the blast-radius amplifier; today one line can freeze all of them.

**Independent Test**: Two lines on one daemon, one pointed at a silent peer, one at a healthy peer (or a stub that answers). Over 30 minutes the healthy line stays registered and its calls work.

**Acceptance Scenarios**:

1. **Given** two lines sharing the daemon with one ePDG silent, **When** 30 minutes pass, **Then** the healthy line shows no re-key, DPD or registration interruption attributable to the silent line.
2. **Given** several lines all silent at once, **When** their requests run concurrently, **Then** the total capacity they can hold stays below the daemon's pool size with headroom left for the daemon's own work.

---

### User Story 6 - Shutdown during an unreachable-ePDG attempt leaves nothing behind (Priority: P3)

Stopping the bridge, or removing a line, while its initiation is pending terminates any outstanding initiation clients and waiter threads promptly.

**Why this priority**: A cleanup guarantee; lower severity because the process is exiting anyway, but long-lived clients and waiters must not outlive their line.

**Independent Test**: Start against a silent ePDG, request shutdown, verify no initiation child process or waiter thread remains.

**Acceptance Scenarios**:

1. **Given** a pending initiation against a silent ePDG, **When** shutdown is requested, **Then** within the shutdown allowance no initiation child process and no waiter thread of this line remains.
2. **Given** a line removed at runtime while pending, **Then** the same holds for that line and other lines are unaffected.

---

### Edge Cases

- The deadline expires while the daemon is still negotiating: the daemon's own retry continues (the deadline detaches the client; it does not cancel the negotiation), so the line is not abandoned and a later cadence tick does not stack another client behind it.
- The initiation request fails immediately (configuration not loaded yet): this must still be retried on the next cadence and must not be mistaken for a pending request.
- Many lines starting at once: the bound is per line, and the sum across lines must stay well under the daemon's pool size.
- A tunnel is up but no P-CSCF was assigned: the existing "terminate then re-initiate" behaviour is preserved and counts as one operation.
- Daemon restart mid-attempt: any pending client of the old daemon is discarded, not left waiting on a dead socket.
- The same bound applies to the startup request and to every later re-initiate (tunnel vanished, IKE_SA lost, CHILD_SA missing, stuck rekey, route-loss recovery, etc.); no path may start an unbounded client.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Every request the supervisor makes to the shared IPsec daemon to initiate a line's tunnel MUST have a finite lifetime, including the very first request at startup and every later re-initiate.
- **FR-002**: The lifetime MUST be shorter than the re-initiate cadence, so that under steady retrying at most one request per line is outstanding.
- **FR-003**: The supervisor MUST NOT start a new initiation request for a line while an earlier request for that same line is still pending.
- **FR-004**: A request's deadline MUST detach the client only; the daemon's own indefinite retry of the negotiation MUST be preserved, so a line is never abandoned after a retry count.
- **FR-005**: During establishment, if the line's negotiation is in flight and has shown daemon activity within the stuck threshold, the supervisor MUST leave it to the daemon; it MUST re-initiate only when no negotiation exists or the existing one has been inactive beyond the threshold (the same rule steady state uses).
- **FR-006**: Initiation-client and waiter resource use for a silent ePDG MUST be bounded independently of outage length.
- **FR-007**: The establishment phase MUST probe the daemon's control interface with a bounded deadline and MUST treat a probe that times out as a defined "control interface unresponsive" state, distinct from "daemon process missing".
- **FR-008**: An unresponsive-control-interface state during establishment MUST lead to the same recovery path steady state uses for that condition, which MUST take other lines on the shared daemon into account.
- **FR-009**: When ePDG reachability returns, the line MUST establish its tunnel and begin IMS registration without a service restart, after an outage of any length.
- **FR-010**: A silent ePDG on one line MUST NOT cause avoidable interruption or unbounded work on any other line sharing the daemon.
- **FR-011**: On shutdown or line removal, the supervisor MUST terminate any of that line's outstanding initiation clients and their waiters; none may remain once shutdown completes.
- **FR-012**: The supervisor MUST log, per line and without subscriber identifiers, when a request is started, when it is skipped because one is pending or a negotiation is in flight, and when a request's deadline expires, so an operator can see retry age and outcome.
- **FR-013**: The behaviour MUST be identical for the startup request and all later re-initiates (one shared rule), and the other tunnel engine (whose re-initiate is a no-op) MUST be unchanged.
- **FR-014**: Test fixtures and logs for this feature MUST use documentation address ranges and synthetic identifiers only; no real subscriber data.

### Key Entities

- **Initiation operation**: one outstanding request, for one line, to bring its tunnel up. Attributes: owning line, start time, deadline, outcome (succeeded / detached at deadline / failed / cancelled). At most one is live per line.
- **Negotiation in flight**: the daemon's own half-open attempt for a line, independent of any client; identified by the daemon's security-association listing, with a last-activity time.
- **Control-interface health**: whether the daemon answers a bounded probe; values at least *answering*, *unresponsive*, *daemon not running*.
- **Shared daemon**: the single IPsec daemon serving every strongSwan-engine line, with a fixed worker capacity that all lines draw from.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With a silent ePDG for at least 30 minutes, the number of live initiation clients per line never exceeds 1 (and the total waiter threads/processes for the line never exceeds a fixed small constant), with no linear growth over time.
- **SC-002**: Throughout that 30 minutes, 100% of bounded probes of the daemon's security associations answer within their deadline, and the daemon's log keeps advancing.
- **SC-003**: After reachability returns, the affected line has a tunnel within the daemon's normal retry interval plus one negotiation round trip, with no restart, regardless of how long the outage lasted (tested at 6 min and 30 min).
- **SC-004**: With one of two lines silent for 30 minutes, the other line records zero tunnel resets, zero registration losses and zero failed test calls attributable to the silent line.
- **SC-005**: When the control interface is made unresponsive during establishment, the line reaches the defined unresponsive state within one probe deadline plus one polling interval, 100% of the time.
- **SC-006**: Shutdown requested during a pending attempt completes with zero leftover initiation processes or waiter threads for that line.
- **SC-007**: A regression check fails the build if any code path starts an initiation request without a finite lifetime.

## Assumptions

- The deadline detaches only the client; verified in the pinned strongSwan fork, whose initiate handler returns "not established after N ms" and frees the worker while the negotiation continues under the connection's unlimited retry setting. Final values (deadline, probe deadline, stuck threshold) are chosen at planning time; the constraint is deadline < re-initiate cadence (~30 s) and probe deadline well under the polling interval budget.
- The existing steady-state rule for half-open negotiations (specs/100) is the model; this feature extends the same rule to the startup loop rather than inventing a new one.
- The daemon's worker pool size is not changed by this feature; raising it would only delay the freeze and would hide the unbounded growth.
- Trying another DNS-advertised ePDG after a bounded failure is a separate enhancement and out of scope.
- The SWu-IKEv2 engine is out of scope; its re-initiate is already a no-op.
- The local container-based reproduction used for this triage (silent UDP sink + the image's daemon + a synthetic connection) is a suitable basis for the automated or manual verification harness.
