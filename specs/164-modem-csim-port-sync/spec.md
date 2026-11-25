# Feature Specification: A Slow SIM Answer No Longer Costs a Modem Line Its Registration, or Corrupts the Next Session on the Port

**Feature Branch**: `164-modem-csim-port-sync`
**Created**: 2026-11-25
**Status**: Draft
**Input**: #237. A modem line (`AT+CSIM`) registers in full when its SIM answers a refresh challenge after more than 5 s, and the abandoned reply then corrupts the next session on the port. Three parts: (A) sync a freshly opened AT port before its first command; (B) give AKA AUTHENTICATE and its GET RESPONSE on a modem a longer response budget than the port's 5 s; (C) log the SIM's timings and any stale bytes the sync discards. Out of scope: retrying the same challenge on modem lines, and reusing the registration's USIM AID. Card-reader lines must behave exactly as before.

## Background

On 2026-11-25 a reporter's modem line (EC20, `/dev/ttyUSB0`, on top of `c249cb1`) had a network-challenged registration refresh. It went like this:

1. The challenge thread opened the port, selected the USIM (2.9 s) and sent AUTHENTICATE. No reply came within the port's 5 s budget, so the refresh fell back to a full registration.
2. The full registration's first attempt failed with `SELECT 3F00 failed: SW=6A83` on a freshly opened port. That counted one renewal failure. The retry 8 s later worked.
3. The network handed the full registration the same challenge as the refresh. The SIM answered it with a sync failure, which a SIM only gives for a challenge it has already accepted. So the SIM had run the first AUTHENTICATE successfully, and only its reply was late. An AUTS resynchronisation followed, and the line was registered again 26 s after the challenge.

The line paid for a full registration it did not need: a new P-CSCF connection, new Gm SAs, a new reg-event subscription, an HSS resync and one counted renewal failure. The registration still had most of its 3600 s left.

Two defects are behind it.

**The 5 s budget is the port's, not the SIM's.** Every APDU is one `AT+CSIM` command under the port's default 5 s timeout, AUTHENTICATE included. On a modem the SIM is shared with the baseband's own SIM traffic, and AUTHENTICATE runs the card's AKA algorithm. The line's previous challenge already took 4.2 s from challenge to answer.

**An abandoned reply corrupts the next session on the port.** Measured on the local EC200U on 2026-11-25 (`tmp/reopen-test/`): a reply abandoned by closing the port is delivered to whoever opens the port next, within about 50 ms of the reopen. The open's own input flush runs first, so it does not help.

- Small replies, such as a `+CSIM` answer, arrive whole when the port is reopened at once.
- After 1 s, a lone trailing `OK` can survive.
- Large replies are held and delivered even 10 s later.

A new session starts out assuming the channel is clean, so it reads the stale reply as the answer to its own first command. Every later reply is then one command behind. A stale `9000` makes a SELECT look successful, so nothing notices until a check fails. Walked through the full registration's open (identities configured, EF_DIR with one record), this ends in exactly the reporter's `SELECT 3F00 failed: SW=6A83`.

A slow command that is still running inside the modem does the same: the new session's commands queue behind it in the modem, and its reply comes out first.

Every program that opens a shared modem port is exposed: the IMS agent, its SMS sweep, `vowifi-usim-bridge` (EAP-AKA for the ePDG tunnel), the PLMN and IMSI reads, discovery, and the circuit-switched workers.

## Clarifications

### Session 2026-11-25

- Q: Should the sync also run after a timeout inside a session that stays open? → A: Yes. After any timeout or desync inside a session, the session runs the same bounded sync before its next command, replacing today's drain. The drain stops at the first empty read, so a reply from a command the modem is still running arrives after it and shifts the channel by one, which is the same fault as on reopen.
- Q: How strong must the protection against a stale *sync* reply be? → A: Read-only, using reply order (option B). The sync sends two different read-only queries, each only after the previous one's reply arrives, and is in step once the second query's reply has been read. This is guaranteed against any single abandoned command or sync. Two or more syncs abandoned in a row while the modem stays busy are not covered. No modem setting is written (FR-004 stands).
- Q: Should the 20 s SIM-answer budget also apply to the AUTHENTICATE that `vowifi-usim-bridge` passes on for charon? → A: Yes, keep FR-007. The bridge recognises an AUTHENTICATE among the APDUs it passes on (instruction byte `0x88`) and gives it, and the GET RESPONSE that collects its result, the 20 s budget. Every other APDU it passes on keeps 5 s. Charon's own EAP timers remain the outer limit.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A session on a modem port never reads another session's reply (Priority: P1)

