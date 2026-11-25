# Feature Specification: The Supervisor Stops Retrying in a Tight Loop While a Card-Reader SIM Is Out of Reach

**Feature Branch**: `163-pcsc-outage-backoff`
**Created**: 2026-11-24
**Status**: Draft
**Input**: Follow-up to #233 (PR #235, merged). Stop the supervisor's futile restart loops while a card-reader (`pcsc_reader`) VoWiFi line's SIM is out of reach.

## Background

#233 made the IMS agent ride out a SIM it cannot reach: it retries a challenge, keeps the registration while it waits for the card, and never exits on its restart budget for a reader outage. That covers the agent while it is running and the ePDG tunnel is up.

The tunnel needs the SIM too. Bringing it up runs EAP-AKA against the card, through charon's `eap-sim-pcsc`. An agent that starts with no card exits at once. So when anything makes the supervisor rebuild the tunnel or restart the agent while the card is out, the line falls into two loops that cannot succeed until the card is back. These were seen live on 2026-11-24, with the local rig's OMNIKEY reader unbound from USB:

- **Agent loop.** Each new agent fails to open the card (`IMS error: SIM card unreachable: no real PC/SC reader found …`) and exits. The supervisor restarts it 5 s later: one restart about every 6 s, roughly 600 an hour.
- **Tunnel loop.** About every 50 s the supervisor re-initiates the IKE SA. Each attempt is a full exchange with the carrier's ePDG that ends in a failed EAP-AKA (`SCardConnect: No smart card inserted`, `AUTHENTICATION_FAILED`): roughly 70 failed logins an hour.

Neither loop backed off over 11 minutes. The line recovered by itself, registering 7 s after the card came back. Meanwhile the loops buried the logs and sent the carrier a steady stream of failed authentications.

The loop starts whenever the tunnel must be rebuilt or the agent restarted while the card is out. That includes a P-CSCF failure, a lost IKE SA, or the IKE SA reaching its maximum lifetime (charon logged 15610 s), which needs a fresh EAP-AKA. So any card outage longer than the current IKE SA's remaining lifetime ends in this loop, whatever the agent does.

The supervisor already has both halves of the remedy, for other causes:

- **Agent exits.** After each exit the supervisor reads the agent's log and classifies the exit: a modem SIM failure (`AT+CSIM failed`), or a modem stall, detected by a marker the agent and the supervisor share. It counts these per incident, applies the modem remedy (`AT+CFUN` power cycle), and once the remedies are spent, backs off to a 15-minute restart. It alerts once and clears on recovery. A card-reader agent's exit is "other": it resets the counters and always restarts in 5 s.
- **Tunnel recoveries.** The underlay guard (specs/100) holds recoveries that cannot work while the uplink to the ePDG is down. It gives a lost IKE SA exactly one initiation per hold, makes an owed agent restart wait for the hold to end, caps every hold at 15 minutes, and polls every 5 s during a hold, so that the end is noticed quickly. Its hold causes are uplink causes only (no route, no response), and the supervisor has no check of whether a line's card can be reached.

## Clarifications

### Session 2026-11-24

- Q: How should the operator be told a line is down because its card is out of reach? → A: Logs, plus the existing tunnel-failure alert names the cause ("SIM card out of reach") while the supervisor is holding the line for its card. No new alert category, metric or status value (option B).
- Refinement during planning (FR-009): while the card is out, a recovery that would tear down an established IKE SA is held until the card is back, not paced. Only initiations for a missing IKE SA are paced (FR-007). Rebuilding needs the SIM, so tearing down could only lose what is left; on 2026-11-24 the supervisor deleted a working IKE SA it then could not rebuild.
- Refinement during planning (FR-013): a paced retry keeps one log line, with its delay (US3 scenario 3). Held ticks log nothing at the default level. SC-005 counts only the "began" and "ended" lines.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A missing card no longer restarts the agent every few seconds (Priority: P1)

A card-reader line's card is out of reach (pulled, the reader dropped off USB, or pcscd stopped) when the agent has to start. Today the agent exits and is restarted every 5 s for as long as the card is out. Instead, the supervisor recognises that the agent exited because the card is out of reach. It logs that once, and slows the restarts down to a backoff. When the card comes back, the agent is started promptly.

**Why this priority**: the agent loop is the larger flood (about 600 restarts an hour). It has a direct precedent in the supervisor's modem SIM handling, and it stands alone: it is useful even if the tunnel loop is left as it is.

**Independent Test**: classify a run of card-unreachable agent exits and check the restart delays grow to their cap, that a clean run resets them, and that modem exits are classified exactly as today. Live: unbind the reader with the tunnel up and restart the agent, then count restarts over 10 minutes.

