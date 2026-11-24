# Feature Specification: A Card-Reader Line Rides Out an Unreachable SIM

**Feature Branch**: `162-pcsc-card-unreachable`  
**Created**: 2026-11-24  
**Status**: Draft  
**Input**: GitHub issue #233, the resilience half of #232. PR #234 (`161-pcsc-leave-card`) removed the resets the bridge caused itself. This feature covers a card that is out of reach for real: a reader that drops off USB/IP, a pulled card, a command that hangs, or another program that resets the card.

## Background

A `pcsc_reader` line keeps its SIM in a PC/SC smart-card reader, often reached over USB/IP across the internet. Its IMS registration is refreshed in place on the live connection. When the network challenges a refresh, the SIM answers in the background while calls carry on.

Today every failure to get that answer is treated the same way: the line abandons the registration it still holds and re-registers in full on a new connection (new TCP connection, new Gm security associations, new Contact port, new reg-event subscription). In #232 the card was reset by another client; the registration held for another ten minutes, and the card answered again two seconds later. A SIM that does not answer at all is waited for up to ten minutes, by which time the registration has lapsed.

When a full registration then fails because the reader cannot be reached, each failure counts toward the renewal budget. After six the agent exits so the supervisor can power-cycle the SIM. That remedy works on a modem; it does nothing for a reader. The restarted agent finds the same reader missing and exits again.

For a line whose SIM sits behind a network link, a card that is briefly out of reach is expected and is not a failed SIM. A full registration also cannot help while the card is out, because it needs the card too.

Today every card failure is reported as free text. Several existing classifiers sort registration failures by matching words in that text. Telling "the card is out of reach" apart from "the card answered with an error" needs a distinct, typed failure, not one more text match.

## Clarifications

### Session 2026-11-24

- Q: Should a long reader outage ever make the agent exit? → A: Never. Card-unreachable failures are retried on the backoff indefinitely, and the registration-loss alert is the escalation. An exit would fix nothing: the supervisor's remedy is `AT+CFUN`, which a reader cannot use, and every card attempt opens a fresh PC/SC context.
- Q: How visible should "waiting for the card" be while the registration is still valid? → A: Logs and the `last_refresh:` value in `vowifi-status` only. No new status line, metric or alert. The existing registration-loss alert covers an outage that outlasts the registration.
- Refinement from planning (research.md D3): a card command that hangs cannot be cut short, and while it hangs it blocks the card for every program, so a second, parallel attempt would hang behind it. A hung attempt therefore uses up the rest of the retry window, and the line then moves to waiting for the card (User Story 2). FR-004's bound applies to the background attempts and checks. A full registration's card access stays synchronous, as today, and is still bounded by the existing renewal watchdog. US1 scenario 4 and FR-004 were reworded to match.
- Refinement from planning (research.md D5): the safety margin forces a full registration only with no call in progress, as the lapse rule already does. During a call the line keeps waiting, and refreshes in place once the card is back, which the existing rule for a lapse during a call already allows. FR-013 and US2 scenarios 4 and 5 were reworded to match.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A brief card outage during a challenged refresh goes unnoticed (Priority: P1)

An operator runs a card-reader line. The network challenges an ordinary registration refresh. While the SIM is being asked for the answer, the card is briefly out of reach: reset by another program, or the USB/IP link drops for a few seconds. The line asks the card again with the same challenge, gets the answer, and completes the refresh in place. The registration, the connection and calls are untouched.

**Why this priority**: This is the exact failure in #232 and the most common one: a card out of reach for seconds.

**Independent Test**: With a stand-in card that reports "card reset" twice and then answers, a challenged refresh completes in place, with no full registration and no new connection.

**Acceptance Scenarios**:

1. **Given** a card-reader line whose refresh has been challenged, **When** the card reports it has been reset, then answers on the next attempt, **Then** the challenge is answered and the refresh completes in place.
2. **Given** the same, **When** the card is unreachable for several attempts but answers within the retry window, **Then** the refresh completes in place, with no full registration.
3. **Given** the card actually answered before the link dropped, so the retried challenge produces a sequence mismatch, **When** the retry completes, **Then** the existing resynchronisation path handles it as for any sequence mismatch.
4. **Given** a card command that does not return (a hung link), **When** the retry window ends, **Then** the attempt is abandoned and counts as "card unreachable", and the line moves to waiting for the card (User Story 2). No second attempt is started while the hung one is outstanding.
5. **Given** the card answers with an error of its own (authentication failure, error status), **When** that answer arrives, **Then** it is handled exactly as today, with no retry.
6. **Given** a call is in progress, **When** any of the above happens, **Then** the call carries on undisturbed.

---

### User Story 2 - A longer card outage keeps the registration (Priority: P1)