Some program on the host opens a modem's AT port that another program (or an earlier session of its own) recently abandoned mid-command. Today the new session can read the abandoned reply as its own and run one reply behind for its whole life, so its commands get the wrong answers. Instead, the new session first makes sure the channel is in step. It reads nothing as its own until a reply that can only belong to it has arrived.

**Why this priority**: it is a correctness fault on every shared modem port, not only in the reported case. An out-of-step channel can make a SIM command appear to succeed with another command's answer. It caused the failed attempt and the counted renewal failure in #237. It is also a precondition for any later retry on a modem line.

**Independent Test**: with a simulated modem that delivers an abandoned reply right after open (whole, as a lone `OK`, or after a delay while a slow command finishes), check that the first command of the new session gets its own reply, and that the session stays in step for every later command. Live: repeat the `tmp/reopen-test/` sequence through the bridge's own port opening and check that the stale reply is discarded and reported.

**Acceptance Scenarios**:

1. **Given** a modem port whose previous holder abandoned a command and whose reply is delivered on the next open, **When** a program opens the port and sends its first command, **Then** that command receives its own reply, and the abandoned reply is discarded and reported.
2. **Given** a modem that is still running an abandoned command when the port is opened, **When** the new session's first command is sent, **Then** the session waits for the abandoned reply to come out and discards it before taking any reply as its own, within the sync bound (FR-003).
3. **Given** a sync that did not complete within its bound, **When** the open finishes, **Then** the open fails with a timeout. It never hands back a session that may be out of step.
4. **Given** a modem port with nothing stale on it, **When** a program opens it, **Then** the open costs at most the sync's two short query exchanges, and the session behaves exactly as today.
5. **Given** an abandoned *sync* exchange from an earlier open, **When** a later open syncs, **Then** the earlier sync's late reply is not mistaken for the later sync's reply, whichever of its two queries it was waiting on when it gave up.
6. **Given** a session that stays open after one of its commands timed out while the modem keeps running it, **When** the session sends its next command, **Then** it first syncs, and the next command receives its own reply, not the late one.

---

### User Story 2 - A SIM that answers a challenge slowly but correctly keeps the registration (Priority: P1)

A modem line's registration refresh is challenged by the network, and the SIM takes longer than 5 s to answer, but answers. Today the answer is abandoned and the line registers in full. Instead, the line waits for the SIM's answer for as long as an AKA run on a busy modem can reasonably take, and completes the refresh in place on the same connection and SAs.

**Why this priority**: this is the reported symptom. Each occurrence costs a full registration and an HSS resync. If it lands during a call, the full registration waits for the call to end and the refresh lead time runs short.

**Independent Test**: with a simulated modem whose AUTHENTICATE reply takes longer than the port's default budget but less than the AKA budget, check that the refresh completes in place and that every other APDU still times out at the port's default budget. Live: not directly reproducible on demand, because no SIM here answers that slowly. Use the timings from C on the reporter's line.

**Acceptance Scenarios**:

1. **Given** a modem line whose refresh is challenged, **When** the SIM's AUTHENTICATE reply (and its GET RESPONSE, where the SIM uses one) arrives after the port's default budget but within the AKA budget (FR-005), **Then** the refresh completes in place and no full registration starts.
2. **Given** a full registration on a modem line, **When** its AKA reply arrives within the AKA budget, **Then** it is accepted in the same way.
3. **Given** a modem whose AUTHENTICATE really does not answer, **When** the AKA budget runs out, **Then** the failure reads exactly as today (the same timeout wording, the same `AT+CSIM failed` wording for an explicit error). The refresh falls back to a full registration, and the supervisor's existing classification and SIM recovery see the same signals as today.
4. **Given** any APDU other than AUTHENTICATE and its GET RESPONSE, **When** it is sent to a modem's SIM, **Then** it is bounded by the port's default budget, as today.
5. **Given** a card-reader line, **When** it answers a challenge, **Then** its behaviour is exactly as today.
6. **Given** charon running EAP-AKA through `vowifi-usim-bridge` on a modem line, **When** the SIM's answer to the AUTHENTICATE the bridge passes on (and its GET RESPONSE) arrives after the port's default budget but within the AKA budget, **Then** the bridge returns it to charon instead of failing the command.

---

### User Story 3 - The operator can see how long the SIM takes and how often a port was out of step (Priority: P2)

The operator, or the maintainer reading a reporter's log, needs to know whether a modem's SIM is getting slow and whether ports are being left out of step. Today neither is visible: per-command latency is only in trace output, and a stale reply is not noticed at all. Instead, each challenge leaves one line with its timings, and each sync that discards stale bytes leaves a warning.