**Acceptance Scenarios**:

1. **Given** a card-reader line whose card is out of reach, **When** its agent exits because it cannot reach the card, **Then** the supervisor logs one line naming the line and the reason, and restarts the agent after a delay that grows with each consecutive such exit, up to a cap.
2. **Given** consecutive card-unreachable exits, **When** the card becomes reachable again, **Then** the agent is started within the recovery bound (SC-003) instead of waiting out the rest of the current delay, and one recovery line is logged.
3. **Given** a card-reader line, **When** its agent exits for any other reason, **Then** the supervisor's handling is exactly what it is today.
4. **Given** a modem-backed line, **When** its agent exits for any reason, **Then** the classification, the `AT+CFUN` remedy, the backoff and the alerts are exactly what they are today.

---

### User Story 2 - A missing card no longer floods the carrier with failed logins (Priority: P1)

The ePDG tunnel of a card-reader line must be brought up, or brought back, while the line's card is out of reach. Today the supervisor re-initiates it about every 50 s, and each attempt is a failed EAP-AKA at the carrier. Instead, the supervisor knows the attempt cannot succeed. It makes new attempts only on a slow cadence while the card is out, and initiates promptly once the card is back.

**Why this priority**: every attempt is a failed authentication at the carrier, sent on the subscriber's behalf, which is the more serious kind of noise even though there are fewer of them. It is the same priority as US1 because the two loops run together, but it can ship separately.

**Independent Test**: drive the supervisor's recovery decisions with an injected clock and a card verdict of "out", then "back". Check that initiations are spaced by the slow cadence while the card is out and happen promptly once it is back. Live: unbind the reader and force a tunnel rebuild, then count IKE attempts over 10 minutes and time the recovery after rebind.

**Acceptance Scenarios**:

1. **Given** a card-reader line whose card is out of reach and whose tunnel is down, **When** the supervisor would re-initiate the tunnel, **Then** it initiates at most once per slow interval (FR-007) instead of on every tick, and logs once that it is holding the tunnel for the card.
2. **Given** that hold, **When** the card becomes reachable again, **Then** the tunnel is initiated within the recovery bound (SC-003) and the hold's end is logged once.
3. **Given** a card-reader line whose card goes out of reach while the tunnel and the agent are up, **When** nothing else happens, **Then** the supervisor changes nothing: it neither tears down the tunnel nor stops the agent. The agent's own handling from #233 applies.
4. **Given** a modem-backed line or a VoLTE line, **When** its tunnel or attachment needs recovery, **Then** the supervisor behaves exactly as today.

---

### User Story 3 - The operator sees one clear signal per card outage (Priority: P2)

An operator watching the logs, or receiving the tunnel-failure alert, during a card outage sees why the line is down: its card is out of reach. The alert says so, and the logs say so once when the outage begins and once more when it ends. They do not see a line per retry.

**Why this priority**: today the cause is buried in hundreds of restart and IKE lines, and nothing says "the card is out". It matters for diagnosis, but it is secondary to stopping the loops.

**Independent Test**: run a simulated outage of 10 minutes through the supervisor's decisions and count the operator-facing log lines it produces, and check the tunnel-failure alert's text with and without a card outage. Live: count the lines in the container log for the same outage, and read the alert on the web UI timeline (and on Discord, if that category is on).

**Acceptance Scenarios**:

1. **Given** a card outage, **When** it begins, **Then** the supervisor logs one line per affected line, naming it and stating that its card is out of reach and that restarts and tunnel attempts are slowed until the card is back.
2. **Given** that outage, **When** the card comes back, **Then** one line says so and how long the outage lasted.
3. **Given** that outage, **When** retries happen in between, **Then** each one is logged at most at debug level, or as a single line with the delay, never as a burst.
4. **Given** that outage, **When** the tunnel-failure alert is raised for the line, **Then** its text names the cause as the SIM card being out of reach. **Given** a tunnel failure with no card outage, or on a modem-backed line, **Then** the alert text is exactly as today.
5. **Given** the tunnel-failure alert was raised with the card cause, **When** the card comes back and the tunnel is up again, **Then** the alert recovers as it does today.

---

### Edge Cases

