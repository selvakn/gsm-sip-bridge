# Feature Specification: Stop the Bridge Resetting a Shared Card-Reader SIM

**Feature Branch**: `161-pcsc-leave-card`  
**Created**: 2026-11-24  
**Status**: Draft  
**Input**: GitHub issue #232 (a card-reader line re-registers in full when its SIM is out of reach for two seconds during a refresh), PR 1 of 2. The resilience half of the issue (retrying the challenge, keeping the registration while the card is out, the renewal-budget exemption, and a typed "card unreachable" error) is #233 and out of scope here.

## Background

A `pcsc_reader` line keeps its SIM in a PC/SC smart-card reader instead of a modem. Several programs share that card: the line's IMS agent (registration, and every background answer to a network challenge), the supervisor's start-up read of the home network code, the operator's `pcsc-list` tool, other card-reader lines probing every reader for their own card, and charon's tunnel authentication.

In #232 a challenged registration refresh failed with "The smart card has been reset, so any shared state information is invalid". The line abandoned a registration that held for another ten minutes and re-registered in full. Triage on a real reader and SIM (`tmp/issue232-repro`) found that **the bridge resets the card itself**. Each time any of its card connections is released, the card is reset for every other program connected to it. That program's next command then fails exactly as in the issue.

| While one program holds the card, another… | First program's next command |
|---|---|
| does nothing (control) | succeeds |
| runs the bridge's PLMN read for this line's card | **fails: card has been reset** |
| runs the bridge's probe for a *different* line's card | **fails: card has been reset** |
| releases its connection *leaving* the card | succeeds |
| (first program holds exclusive access) resets the card 8 times | succeeds; the resets do not disturb it |

charon's tunnel authentication always releases the card as it found it, so it is not a cause.

A second, latent defect was found on the way. The line answers a challenge by selecting the SIM's USIM application and then asking the card to authenticate. The authentication only works while that application is still selected, but the two steps run without exclusive access. Another program's reset or selection can land between them. Code comments claim the authentication step depends on no prior selection, and that is wrong.

## Clarifications

### Session 2026-11-24

- Refinement from planning (research.md D4): the USIM application's identity is discovered once, inside the exclusive window that already identifies the card when it is opened, and reused. Each challenge answer re-selects it and authenticates inside its own window. FR-003, FR-004, US3 and SC-002 were reworded to match. Rediscovering on every answer would add about six card commands per answer over a slow link and protect nothing extra, because the card's application list does not change while it stays in the reader.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - The bridge never disturbs another program's card session (Priority: P1)

An operator runs a card-reader line. Whatever the bridge does with the card (registering, answering a challenge, reading the home network code at start-up, listing readers), every other program connected to the same card carries on as if the bridge were not there.

**Why this priority**: This is the root cause of #232. The bridge's own routine card use breaks the line's own in-flight challenge answer, and it can break charon's tunnel authentication and other lines' work.

**Independent Test**: Hold a connection to the card in one program. Run each of the bridge's card-using commands in another program while the first waits. Then send a command from the first program. It succeeds every time.

**Acceptance Scenarios**:

1. **Given** a program holding a shared connection to the line's card, **When** the bridge's PLMN read for that card runs and finishes, **Then** the first program's next command succeeds.
2. **Given** a program holding a shared connection to the line's card, **When** the bridge's reader listing runs and finishes, **Then** the first program's next command succeeds.
3. **Given** the bridge has finished a registration or challenge answer and released the card, **When** charon or another line next uses the card, **Then** that use succeeds without seeing a reset.
4. **Given** a bridge card operation fails part-way (unreadable card, wrong card, error reply), **When** its connection is released, **Then** the card is still left as found.

---

### User Story 2 - One line's search for its card does not disturb other lines' cards (Priority: P1)

An operator runs two or more card-reader lines, each with its own reader. When a line looks for its own card, it checks every reader and moves on from the ones holding someone else's card. That check leaves those other cards exactly as it found them.

**Why this priority**: With several lines, refreshes are scheduled from similar grant times and often run together. Today each line's search resets every other line's card, which is the most likely way two lines break each other's challenge answers.

**Independent Test**: Hold a connection to the card in one program. Run the bridge's card search for an IMSI the card does not hold. The search reports no match, and the first program's next command succeeds.

**Acceptance Scenarios**:

1. **Given** a program holding a shared connection to a card, **When** a bridge line searches the readers for a different IMSI and rejects this card, **Then** the first program's next command succeeds.
2. **Given** several readers, **When** a line's search rejects some and keeps its own, **Then** the rejected cards are left as found and the kept card is used as today.

---

### User Story 3 - A challenge answer cannot be split by another program (Priority: P2)

