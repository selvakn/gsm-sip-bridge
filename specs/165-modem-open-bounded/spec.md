# Feature Specification: A Hung Modem No Longer Costs a Modem Line Its Registration

**Feature Branch**: `165-modem-open-bounded`
**Created**: 2026-11-28
**Status**: Implemented; verified live twice at 90 s and once at 180 s (tasks.md, Live verification)
**Input**: #239, second part. Opening a modem AT port is bounded the way every AT command already is, so a modem that hangs without disappearing no longer stalls the SMS sweep until the watchdog restarts the agent and the line registers in full. The first part of #239 (`a2ef631`, keeping the stalled run's log and the watchdog's report) is what showed where the stall was.

## Background

On 2026-11-26 the USB/IP link to a modem line's modem stalled for about 15 s and then closed. The line's IMS agent was stopped by its stall watchdog and came back only after a full registration. Nothing on the Gm side needed the modem at that moment: the ePDG tunnel stayed up, the Gm SAs were intact, and the next time the line needed the SIM was hours later. The agent's log, which would have said what stalled, was erased by the restarts (#239, first part).

On 2026-11-28 it was reproduced on purpose, with a build that keeps the watchdog's report. The modem's USB/IP traffic was dropped at the router for 90 s while the device stayed attached on the host:

```
14:09:02 ERROR watchdog: the dispatch loop has made no progress activity="sms-sweep" phase="sms-sweep" stalled_secs=94 budget_secs=90 last_at_command=AT+CMGL=4 last_at_command_age_secs=114
14:09:13 [supervise] line 0: vowifi-ims-agent exited (stalled on the modem); restarting in 5s
14:09:26 initial REGISTER response status=401 … registration through P-CSCF … registered
```

- **The stalled activity was the SMS sweep**, a thread of its own that keeps nothing of the registration. The dispatch loop, which serves SIP, was not reported.
- **The sweep stalled before it sent any AT command.** The last command it had sent (`AT+CMGL=4`) was 114 s old, from the previous pass; the stalled pass had been in its phase for 94 s. Every wait a pass makes is bounded: the modem lock (20 s), the port's sync (5 s plus 2 s grace, #237) and the open retries (under 2 s). Together they explain about 30 s at most. What is not bounded is the open itself, which runs on the calling thread; on a device that is attached but no longer answering it did not return. Kernel stacks of the stranded open on two repeats showed where: `open(2)` either sits in the driver, cancelling the USB control transfer that raises DTR on open, which cannot complete on the silent link, or waits for the tty's lock held by a close stuck the same way (research F1).
- **The cost was a full registration** (24 s unregistered, a new AKA run, new SAs, a new reg-event subscription) for a stall that touched only the SMS sweep.

Since #237 and specs/039, a command that does not come back strands the worker thread that sent it, and its caller returns on time. The open is the one step that still runs on the caller's thread.

Every program that opens a modem port goes through the same open: the IMS agent's registration, refresh challenge and startup, its SMS sweep and signal poll, `vowifi-usim-bridge` (EAP-AKA for the ePDG tunnel), the PLMN, IMSI and IMS-mode reads, discovery, the VoLTE paths and the circuit-switched workers.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A modem line stays registered while its modem hangs (Priority: P1)

A modem-backed VoWiFi line is registered. Its modem stops answering without disappearing from the host: a USB/IP link that stalls, a USB hub that wedges, a modem that hangs. For as long as the line does not need its SIM, it keeps its registration and keeps taking calls and IMS SMS. Today the SMS sweep stalls on the next open of the port, the watchdog stops the agent after 90 s, and the line registers in full.

**Why this priority**: this is the reported defect, reproduced live. The registration does not depend on the modem between SIM challenges, so losing it for a modem stall is avoidable cost: time unregistered, an extra AKA run, a new signalling association.

**Independent Test**: on a live modem line, drop the modem's USB/IP traffic at the router for 90 s and 180 s, with no SIM challenge in that window. The agent is not restarted, the line stays registered throughout, and the SMS sweep reports the modem as unavailable and resumes when the traffic is let through.

**Acceptance Scenarios**:

1. **Given** a registered modem line, **When** its modem stops answering while staying attached, and the line needs no SIM answer during the stall, **Then** the line stays registered, its agent is not restarted, and the watchdog does not report a stall.
2. **Given** the same stall, **When** the SMS sweep's next pass opens the port, **Then** the open fails within its bound, the pass reports that the modem did not open, and the sweep tries again on its usual interval.
3. **Given** the stall ends, by the modem answering again or by the device going away, **When** the next sweep pass runs, **Then** it opens the port normally or reports the device as missing, with no restart in between.