- **A wrong "card out" verdict.** For example, the supervisor's own PC/SC access is broken while charon and the agent could still reach the card. The line must never be held indefinitely: attempts continue, at least once per cap interval (FR-007), whatever the verdict. A successful attempt ends the hold.
- **Several card-reader lines.** Each line's verdict is about its own card, matched by IMSI. One line's missing card never slows another line whose card is present. When the readers themselves are gone (pcscd stopped, or every reader unplugged), every card-reader line is affected, and each is handled on its own.
- **The card comes back between two checks.** Recovery is bounded by the check interval plus one normal start (SC-003), not by the current backoff delay.
- **The card is out and the uplink is down at the same time.** The underlay guard's hold and the card hold must not double up or fight. A recovery that either of them holds is held; one that both allow goes ahead. Neither hold extends the other's cap.
- **The card flaps** (out, back, out within seconds). It is one outage, logged once, unless the line registers in between. A registration ends the incident and resets the backoff.
- **The agent exits for another reason during a card outage** (a crash, or a modem-lock timeout, which does not apply to card readers). The exit is classified by its own cause and does not reset the card backoff unless the run was clean (it registered).
- **A modem-backed line that shares the deployment with card-reader lines.** Untouched, including its SIM-recovery counters and alerts.
- **The card is out when the whole bridge starts.** Out of scope here (see Out of scope). The start-up MCC/MNC read and the agent's start-up exit behave as today, except that agent restarts are now paced (US1).
- **Checks touching a card that another program is using.** A card check must release the card without resetting it (#232), and must not disturb a sibling line's card while looking for its own.

## Requirements *(mandatory)*

### Functional Requirements

**Recognising a card outage**

- **FR-001**: When a card-reader line's agent exits because its card cannot be reached, the supervisor MUST recognise that cause unambiguously. The cause MUST be signalled by a marker that the agent and the supervisor share and that a test pins. It MUST NOT be recognised by matching free-form error text.
- **FR-002**: The supervisor MUST be able to tell, without involving the agent or the carrier, whether a card-reader line's own card (identified by the line's IMSI) can be reached now. The check MUST be local only (no network traffic, no authentication), MUST leave the card as it found it (no reset, #232), and MUST be bounded in time.
- **FR-003**: Only card-reader lines MUST ever be treated as having a card outage. Modem-backed lines and VoLTE lines MUST keep every current behaviour, including the modem SIM recovery (classification, `AT+CFUN` remedy, 15-minute give-up backoff, alerts).

**Pacing the agent**

- **FR-004**: While a card-reader line's agent keeps exiting because its card cannot be reached, the supervisor MUST restart it on a growing delay: starting at the current 5 s and doubling up to a cap (FR-007). It MUST NOT apply any SIM remedy, since a card reader has none.
- **FR-005**: A run in which the agent registers MUST end the incident and reset the delay. An exit for another cause MUST be handled as today.
- **FR-006**: When the card is found reachable again while the agent is waiting to be restarted, the agent MUST be started promptly, within the recovery bound (SC-003), rather than after the remaining delay.

**Pacing the tunnel**

- **FR-007**: While a card-reader line's card cannot be reached, the supervisor MUST NOT initiate that line's tunnel on every tick. It MUST make at most one initiation per slow interval, which starts at today's interval between attempts and doubles up to the same cap as FR-004. Attempts MUST never stop altogether: there is at least one per cap interval, so a wrong verdict costs at most one cap of extra downtime.
- **FR-008**: When the card is found reachable again, the supervisor MUST initiate the tunnel promptly, within the recovery bound (SC-003), and resume normal recovery.
- **FR-009**: The supervisor MUST NOT tear down a working tunnel, or stop a running agent, because a card is out of reach. While a card-reader line's card is out of reach, a recovery that would tear down an established IKE SA and rebuild it (which needs the SIM) MUST be held until the card is reachable again, not paced. The pacing of FR-007 applies to initiations when no IKE SA exists.
- **FR-010**: The card hold and the underlay guard's uplink hold MUST compose: a recovery is held while either holds it. Each keeps its own cap, and neither resets or extends the other.
- **FR-011**: Whether the card hold is a new cause in the underlay guard or a separate gate is a planning decision. It MUST be decided on these criteria:
  - (a) no change to what the existing uplink states mean in the control protocol, status and metrics;
  - (b) FR-010's composition is testable with an injected clock;
  - (c) the smaller change that meets FR-007–FR-010.

**Multiple lines**

- **FR-012**: Each card-reader line's outage state MUST be its own. A card that is out for one line MUST NOT hold, slow or log for another line whose card is reachable.

**Operator visibility**

- **FR-013**: The supervisor MUST log one line when a line's card outage begins (naming the line and stating that restarts and tunnel attempts are slowed until the card is back), and one line when it ends (with the outage's duration). A paced retry MAY keep one line, stating its delay. Held ticks and checks MUST NOT produce lines at the default log level, so there is never a burst. No line MAY carry an IMSI, IMEI or MSISDN.
- **FR-014**: While the supervisor holds a card-reader line for its card, the existing tunnel-failure alert for that line MUST name the cause ("SIM card out of reach") in its text, on the web UI timeline and on Discord alike. In every other case, including a modem-backed line, a VoLTE line, and a card-reader line whose tunnel fails for another reason, the alert text MUST be exactly as today. The alert's raise, recovery, thresholds and toggles MUST NOT change. The registration-loss alert MUST behave as today. This feature adds no new alert category, metric or status value (Clarification 2026-11-24).