When a card-reader line answers an authentication challenge, at initial registration or for a refresh, the steps that depend on each other run as one exclusive unit: selecting the USIM application, then authenticating (including fetching the card's answer). The application's identity is the one found when the line opened the card. Another program's reset or selection either completes before the unit starts or waits until it ends. It can never land in the middle.

**Why this priority**: User Stories 1 and 2 remove the bridge's own resets. This story covers programs outside the bridge's control and the latent wrong-selection hazard. It is lower priority because no field failure has been attributed to it yet.

**Independent Test**: Use a stand-in card that records the order of exclusive-access start, commands and exclusive-access end. Answering a challenge puts the application selection and the authentication inside the same exclusive window.

**Acceptance Scenarios**:

1. **Given** a card-reader line answering a challenge, **When** the answer runs, **Then** application selection, authentication and fetching the answer all happen inside one exclusive window.
2. **Given** another program trying to reset or select the card during that window, **When** the window ends, **Then** the answer has succeeded and the other program's action happens afterwards.
3. **Given** a challenge answered with a resynchronisation (sequence mismatch), **When** the network sends its fresh challenge, **Then** that answer is its own exclusive window as well.
4. **Given** a modem-backed line, **When** it answers a challenge, **Then** its behaviour is unchanged. Its SIM is reached over the modem, which has no such exclusive access.

### Edge Cases

- **The card is unreachable when exclusive access is requested** (reader gone, card pulled, card already reset): the answer fails as an ordinary SIM error, reported as today. Recovering from it is #233.
- **Releasing the connection fails** (the reader vanished first): the failure is ignored quietly. It must not crash, hang, or turn a successful operation into a failure.
- **A slow card or slow link** (a reader over USB/IP across the internet): other programs wait for the exclusive window, which lasts only as long as the card takes to answer (about a second in #232's log). They wait rather than fail.
  - charon's tunnel authentication takes exclusive windows of its own, so it queues behind ours.
  - Observed on the real reader: a program with no exclusive window of its own, connecting while ours was held, was held at connect for about 2.5 s until ours ended, then its command succeeded.
- **A card or link that hangs inside the window**: other programs wait for as long as it hangs. That is no new exposure, because the reader serves one command at a time, so a hung command blocks everyone with or without the window. Bounding a hung SIM is #233.
- **A bridge operation panics or errors out mid-way**: the connection is still released leaving the card, on every exit path.
- **Non-matching readers that cannot be read at all**: they are skipped as today and released leaving the card.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Every card connection the bridge opens MUST be released in a way that leaves the card as found, never resetting it. This covers line registration, background challenge answers, the agent's start-up home-network read, the supervisor's PLMN read, and the reader listing. It holds on every exit path: success, error, panic, early return.
- **FR-002**: When a line searches the readers for its own card, every reader it rejects (another IMSI, unreadable card, failed probe) MUST be released leaving the card as found.
- **FR-003**: On a card-reader line, answering an authentication challenge MUST run USIM application selection, authentication and fetching the card's answer inside one exclusive window on the card. This applies at initial registration and for background re-authentication. The application's identity may be the one discovered (inside its own exclusive window) when the card was opened. No multi-step selection on a card-reader line may run outside an exclusive window.
- **FR-004**: Each further challenge answered after a resynchronisation MUST be its own exclusive window, including selection, and MUST NOT rely on a selection made in an earlier window.
- **FR-005**: Modem-backed lines MUST behave exactly as before. Their challenge path, card commands and error messages are unchanged.
- **FR-006**: Existing error wording from card operations MUST be preserved where code or operators may rely on it, such as the "PC/SC transmit failed" prefix. New failure points (for example, failing to start the exclusive window) MUST use wording consistent with the existing messages.
- **FR-007**: Comments and documentation that say authentication needs no prior application selection, or that the card's release disposition does not matter, MUST be corrected.
- **FR-008**: A failure to release a connection MUST NOT change the outcome of the operation that used it.

### Key Entities

- **Card connection**: one program's shared connection to the SIM in a reader. Its release either leaves the card as found or resets it for every other connection.
- **Exclusive window**: a period during which only one connection may send commands to the card. Other programs' commands and resets wait until it ends.
- **Challenge answer**: the sequence (select the USIM application, authenticate with the network's challenge, fetch the answer) that produces the registration's credentials and session keys.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: In the two-process reproduction against the real reader, each scenario where a bridge command runs alongside a held connection (PLMN read for this card, search for another IMSI, reader listing) ends with the held connection's next command succeeding in 10 out of 10 runs. Today it fails in every run of the first two.
- **SC-002**: Tests show that every challenge answer on a card-reader line, including after a resynchronisation, goes through one path that selects the USIM application and authenticates as a single sequence inside one exclusive window.
- **SC-003**: Every existing automated test for modem-backed lines passes unchanged, with no edits to its expectations.
- **SC-004**: A card-reader line still registers end to end on the local rig with the fixed build, in no more time than before (about 5 s on the reference setup).

## Assumptions

- Leaving the card as found is safe for every bridge use. Each bridge sequence selects what it needs before reading, so none relies on the card starting from a fresh reset. charon already releases this way.
- The exclusive window is held only for the duration of the card's answer, so blocking other programs briefly is acceptable. Observed: about 1 s over USB/IP in #232's log, milliseconds on a local reader.
- The live evidence for exclusive access was observed on the local pcsc-lite. The mechanism inside pcsc-lite is not relied on beyond that observed behaviour.
  - 8 resets by another program while the window was held: none disturbed it.
  - Another program's commands, with or without a window of its own: they waited until it ended, then succeeded.
- A live end-to-end test of a *challenged* refresh is not possible on the local rig, because Vodafone accepts in-place refreshes without a challenge. Challenge-path behaviour is covered by automated tests. Root-cause verification uses the two-process reproduction.
- The typed "card unreachable" error, retrying the challenge, keeping the registration while the card is out, and exempting reader outages from the renewal budget are in #233.