The card stays out of reach past the retry window, for example while a reader is re-plugged or the USB/IP link is restored. The line gives up that challenge but keeps its registration, which is still valid. It checks the card locally, without network traffic, until the card answers. Then it sends a fresh refresh in place, which brings a fresh challenge. It re-registers in full only when the registration is about to lapse, or has lapsed.

**Why this priority**: Without it, any outage longer than the retry window still throws away a valid registration and its connection.

**Independent Test**: With a stand-in card that is unreachable past the retry window and then comes back, the line keeps its registration (no full registration), sends no REGISTER while the card is out, and sends one fresh in-place REGISTER when the card is back.

**Acceptance Scenarios**:

1. **Given** a challenged refresh whose card stays unreachable past the retry window, **When** the window ends with the registration still valid, **Then** the challenge is abandoned, the registration is kept, and no full registration is scheduled.
2. **Given** the line is waiting for the card, **When** the scheduled "refresh due" check runs, **Then** no new refresh is sent while the card is out.
3. **Given** the line is waiting for the card, **When** a local check finds the card answering, **Then** one fresh in-place refresh is sent.
4. **Given** no call is in progress, **When** the remaining registration lifetime is under the safety margin or the registration has lapsed, **Then** the line stops waiting and re-registers in full.
5. **Given** the line is waiting for the card with no call in progress, **When** the remaining lifetime drops under the safety margin while the card is still out, **Then** the line hands over to full registration, which is then retried as in User Story 3. During a call it keeps waiting, and refreshes in place once the card is back.
6. **Given** the line is waiting for the card, **When** anything else ends the registration or forces a full one (a full registration for another reason, P-CSCF failover, the Gm connection being declared dead, the network deregistering the line, a zero-lifetime grant, agent shutdown), **Then** the waiting state ends and does not survive into the new registration.
7. **Given** the line is waiting for the card, **When** calls arrive or are placed, **Then** they are served as usual on the still-valid registration.

---

### User Story 3 - A reader outage never makes the agent exit (Priority: P2)

The registration has lapsed and the card is still out of reach. Every full registration attempt fails because the card cannot be reached. The line keeps retrying on the normal backoff for as long as it takes, and the operator is told the registration is lost. It never exits the agent for this reason, because restarting buys a modem SIM power-cycle that a reader cannot use.

**Why this priority**: This is the related loop in #232. It is lower than User Stories 1 and 2 because the line is already unregistered by then; this story stops the restart churn and lets the line come back by itself as soon as the card does.

**Independent Test**: Drive a card-reader line through more full registration failures than the renewal budget allows, all caused by an unreachable card. The agent does not exit, retries stay on the backoff ladder, and the registration is reported as lost.

**Acceptance Scenarios**:

1. **Given** a card-reader line with a lapsed registration, **When** a full registration fails because the card cannot be reached (when opening it, or while answering the challenge), **Then** it is retried on the normal backoff and does not count toward the renewal budget.
2. **Given** the same line, **When** a full registration fails for any other reason (rejected, no answer from the P-CSCF, transport failure), **Then** it counts toward the budget exactly as today.
3. **Given** a card-unreachable failure, **When** it is recorded, **Then** the P-CSCF is not marked as failed.
4. **Given** a modem-backed line, **When** any full registration fails, **Then** it counts toward the budget exactly as today, so the supervisor's SIM recovery still gets its chance.
5. **Given** a card-reader line that keeps failing because the card is out, **When** the operator looks at status, metrics or alerts, **Then** the registration is shown as lost, and the reason says the card is unreachable.
6. **Given** card-unreachable failures and other failures interleave, **When** the budget is counted, **Then** only the other failures count, and one success clears the count as today.

---

### User Story 4 - The operator can tell a card outage from a failed SIM (Priority: P3)

The operator reads the logs and status of a card-reader line. Each step is visible: the challenge being retried, the line giving up the challenge and waiting for the card, the card back, and a full registration started because of the margin or a lapse. The last refresh outcome says "waiting for the card", not a generic "error".

**Why this priority**: It does not change behaviour, but without it an operator cannot tell this feature working from a stuck line.

**Independent Test**: Run the scenarios of User Stories 1 to 3 and check that each transition produces its log line and that status shows the waiting state distinctly.

**Acceptance Scenarios**:

1. **Given** each transition above, **When** it happens, **Then** exactly one log line names it, with no identifiers beyond what existing registration logs carry.
2. **Given** the line is waiting for the card, **When** status is read, **Then** the last refresh outcome reads as waiting for the card.

### Edge Cases