**Unchanged**

- **FR-015**: The agent's in-process behaviour from #233 MUST be unchanged: the challenge retry, the wait for the card, and the uncounted renewal failures.

### Key Entities

- **Card outage (per card-reader line)**: whether the line's card is currently considered out of reach, when the outage began, the current retry delay, and when the next agent restart and tunnel initiation may happen. It is cleared by a registration, or by a check that finds the card reachable. The daemon must learn whether a line is held for its card, so that the tunnel-failure alert can name the cause (FR-014).
- **Agent exit cause**: the supervisor's classification of an agent exit. It gains a "card unreachable" cause for card-reader lines, alongside the existing modem SIM failure, modem stall and "other".
- **Card check**: a bounded, local look for the line's card by IMSI, whose answer is reachable, unreachable, or unknown (the check itself failed). Unknown never starts an outage on its own.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: During a 10-minute card outage on one card-reader line, the agent is restarted at most 15 times, down from about 100 today.
- **SC-002**: During the same 10-minute outage, the tunnel is initiated against the carrier at most 6 times, down from about 12 today, and at most once per cap interval once the backoff has reached its cap.
- **SC-003**: After the card comes back, the line is registered again within 30 s.
- **SC-004**: A card-reader line whose card is present is never held, whatever happens to other lines' cards. A modem-backed line's SIM recovery produces the same decisions as before, for the same sequence of exits.
- **SC-005**: A 10-minute outage produces exactly one "began" and one "ended" log line per affected line at the default level. The tunnel-failure alert raised during it names the card as the cause, and no alert outside a card outage changes its text.
- **SC-006**: A wrong "card out" verdict delays the line by at most one cap interval beyond what it would be today.
- **SC-007**: Every scenario in US1–US3 and every edge case listed has a deterministic test that runs with an injected clock in under a second.

## Assumptions

- **The cap is 2 minutes** for both agent restarts and tunnel initiations. Agent restarts start at 5 s and double: 5, 10, 20, 40, 80, 120, 120 s…, about 8 restarts in 10 minutes. Tunnel initiations start at today's interval (about 50 s) and double to the cap: about 5 in 10 minutes. This matches the agent's own renewal ladder (`RETRY_MAX_BACKOFF`, 120 s), and is short because recovery only means waiting for the card. The modem path's 15 minutes is for a fault that remedies could not fix. Planning may tune the cap within SC-001, SC-002 and SC-006.
- **The card is checked every 5 s while an outage is on**, matching the underlay guard's held polling and the existing PC/SC retries in the supervisor (`PCSC_PLMN_READ_RETRY_DELAY`, `PCSC_CONTEXT_RETRY_DELAY`). This is what keeps recovery prompt (SC-003) while attempts are slow.
- **Operator visibility is log lines plus the cause in the existing tunnel-failure alert** (Clarification 2026-11-24). There is no new alert category or metric, consistent with #233's decision (Clarification Q2 there). If the tunnel-failure alert is turned off for Discord, the cause still appears on the web UI timeline, because the timeline records every incident whatever its Discord setting.
- **An outage starts on evidence and is confirmed by the check.** The evidence is a card-unreachable agent exit, or a failed tunnel initiation on a card-reader line. A check that cannot run ("unknown") does not start an outage.
- **The local rig is the test bed.** Live verification repeats #233's recipe: unbind the reader from USB via a throwaway privileged container, and force a tunnel rebuild (see the memory note `live-card-outage-test-recipe`). The carrier side of a failed EAP-AKA is observed only through charon's log.

## Out of Scope

- **Making the agent wait in-process for the card at start-up instead of exiting.** It would remove the agent loop but not the tunnel loop, and it changes the agent's start-up contract with the supervisor. Deferred.
- **The card being out when the whole bridge starts** (the start-up MCC/MNC read retries for about 70 s, then skips the line until the next restart). This is existing behaviour with its own trade-offs. A separate issue may be worth opening.
- **VoLTE lines, modem-backed lines, and any change to modem SIM recovery.**
- **charon's own retry behaviour** inside a single initiation.