**Why this priority**: it doesn't change behaviour, but it is how the AKA budget gets checked against real data, and how the frequency of US1's fault becomes known. It ships with US1 and US2 but is not required for either to work.

**Independent Test**: run a challenge through a simulated modem with known delays and check the logged timings. Open a port that delivers a stale reply and check for one warning with the discarded size.

**Acceptance Scenarios**:

1. **Given** a SIM challenge on any line, **When** it completes or fails, **Then** one debug-level line records how long it took to open the SIM, select the USIM (where that step exists) and run AUTHENTICATE, and the outcome.
2. **Given** a port open whose sync discards stale bytes, **When** the sync completes, **Then** one warning names the port and the number of bytes discarded.
3. **Given** a port open with nothing stale, **When** the sync completes, **Then** nothing is logged above debug level.

---

### Edge Cases

- **Abandoned reply arrives while the port is closed, then nothing.** The sync finds nothing stale and costs its two short exchanges.
- **A lone stale `OK`.** It must not end the sync's own exchange early. Only the B-reply that follows this sync's own B ends it (FR-002).
- **The modem is still running a slow command at open.** The sync waits for it, up to the bound. Past the bound, the open fails with a timeout, and the caller's existing handling applies (the agent retries with backoff, the SMS sweep tries next interval, and so on).
- **The port is not an AT port (discovery probing).** The sync is bounded by the probe's own short timeout, so probing a non-AT port takes no longer than it does today.
- **The modem is gone or wedged.** An open now fails during the sync instead of at the first command. The failure latency must not grow beyond what the caller's own timeouts and the agent's stall watchdog already allow (FR-003).
- **An unsolicited result code (`RING`, `+CMTI`, `+CLIP`) arrives during the sync.** It is discarded with the stale bytes and counted in the warning. A port that was just opened was not listening before the open either. Stored SMS is still found by the sweep's listing.
- **The AKA budget overlaps the SMS sweep or `vowifi-usim-bridge` wanting the port.** They wait for the port as today. A longer AUTHENTICATE holds the port longer only when the SIM is actually slow.
- **AUTHENTICATE relayed for charon's EAP-AKA through `vowifi-usim-bridge`.** The same SIM slowness hits the ePDG tunnel's authentication, so the relay gives an AUTHENTICATE it forwards to a modem's SIM the same budget (FR-007).
- **The sync itself is abandoned (bounded out), and its reply arrives later.** The next open's sync must not take that late reply as its own (US1 scenario 5, FR-002). Two or more syncs abandoned in a row while the modem stays busy are not covered (Assumptions).

## Requirements *(mandatory)*

### Functional Requirements

**A. Sync on open**

- **FR-001**: Every new session on a modem AT port MUST bring the channel into step before it accepts any reply as an answer to a caller's command. That includes every open of a port and every reopen that recovers a suspect channel. Until then, everything received is discarded.
- **FR-001a**: A session that stays open after one of its commands timed out, or after any other desync, MUST run the same bounded sync before its next command. This replaces today's drain, which stops at the first empty read. If that sync fails, the next command fails with a timeout and the channel is treated as suspect, as a failed resync is today.
- **FR-002**: The sync exchange MUST be two different read-only queries, A then B, that no other code path sends. A is sent first. B is sent only after an A-reply has been read, and the session is in step once a B-reply has been read after B was sent. Everything read before that is discarded, including our own A-reply when it arrives after a stale one. So a stale reply to any single earlier command, including a single earlier abandoned sync (whether it was waiting for its A-reply or its B-reply), MUST NOT leave the session out of step.
- **FR-003**: The sync MUST be bounded. By default the bound is the port's own response timeout, and a caller that opens with a shorter timeout (discovery probes) gets that shorter bound. If the sync does not complete within its bound, the open MUST fail with an error that existing timeout classifiers recognise as a timeout. It MUST NOT return a session that may be out of step.
- **FR-004**: On a port with nothing stale and a responsive modem, the sync MUST cost no more than its two short query exchanges. It MUST NOT change the modem's configuration.

**B. A budget for the SIM's AKA**

