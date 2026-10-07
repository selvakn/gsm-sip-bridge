# Feature Specification: Security-agreement headers on every Gm request

**Feature Branch**: `082-sec-agree-all-gm-requests`
**Created**: 2026-10-07
**Status**: Draft
**Input**: User description: "VoWiFi: send sec-agree headers (Require, Proxy-Require, Security-Verify) on all requests sent over the negotiated Gm security association — follow-up to #95 / PR #99 (issue #100)."

## Why this exists

[GitHub issue #100](https://github.com/selvakn/gsm-sip-bridge/issues/100)
follows up #95 / PR #99. RFC 3329 §2.3.1 and TS 24.229 §5.1.1.5.1 require every
request sent over a negotiated Gm security association to carry
`Require: sec-agree`, `Proxy-Require: sec-agree` and a `Security-Verify` that
echoes the carrier's full `Security-Server` list. PR #99 added this to the
outbound INVITE and the SMS delivery-report MESSAGE only. Every other request
still goes out bare:

- in-dialog: BYE, UPDATE (session-timer refresh), PRACK, ACK, CANCEL;
- background: registration-event SUBSCRIBE, OPTIONS keepalive, un-REGISTER.

A carrier that enforces the mechanism strictly (MEO is known to) may answer
such requests with 494. The expected symptom there is a call that connects but
then cannot be hung up or refreshed, plus failing keepalive and subscription.
This has **not** been observed on any enforcing carrier. Jio and Vodafone
accept these requests without the headers today (a Jio BYE got 200 OK), so
this is a compliance and robustness fix, not a live outage.

Two maintenance problems make it easy to get wrong again:

- The header block (and its explanatory comment) is copy-pasted between the
  INVITE and MESSAGE builders.
- The echoed value is stored as a separate field next to the Gm security
  state, so the two can disagree (for example after a re-registration).

And nothing tests the path from registration to the requests. A regression to
a partial echo, which Jio rejects with 494 on REGISTER, would go unnoticed.

## Clarifications

### Session 2026-10-07

- Q: Which echo does an in-dialog request use when the registration renewed mid-call? → A: The echo of the current registration (option A). Planning found renewal is deferred during calls, so a call's dialog state can capture the echo at setup, like it already does for its local address and transport, with identical behaviour.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Calls hang up and refresh on a strict carrier (Priority: P1)

An operator on a carrier that enforces security agreement places or receives a
VoWiFi call. The call connects, the session timer refreshes mid-call, and a
hang-up from either side completes cleanly.

**Why this priority**: This is the failure the issue predicts: a connected
call that cannot be ended or refreshed.

**Independent Test**: Place a call through a registration with a negotiated
Gm SA, then check that BYE, UPDATE and PRACK on the wire each carry the three
headers with the full `Security-Server` echo.

**Acceptance Scenarios**:

1. **Given** a registration with a negotiated Gm SA, **When** the bridge sends
   BYE, UPDATE, PRACK or a 2xx-ACK in a dialog, **Then** the request carries
   `Require: sec-agree`, `Proxy-Require: sec-agree` and a `Security-Verify`
   equal to the full `Security-Server` list.
2. **Given** the same registration, **When** the bridge sends CANCEL for a
   pending INVITE, **Then** it carries the `Security-Verify` echo and its
   `Require`/`Proxy-Require` as the Requirements section specifies.

---

### User Story 2 - Background signalling stays accepted (Priority: P2)

A registered line stays healthy for hours: the registration-event
subscription is established and renewed, the OPTIONS keepalive succeeds, and
de-registration on shutdown is accepted.

**Why this priority**: Failing keepalive or subscription degrades or drops the
registration, but it is less immediately visible than a stuck call.

**Independent Test**: With a negotiated Gm SA, capture SUBSCRIBE, OPTIONS and
un-REGISTER and check each carries the three headers.

**Acceptance Scenarios**:

1. **Given** a negotiated Gm SA, **When** the bridge sends SUBSCRIBE, an
   OPTIONS keepalive or an un-REGISTER, **Then** each carries the three
   headers with the full echo.

---

### User Story 3 - No change where no SA is negotiated (Priority: P1)

An operator whose registration negotiated no Gm SA (sec-agree off, or a
carrier that rejects it) sees byte-for-byte the same requests as today.

**Why this priority**: Vodafone India rejects sec-agree outright, and Jio is
sensitive to header changes. Regressing either is worse than the gap being
fixed.

**Independent Test**: With no Gm SA, build every request type and check none
mentions `sec-agree` or `Security-Verify`.

**Acceptance Scenarios**:

1. **Given** a registration with no negotiated Gm SA, **When** any request is
   built, **Then** it carries none of the three headers.

---

### User Story 4 - Regressions are caught by tests (Priority: P2)

A developer who changes registration or a request builder gets a failing test
if the echo is dropped, truncated, or goes stale.

**Why this priority**: The partial-echo regression is already known to be
plausible and would be silent today.

**Independent Test**: Feed a synthetic multi-entry `Security-Server` list
through registration and assert every request type carries the complete list.

**Acceptance Scenarios**:

1. **Given** a mock P-CSCF offering several `Security-Server` entries,
   **When** registration completes and each request type is built, **Then**
   each carries the verbatim full list.
2. **Given** a re-registration that negotiates a different `Security-Server`
   list, **When** the next request is built, **Then** it carries the new list
   and never the old one.

---

### Edge Cases

- A re-registration (renewal) renegotiates the SA: later requests must use the
  new echo, never the previous one.
- A call is in progress while the registration renews: renewal is deferred
  while a call is active (`MaintenancePolicy`), so a call never straddles a
  renegotiation. Its in-dialog requests carry the echo of the registration the
  call was set up under, which is the current one for as long as the call lasts.
- Registration fails or is torn down: no stale echo may leak into requests
  built afterwards.
- A carrier sends several `Security-Server` entries: the whole list is echoed
  verbatim, not only the entry we selected (a partial echo is rejected by Jio
  with 494 on REGISTER).
- Requests that do not travel over the Gm SA (for example a response, or
  traffic to a non-IMS peer) must not gain the headers.
- Messages grow by roughly 100 bytes; this must not push a request over the
  transport's size limit in realistic cases.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: When a Gm SA is negotiated, every request the bridge sends over
  it MUST carry `Require: sec-agree`, `Proxy-Require: sec-agree` and a
  `Security-Verify` containing the carrier's full `Security-Server` list
  verbatim. This covers INVITE, MESSAGE, BYE, UPDATE, PRACK, SUBSCRIBE,
  OPTIONS keepalive, un-REGISTER and the ACK for a 2xx.
- **FR-002**: CANCEL MUST carry the `Security-Verify` echo. It mirrors the
  INVITE it cancels, so it carries `Require`/`Proxy-Require` as that INVITE did.
- **FR-003**: The ACK for a non-2xx final response belongs to the INVITE
  transaction and MUST mirror the INVITE's headers rather than add new ones.
- **FR-004**: When no Gm SA is negotiated, no request MAY carry any of the
  three headers; output MUST be unchanged from today.
- **FR-005**: The three-header block MUST be produced in exactly one place and
  used by every request type above; no request type may hand-roll it.
- **FR-006**: The echo MUST live with the Gm security state it belongs to, so
  it is replaced together with that state on every (re-)registration and can
  never be present without, or outlive, its SA.
- **FR-007**: An automated test MUST drive a synthetic multi-entry
  `Security-Server` list through registration and assert that every request
  type carries the complete list, and that a partial echo fails the test.
- **FR-008**: An automated test MUST show that a new registration which changes
  the `Security-Server` list changes the echo on requests built afterwards, and
  that a call set up under the new registration echoes the new list, never the
  old one.
- **FR-009**: Before merge, the change MUST be verified live on both Vodafone
  (local rig) and Jio (remote Pi): a call placed, session-refreshed and hung up
  from each side that is practical, plus keepalive and subscription observed
  healthy, with no new 4xx on any request.
- **FR-010**: A release note MUST describe the change, following the project's
  release-note convention.
- **FR-011**: Tests, docs and captures committed with this work MUST use
  synthetic identifiers only (for example `+919000000000`).

### Key Entities

- **Gm security state**: the negotiated SA parameters for a registration plus
  the verbatim `Security-Server` list to echo. Created or replaced on each
  successful registration; absent when no SA was negotiated.
- **Sec-agree header block**: the three headers derived from the Gm security
  state; empty when the state is absent.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: 100% of request types listed in FR-001 carry the full header
  block when a Gm SA is negotiated, verified by automated tests.
- **SC-002**: 0 requests carry any of the three headers when no Gm SA is
  negotiated, verified by automated tests.
- **SC-003**: On Jio and Vodafone live runs, 0 requests receive a new 4xx
  compared with the pre-change baseline, and calls still connect, refresh and
  end normally.
- **SC-004**: Dropping or truncating the echo in any request path makes the
  test suite fail.
- **SC-005**: The header block is defined in one place; a search finds no
  second copy.

## Assumptions

- The header block is the same for every request type except as FR-002 and
  FR-003 specify for CANCEL and the non-2xx ACK, which is the RFC-aligned
  reading. The plan phase confirms it against RFC 3329 and TS 24.229.
- Jio and Vodafone are expected to tolerate the headers on in-dialog requests,
  but this is unproven; the live runs in FR-009 are the gate. If either
  carrier rejects them, the fallback is to make the behaviour opt-in per
  carrier rather than ship it on by default.
- Confirmation from the MEO reporter of #95 is a follow-up after merge, not a
  gate; no enforcing carrier is available to test here.
- Responses the bridge sends (for example 200 OK to a carrier BYE) are out of
  scope: the RFCs require the headers on requests.
- The stale worktree `ims-renewal-watchdog` has been removed, so there is no
  in-flight branch to conflict with the refactor in FR-006.