- **The card is reset again during a retry**: it is one more unreachable attempt inside the same window.
- **A link that flaps so the card keeps answering just before each drop**: each retry yields a sequence mismatch and uses a resynchronisation. Once the existing resynchronisation limit is spent, the line falls back to full registration as today.
- **A hung card command**: the worker attempt cannot be cut short, and it blocks the card for every program while it hangs. It is abandoned after its bound, and its late answer is discarded. No new attempt or check starts while an abandoned one is still outstanding, so hung attempts cannot pile up.
- **The card comes back as a different card** (another IMSI in the reader): the line's card is still unreachable. It is never matched to the wrong subscriber.
- **The card is back but its answer is a card error, not unreachable**: this follows today's handling (full registration), not the waiting path.
- **The fresh in-place refresh after the card is back is not challenged** (the carrier accepts it with cached credentials): it completes as an ordinary refresh, and the waiting state ends.
- **The fresh in-place refresh is challenged and the card drops out again**: User Stories 1 and 2 apply again from the start.
- **The registration lapses, or falls under the margin, while the line waits and a call is in progress**: the existing rule for a lapse during a call applies (an in-place refresh is still allowed while a call holds the line), so the line keeps waiting. With no call, it goes to full registration.
- **A network-forced full registration while the card is out**: it fails because the card is out, is retried on the budget-exempt backoff (User Story 3), and the waiting state is gone.
- **Shutdown while a card attempt or check is outstanding**: shutdown never waits for it.
- **Modem-backed lines**: none of this applies. Their SIM failures, timeouts and budget are unchanged.

## Requirements *(mandatory)*

### Functional Requirements

**Typed "card unreachable" failure**

- **FR-001**: A card-reader line's card access MUST report a distinct "card unreachable" failure for these conditions: the card was reset, removed, absent, unpowered or unresponsive; the reader is unavailable or there are no readers; the smart-card service is stopped or unavailable; no reader holds a card with this line's IMSI.
- **FR-002**: A response from the card itself (authentication failure, error status word, sequence mismatch) MUST NOT be reported as "card unreachable". It keeps today's handling.
- **FR-003**: The "card unreachable" failure MUST be recognisable by its type, not by its wording. Every existing failure message that existing classifiers depend on MUST keep being classified as before. The new failure's message MUST NOT match any existing classifier by accident, for example as a network, timeout or transport failure.
- **FR-004**: A background card attempt (a challenge retry or a local check) that does not return within its bound MUST count as "card unreachable". A full registration's card access stays synchronous and keeps today's renewal watchdog bound.

**Retrying the challenge (card-reader lines)**

- **FR-005**: When answering a challenge to a refresh meets "card unreachable", the line MUST reopen the card and put the same challenge to it again, repeatedly, within a bounded retry window (default 30 s), with an interval between attempts.
- **FR-006**: The retry MUST run off the dispatch loop, so calls, notifications and other signalling keep being served throughout.
- **FR-007**: For a card-reader line, the deadline by which an unanswered challenge is given up MUST be the retry window, not today's ten-minute limit.
- **FR-008**: A sequence mismatch produced by a retried challenge MUST be handled by the existing resynchronisation path and its existing limit. A resynchronisation's fresh challenge gets its own retry window.

**Keeping the registration while the card is out (card-reader lines)**

- **FR-009**: When the retry window ends with "card unreachable" and the registration is still valid, the line MUST abandon the challenge, undo anything staged for it, keep the registration and its connection, and MUST NOT schedule a full registration.
- **FR-010**: While waiting for the card, the line MUST check the card locally, without any network traffic and without running authentication, on a backoff (default: 5 s, doubling, capped at 60 s). Each check MUST be bounded and MUST run off the dispatch loop.
- **FR-011**: While waiting for the card, the line MUST NOT start an in-place refresh or a full registration, except as FR-013 and FR-014 allow. A refresh falling due MUST NOT re-fire on every pass.
- **FR-012**: When a check finds the card answering and the remaining lifetime is at least the safety margin (default: the existing 60 s refresh floor), the line MUST send one fresh in-place refresh and stop waiting.
- **FR-013**: With no call in progress, when the remaining lifetime falls under the safety margin or the registration lapses, the line MUST stop waiting and re-register in full, whether or not the card has answered. During a call it MUST keep waiting, and refresh in place once the card answers.
- **FR-014**: The waiting state MUST end on every path that ends or replaces the registration: card back (FR-012), margin or lapse (FR-013), a full registration forced for another reason (Gm connection declared dead, P-CSCF failover, zero-lifetime grant, a later refresh failure that needs one), network deregistration by reg-event, and agent shutdown. It MUST NOT carry into a new registration.
- **FR-015**: At most one card attempt or check MAY be outstanding at a time, counting abandoned ones that have not returned yet. Shutdown MUST NOT wait for an outstanding one.

**Renewal budget (card-reader lines)**