- **FR-005**: On a modem line, an AKA AUTHENTICATE sent to the SIM, and the GET RESPONSE that collects its result, MUST each be allowed a response budget of 20 s. Every other APDU and AT command keeps the port's default budget.
- **FR-006**: FR-005 MUST apply to both the in-place refresh challenge and a full registration's AKA.
- **FR-007**: `vowifi-usim-bridge` MUST apply the FR-005 budget to an AUTHENTICATE (instruction byte `0x88`) that it passes on to a modem's SIM for charon's EAP-AKA, and to the GET RESPONSE that immediately follows it when the AUTHENTICATE answered `61XX`. Every other APDU it passes on keeps the default budget. The bridge's existing handling of `AT+CSIM failed`, including its in-place SIM reset after repeated failures, sees the same errors as today.
- **FR-008**: When the AKA budget runs out, or the SIM returns an explicit error, the error text MUST be exactly what it is today. Every classifier that reads it (the supervisor's `AT+CSIM failed` match and SIM recovery, the timeout classifiers for registration status and the circuit-switched workers) MUST see the same input it sees today.
- **FR-009**: The agent's stall-watchdog budgets MUST stay above the worst case of the paths they guard, with the AKA budget counted for the AKA steps. The existing derivation test MUST be updated to count it.
- **FR-010**: Card-reader (`pcsc_reader`) lines MUST behave exactly as today, apart from the debug line in FR-011.

**C. Observability**

- **FR-011**: Each SIM challenge, on any line, MUST log one debug-level line with the time taken to open the SIM, to select the USIM (where that step exists), and to run AUTHENTICATE, plus the outcome.
- **FR-012**: A sync that discards stale bytes MUST log one warning naming the port and the number of bytes discarded. A sync that discards nothing MUST log nothing above debug level.

### Key Entities

- **Port session**: one open of a modem's AT port, from open to close. It is either in step (every reply it reads answers its own last command) or not. Today a new session is assumed to be in step. After this change it becomes in step only after its sync, and again after a timeout only after a fresh sync (FR-001a).
- **Sync exchange**: the short exchange a session runs to prove it is in step: two different read-only queries, A then B, used by nothing else, each sent only after the previous reply arrives. It writes nothing to the modem.
- **AKA budget**: the response budget for AUTHENTICATE and its GET RESPONSE on a modem's SIM (20 s). It is separate from the port's default budget (5 s).

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: In the `tmp/reopen-test/` scenarios (a stale reply delivered whole on reopen, a lone stale `OK`, and a large reply held for 10 s), the first command of a new session receives its own reply in 100% of runs, and every later command in that session stays in step.
- **SC-002**: A modem line whose SIM answers a refresh challenge in between 5 s and 20 s completes the refresh in place, with no full registration, no new P-CSCF connection and no counted renewal failure.
- **SC-003**: On a port with nothing stale, opening the port takes no more than 100 ms longer than today on the local rig.
- **SC-004**: A modem whose `AT+CSIM` really fails reaches the supervisor's SIM recovery through the same classification as today. The existing sim-recovery and classifier tests pass unchanged.
- **SC-005**: Every challenge leaves exactly one timing line, and every sync that discarded bytes leaves exactly one warning. A healthy line that sees no stale bytes logs nothing new above debug level.
- **SC-006**: Card-reader lines' tests and behaviour are unchanged, apart from the debug timing line.

## Assumptions

- 20 s for the AKA budget: the incident's reply arrived after 5 s and no later than about 11 s, and the previous challenge took 4.2 s end to end. The network waits far longer for an answer (the registrar's reg-await-auth timer, the same bound `CARD_RETRY_WINDOW`'s 30 s already sits inside). The C timings are there to check this value on real lines.
- The challenge runs on its own thread with a 600 s outer deadline, so a 20 s AUTHENTICATE does not hold up signalling.
- The EC200U measurement is taken to hold for the reporter's EC20 too: the host-side USB serial driver is the same, though the device's buffering may differ. The fix does not depend on how long a modem holds data, only on not trusting the channel until the sync proves it is in step.
- The modem has two read-only queries whose replies can be told apart from each other, from any `+CSIM` reply, and from every reply the bridge's other commands get. Choosing them is left to planning.
- Replies come out in the order their commands were sent, on a single port. The sync's guarantee (FR-002) rests on this.
- Not covered: two or more syncs abandoned in a row while the modem stays busy for more than two whole bounds (over 10 s at the default). The stale replies can then line up as A then B ahead of our own. A modem that busy fails the open anyway, and a unique marker would mean writing a modem setting shared across AT ports, including ModemManager's data port (rejected in Clarifications).
- Discarding an unsolicited result code during the sync's short window is acceptable. A port that was just opened was not listening before the open, and the SMS sweep's listing recovers stored messages.

## Out of Scope

- Putting the same challenge to a modem's SIM again after an AT timeout (the issue's option 2), to keep the registration when the SIM never answers. Deferred until C shows whether that happens.
- Reusing the USIM AID found at registration to cut a challenge's APDU count. Deferred: hardening, not a fix.
- Explaining why the reporter's SIM took more than 5 s. C provides the data for that.
- Any change to card-reader lines beyond the debug timing line.