---

### User Story 2 - Nothing that opens a modem port can be held by a hung open (Priority: P1)

Any program that opens a modem port gets either an open, in-step port or a failure within a bound, whatever state the device is in. This holds for every caller, not only the SMS sweep: an IMS registration, a refresh challenge, `vowifi-usim-bridge` answering charon, the start-up reads.

**Why this priority**: a hung open on any of these paths stalls something else instead: a registration renewal until the watchdog's renewal budget (540 s), an agent start until its startup budget (600 s), charon's EAP exchange. The fix belongs where every open passes.

**Independent Test**: with a simulated device whose open never returns, check that the open fails within its bound, that the caller's existing handling of a port that is not in step applies, and that this holds through every open-retry loop.

**Acceptance Scenarios**:

1. **Given** a device whose open blocks indefinitely, **When** a caller opens it, **Then** the open fails within its bound with the existing "port not in step" failure, so every caller treats it as it already treats a modem that does not answer its sync.
2. **Given** an open-retry loop (the SMS sweep's, the IMS agent's wait for a busy port, `vowifi-usim-bridge`'s), **When** an open fails this way, **Then** the loop does not retry it.
3. **Given** a device whose open blocks and later completes, **When** it completes after the caller has given up, **Then** the port is closed at once and handed to no one.

---

### User Story 3 - A long hang does not pile up stuck threads (Priority: P2)

While the modem stays hung for minutes or hours, the SMS sweep keeps trying every 20 s, and other callers may try too. The process does not accumulate one stuck open per attempt.

**Why this priority**: without a limit, a long stall turns a bounded caller into an unbounded thread and descriptor leak (about 180 per hour from the sweep alone), which ends the process another way.

**Independent Test**: with a simulated device whose open never returns, run many opens in a row and check that only the first one is left outstanding, and that the others fail at once without starting another.

**Acceptance Scenarios**:

1. **Given** an earlier open of a port that has not returned, **When** another open of the same port is made in the same process, **Then** it fails at once, says an earlier open is still outstanding, and starts nothing.
2. **Given** that outstanding open finally returns, **When** the next open of the port is made, **Then** it is attempted normally.
3. **Given** two different ports, **When** one of them has an outstanding open, **Then** opens of the other are not affected.

---

### Edge Cases

- **The SIM is needed during the stall.** A refresh challenge, a full registration or an agent start needs the SIM. Its open fails within the bound and the existing paths apply: the challenge falls back to a full registration, a full registration fails and is retried on the renewal backoff, a start fails and the supervisor restarts the agent. Keeping the registration through that is out of scope: the SIM cannot answer.
- **The stall begins mid-command**, after the port is open. Already bounded since specs/039 and #237: the caller times out and the worker is stranded holding the port. The next open fails as the port is busy, within the existing open-retry limits.
- **The stranded open holds the port** (it got the device's exclusive lock and then stalled in setup). Other openers, in this or another process, fail as the port is busy, within their existing limits, until it returns.
- **The device goes away during the stall.** The stuck open returns with an error, the outstanding marker clears, and later opens fail at once as the device is missing, as today.
- **A call is up during the stall.** Calls do not use the modem on a VoWiFi line and are unaffected.
- **The modem's open is merely slow but completes within the bound.** It behaves exactly as today.
- **Card-reader lines** never open a modem port and are unchanged.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Opening a modem AT port MUST complete or fail within a bound, whatever the device does. The bound covers the device open and its terminal setup, and is separate from the existing sync bound that follows it.
- **FR-002**: The open's bound MUST be the port's own timeout (the timeout the open was asked for: 5 s by default, the probe timeout for discovery) plus the existing worker grace, so an open that fails this way takes no longer than its sync can.
- **FR-003**: An open that does not complete within its bound MUST fail as a "port not in step" failure (`is_port_out_of_step()` is true for it), whose text reads as an AT timeout, so every caller and classifier that already handles a modem that does not answer its sync handles this the same way without change. The message MUST say the port did not open in time, naming the port. Discovery's probes MUST be able to tell it apart from a port that opened but did not answer, and MUST keep counting it as a probe timeout toward the port's quarantine, as they counted a hung open before (specs/030).
- **FR-004**: The work of an open that was given up on MUST NOT reach any caller. If it completes later, the port MUST be closed at once, releasing its exclusive lock.
- **FR-005**: Once an open of a port has been given up on, no further open of that port MAY be started in that process until it returns: every further open MUST fail at once, with the same failure kind as FR-003 and a message saying an earlier open has not returned. The given-up opens of a port are therefore at most the opens of it that were already under way when it hung (one in the usual case), and their number never grows while the hang lasts.
- **FR-006**: Once the outstanding open returns, successfully or not, opens of that port MUST be attempted normally again.
- **FR-007**: FR-001 to FR-006 MUST apply to every way a modem AT port is opened, the first open and a session's reopen alike, and MUST NOT add a way to open a port that skips the existing sync (#237).
- **FR-008**: No open-retry loop MAY retry an open that failed under FR-003 or FR-005. The loops that exist today already return a "port not in step" failure at once; this MUST stay true, and MUST be covered by a test for each.
- **FR-009**: An open that succeeds within its bound MUST behave exactly as today: the same session, the same sync, the same errors for a missing or busy device.
- **FR-010**: Each open that fails under FR-003 or FR-005 MUST be visible in the logs of the program that made it, through that program's existing failure reporting (for the SMS sweep, its existing `modem SMS sweep failed` warning). This feature adds no new log line per pass.
- **FR-011**: The stall watchdog's decision to stop an agent, and the supervisor's handling of that exit, MUST stay as they are. With every step of an SMS sweep pass bounded, the sweep no longer outruns its budget on a hung modem; the watchdog remains the backstop for a stall in a place not yet known. The budgets that are derived from the timeouts of the steps a phase performs (the SMS sweep's per-message and per-pass budgets, the watchdog's sweep, renewal and startup budgets) MUST count the open's new bound, and keep the margins their existing tests require; any budget that has to rise to do so is named in the release notes.
- **FR-012**: Card-reader lines MUST behave exactly as before.

### Key Entities

- **Outstanding open**: an open of a given port, in this process, whose caller has given up on it and which has not returned yet. While a port has one, no new open of it starts. It ends when the open returns; if the open succeeded, the port is closed at that moment.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With a modem's USB/IP traffic dropped for 90 s and for 180 s while the device stays attached, and no SIM challenge in that window, the line stays registered: 0 agent restarts, 0 full registrations, 0 watchdog stall reports, in 2 out of 2 runs of each.
- **SC-002**: During such a stall, every SMS sweep pass ends within 20 s of starting and reports the modem as unavailable, and the first pass after the traffic is let through works without a restart.
- **SC-003**: No open of a modem port takes longer than its timeout plus the worker grace plus its sync bound, which is the timeout plus the grace again (14 s at the default 5 s timeout and 2 s grace) to succeed or fail, for any device behaviour, including one whose open never returns.
- **SC-004**: However long a stall lasts, the number of outstanding opens of a port in a process does not grow after the first one is given up (one, unless several opens of the port were already under way when it hung).
- **SC-005**: On a healthy modem, a port open costs no more than today plus one thread start (well under 10 ms), and the existing test suite passes unchanged apart from tests added for this feature.

## Assumptions

- **Where the stall was.** First inferred from the watchdog's report and from every other step of a pass being bounded, then confirmed by the kernel stacks of the stranded open on two repeats: `open(2)` sits either in `usb_wwan_dtr_rts` → `usb_control_msg` → `usb_kill_urb`, or in `tty_lock_interruptible` behind a close (research F1). The design does not depend on which: it bounds the whole open.
- **Stranding a thread is acceptable.** A stuck open cannot be cancelled from user space. Stranding the thread that runs it, as specs/039 already does for a stuck read, is the accepted cost; FR-005 bounds it to one per port.
- **The watchdog's per-activity policy is not changed.** #239 proposed letting a stalled modem-only activity run on instead of stopping the agent. With the open bounded there is no known unbounded step left in a sweep pass, and keeping a stuck thread alive cannot release the modem lock or the port it holds, so the line would only fail later, when the SIM is needed. This can be revisited if a kept stall report shows another place.
- **The registration still depends on the SIM when the network asks for it.** A challenged refresh, a full registration or an agent start during a stall fails as it does today; recovering that is out of scope.
- **The live test method is the router drop** used on 2026-11-28 (drop both directions of the modem's USB/IP connection in `FORWARD`), which reproduces an attached but silent device.