- **FR-016**: On a card-reader line, a full registration that fails because the card is unreachable (when opening the card or while answering its challenge) MUST be retried on the normal backoff ladder, MUST NOT count toward the renewal budget that exits the agent, and MUST NOT mark the P-CSCF as failed. There is no separate cap or budget for these failures: they are retried indefinitely and never make the agent exit. The registration-loss reporting in FR-018 is the escalation.
- **FR-017**: Every other full registration failure on a card-reader line MUST count toward the budget as today. A success MUST clear the count as today.
- **FR-018**: Registration-loss reporting (status, metrics, alerts) MUST still show a card-reader line as unregistered while it retries, with a reason that says the card is unreachable.

**Scope and observability**

- **FR-019**: Modem-backed lines MUST behave exactly as before. Their challenge timeout, fallback to full registration, failure messages and renewal budget are unchanged, even if a modem failure resembles "card unreachable".
- **FR-020**: Each transition MUST log one line: retrying the challenge (with attempt count), giving up the challenge and waiting for the card, the card back, and a full registration because of the margin or a lapse. Logs MUST carry no subscriber identifiers beyond what existing registration logs carry.
- **FR-021**: The last-refresh outcome shown in status (the `last_refresh:` line of `vowifi-status`) MUST say the line is waiting for the card, distinct from a generic error. No new status line, metric or alert is added for the waiting state. An outage that outlasts the registration is reported by the existing registration-loss alert (FR-018).
- **FR-022**: The retry window, retry interval and check backoff MUST be adjustable by tests, so the behaviour can be tested deterministically without real waits. Operator configuration is not required.

### Key Entities

- **Card-unreachable failure**: a failure of a card-reader line's card access meaning "the card could not be reached", as opposed to "the card answered with an error". Recognised by type.
- **Challenge retry window**: the bounded period after a challenge during which the same challenge is put to the card again whenever it is unreachable.
- **Waiting-for-card state**: a still-valid registration with no outstanding challenge, waiting for local checks to find the card. It has a backoff and ends on every exit path in FR-014.
- **Safety margin**: the minimum remaining registration lifetime below which the line re-registers in full instead of refreshing in place.
- **Renewal budget**: the count of consecutive failed full registrations after which the agent exits so the supervisor can recover the line. Card-unreachable failures on card-reader lines are not counted.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: Replaying #232 (a challenged refresh during which the card is unreachable for 2 s) completes the refresh in place, with zero full registrations and zero new Gm connections.
- **SC-002**: A card outage of any length shorter than the registration's remaining lifetime minus the safety margin ends with the original registration and connection still in use, and zero full registrations.
- **SC-003**: While the card is out and the registration still valid, zero REGISTER requests are sent. After the card is back, exactly one in-place REGISTER is sent.
- **SC-004**: A card-reader line whose card stays unreachable through at least twice the renewal budget's number of failures never exits, and is shown as unregistered throughout.
- **SC-005**: The line re-registers within one backoff step after the card returns from an outage that outlasted the registration.
- **SC-006**: Every existing automated test of modem-backed lines, of non-card registration failures, and of failure classification passes without changing its expectations.
- **SC-007**: Each scenario under User Stories 1 to 3 is covered by a deterministic automated test that finishes in under a second, without hardware.

## Assumptions

- **The network waits well over 30 s for a challenge answer.** 30 s is used as the retry window to stay inside it. If live evidence shows a carrier waiting less, the default is shortened.
- **Retrying the same challenge is safe.** The smart-card service reports a reset before the command reaches the card. If the card did answer before the link dropped, its sequence number has moved on, and the retry produces a sequence mismatch, which the network resolves with a fresh challenge.
- **A local check is preferred over resending the refresh.** Each resend draws a fresh network challenge and uses up an authentication vector while the card cannot answer it.
- **Opening the card and confirming it holds this line's IMSI is enough of a check.** It needs no authentication and does not change the card's state.
- **The safety margin reuses the existing 60 s refresh floor.** It is enough for a full registration on the reference setup, about 5 s.
- **Card-reader lines are VoWiFi only.** VoLTE lines never use a reader.
- **A challenged refresh cannot be exercised live on the local rig**, because Vodafone accepts in-place refreshes without a challenge. User Stories 1 and 2 are verified by automated tests. Live checks are limited to: no regression in registration or refresh on the card-reader rig; mapping of real reader conditions (card pulled, reader removed, card reset by another program) to "card unreachable" on the reader host; and User Story 3 by removing the reader while the registration is lapsed.
- **Out of scope**:
  - A reader missing when the agent starts. The agent still exits and the supervisor restarts it, with no SIM power-cycle and no cap. That loop is untidy but recovers by itself.
  - The IKE tunnel. While it is up it only rekeys, which does not touch the SIM. Rebuilding it is retried indefinitely by the supervisor.
  - VoLTE lines.
  - Operator configuration of the windows and backoffs.
