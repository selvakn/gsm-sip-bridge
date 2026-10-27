# Feature Specification: IMS Registration Refresh Without Call Interruption

**Feature Branch**: `123-ims-registration-refresh`  
**Created**: 2026-10-27  
**Status**: Draft  
**Input**: GitHub issue #163 (IMS registration refresh and Gm security continuity) and reporter PR #168 (commit `3e914c3`, used as reference only).

## Background

Today the bridge keeps its carrier IMS registration alive by repeating the *initial* registration procedure shortly before the granted lifetime ends: a new registration identity, a fresh authentication exchange, and a brand-new signalling connection, security association set and registration-event subscription. Because that replaces everything a live call's signalling depends on, the renewal is **held back while a call or an outgoing call set-up is in progress**. A sufficiently long call can therefore outlast its registration. Renewal also starts later than the 3GPP schedule (a one-hour grant renews after 55 minutes, not 50) and churns the contact endpoint and security state on every cycle, which the reporter believes (association only, not proven) may trigger carrier-side revocation shortly after re-registration.

The reporter's PR #168 demonstrates the approach works on a live network (16 renewals over ~13 h, 14 plain refreshes and 2 network-requested re-authentications, no failures, no revocations, no calls active) but is a single ~4,500-line change with confirmed defects and a stale base. This feature delivers the same outcome in reviewable, independently shippable slices. Reporter code is reused where sound and credited.

## Clarifications

### Session 2026-10-27

- Q: What happens if a carrier rejects the in-place refresh? → A: Automatic fallback. After N consecutive rejections of in-place refresh, the line uses the full re-registration for that cycle (deferred until idle if a call is active). No configuration switch.
- Q: What happens to a call if the registration expires mid-call? → A: The call is left running. The line is reported unregistered, refresh keeps retrying on backoff during the call, and full re-registration runs as soon as the call ends.
- Q: How short can the refresh interval get? → A: Floor of 60 s between refresh starts. A grant under 120 s refreshes at the floor and logs a warning.
- Q: Which lines must be live-tested before release? → A: Jio VoWiFi and the local Vodafone VoWiFi rig. VoLTE relies on automated tests and the fallback.
- Q: How should an operator see which refresh mode a line is using? → A: Logs plus the existing status CLIs, showing last refresh result and time, next scheduled refresh, mode in use (in-place or fallback) and consecutive rejections. No new metric or alert.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Registration renews on the standard schedule (Priority: P1)

An operator running a VoWiFi or VoLTE line expects the carrier registration to be renewed at the point the carrier's standards say, so the line is never at risk of lapsing because the bridge renewed late.

**Why this priority**: Smallest change, no protocol risk, and independently removes part of the lapse window. It also unblocks the later stories, which depend on correct timing.

**Independent Test**: Feed the scheduler grants of various lengths and verify the instant a renewal becomes due; no network needed.

**Acceptance Scenarios**:

1. **Given** a registration granted for 3600 s, **When** the schedule is evaluated, **Then** renewal becomes due 3000 s after registration (600 s before expiry).
2. **Given** a registration granted for 1200 s or less (e.g. 600 s), **When** the schedule is evaluated, **Then** renewal becomes due at half the granted lifetime (300 s).
3. **Given** a grant of exactly 1200 s and one of 1201 s, **When** evaluated, **Then** the first renews at 600 s and the second at 601 s (boundary behaves per the standard, no gap or double-fire).
4. **Given** the registrar grants a different lifetime on a later renewal, **When** the next schedule is computed, **Then** it uses the newly granted value, not the first one.

---

### User Story 2 - Registration refreshes during a call without disturbing it (Priority: P1)

A caller is on a long call (or a call is being set up). The registration reaches its refresh point. The bridge refreshes the existing registration over the signalling connection already in use; the call keeps its audio, its signalling (including the eventual hang-up) and its state, and the registration does not lapse.

**Why this priority**: This is the defect the issue is about: a call must neither be cut by, nor outlive, its registration.

**Independent Test**: With a simulated carrier, start a call, advance to the refresh point, and verify the call stays up and the registration lifetime is extended; then repeat with an outgoing call still being set up.

**Acceptance Scenarios**:

1. **Given** an active call and a registration at its refresh point, **When** the refresh runs, **Then** the call's media and signalling continue uninterrupted and the registration lifetime is extended.
2. **Given** an outgoing call being set up, **When** the refresh point is reached, **Then** the refresh proceeds and the call set-up completes normally.
3. **Given** a refresh is awaiting its answer, **When** call signalling or registration-event notifications arrive, **Then** they are handled without waiting for the refresh answer.
4. **Given** a successful refresh, **When** the registration-event subscription is examined, **Then** it is the same subscription as before (not replaced) and still renews on its own schedule.
5. **Given** a refresh is sent, **When** the carrier answers, **Then** the answer is accepted only if it belongs to that exact request (same registration identity, sequence number and transaction); a late or unrelated answer, including one that belongs to a different kind of request, never confirms the registration.
6. **Given** a sequence of refreshes and authentication exchanges, **When** the requests on the wire are examined, **Then** they all carry the one registration identity and a strictly increasing sequence number with no reuse.
7. **Given** no answer arrives, **When** the transaction timer (128 s) expires, **Then** the attempt is declared failed; over unreliable transport the request is retransmitted on the carrier-specified timers (2 s initial, 16 s cap) until then.

---

### User Story 3 - A failed refresh fails over and recovers without cutting the call (Priority: P1)

A refresh fails (carrier busy, wrong server, timeout, rejection). The line should behave as the existing renewal failure handling does: choose another carrier proxy when this one is unhealthy, retry on backoff, and only perform disruptive recovery when no call is active.

**Why this priority**: Without this, an in-place refresh would regress availability compared to today (the reporter's PR did exactly that: a busy answer with a long retry hint would make the line wait out the hint on a bad server instead of using the healthy one).

**Independent Test**: With a simulated carrier pool, fail a refresh in each way and verify the proxy-pool state, retry timing, registration-health reporting and that an active call is untouched.

**Acceptance Scenarios**:

1. **Given** two carrier proxies where the current one answers "service unavailable" with a long retry hint, **When** a refresh fails this way, **Then** that proxy is marked unavailable and the next registration goes through the other one rather than waiting out the hint.
2. **Given** a refresh failure while a call is active, **When** the failure is handled, **Then** the call is not torn down, disruptive recovery (replacing the connection, rebuilding the network attachment) waits for the call to end, and the previous binding keeps being reported as valid until it actually expires.
3. **Given** the binding actually expires or the network explicitly de-registers the line, **When** that happens, **Then** health reporting shows the line unregistered, even if a call is still up; the call is not ended by the bridge, refresh keeps retrying on backoff, and full re-registration starts as soon as the call ends.
4. **Given** a status that means in-place refresh cannot succeed (to be fixed in planning, see Open Questions), **When** received, **Then** the line falls back to a full re-registration, deferred until idle if a call is active.
5. **Given** repeated failures, **When** the existing failure budget is spent, **Then** the existing exit-and-supervisor-restart behaviour applies, deferred while a call is active as today.
6. **Given** a carrier that rejects the in-place refresh itself on 2 consecutive attempts (a final error answer, not a timeout, not a proxy-unavailable answer handled by scenario 1), **When** the next refresh is due, **Then** that cycle uses the full re-registration (deferred until idle if a call is active), the fallback is logged with the rejecting status, and the line stays registered.
7. **Given** a fallback re-registration has succeeded, **When** the following cycle is due, **Then** in-place refresh is tried again, so a transient rejection does not disable it permanently.

---

### User Story 4 - Refresh keeps working while an incoming call is ringing (Priority: P1)

An incoming call is ringing and the bridge is waiting on the PBX. A refresh answer arrives during that wait. It must be honoured, not discarded by a timer that expired while the answer was held.

**Why this priority**: Confirmed defect in the reference PR; a lost answer turns a successful refresh into a false failure.

**Independent Test**: Simulate an incoming call that rings for longer than the remaining refresh-transaction time, with the carrier answering the refresh during the ring.

**Acceptance Scenarios**:

1. **Given** a refresh started 100 s ago and an incoming call that rings for 40 s, **When** the carrier answers the refresh at 110 s, **Then** the refresh is confirmed, not timed out.
2. **Given** the PBX is slow to answer an incoming call, **When** registration answers and transaction timers come due, **Then** they are processed on time regardless of the ring wait.

---

### User Story 5 - Network-requested re-authentication without disturbing calls (Priority: P2, separate delivery)

The carrier occasionally answers a refresh with a fresh authentication challenge. The bridge answers it using the SIM without freezing call signalling, brings up replacement security associations alongside the current ones, switches over only when authentication succeeds, and keeps the old ones long enough for in-flight transactions.

**Why this priority**: Needed for completeness (the carrier does this about every 5 h in the reporter's trace) but the riskiest slice, so it ships separately and only after Stories 1-4 are verified live.

**Independent Test**: Simulated carrier issuing a challenge during a call; verify call signalling stays responsive, success switches over, failure rolls back, and the old security state still serves pending transactions.

**Acceptance Scenarios**:

1. **Given** a challenge arrives during an active call, **When** the SIM computation and security negotiation run, **Then** call signalling and registration-event notifications are still processed while they run.
2. **Given** a successful re-authentication, **When** the replacement takes over, **Then** the carrier-facing server port is unchanged, only the client side changes, and active call dialogs keep working (their next in-dialog request uses the new connection).
3. **Given** the re-authentication fails or is cancelled, **When** it is abandoned, **Then** only the staged replacement is removed and the working security state is untouched.
4. **Given** transactions that started on the old connection (including a pending outgoing call set-up), **When** the switch happens, **Then** the old connection stays usable for them for a drain period of at least the transaction timeout.
5. **Given** UDP transport, **When** the carrier's authentication reply is sent to the advertised server port instead of the new client port, **Then** it is still received and the authentication succeeds.
6. **Given** a re-authentication in progress, **When** a stale or superseded answer arrives later, **Then** it cannot restore or invalidate registration state.
7. **Given** the SIM reports a sequence-number mismatch, **When** the challenge is processed, **Then** the existing resynchronisation behaviour still applies.

---

### User Story 6 - Contributor credit and traceable evidence (Priority: P3)

The reporter's analysis and sound code are credited, and each shipped slice has live-network evidence recorded.

**Independent Test**: Review of commit/PR metadata and the evidence notes.

**Acceptance Scenarios**:

1. **Given** code or tests reused from PR #168, **When** it lands, **Then** the commit carries the reporter as author or co-author.
2. **Given** each slice, **When** it is released, **Then** the release notes carry a user-facing entry and the evidence (see Success Criteria) is recorded in the spec directory.

### Edge Cases

- The registrar grants a very short lifetime (under 120 s): refresh is paced at the 60 s floor (FR-003), so the binding may lapse before the next refresh; a warning is logged.
- Registrar returns "interval too brief" with a minimum: retry once with the minimum, not indefinitely.
- The registrar's answer lists other devices' contacts first, or omits ours: lifetime comes from our own contact; absence of our contact is a failure, not a success; a lifetime of zero means the binding was lost.
- Answer carries a malformed or duplicated lifetime parameter: treated as failure, not silently defaulted.
- Refresh and a force-recovery request both pending (connection judged dead): recovery wins, deferred until idle if a call is active.
- Clock changes or a long process pause make a refresh overdue: refresh is sent immediately on resume, once.
- Origination and incoming call at the same time as a refresh answer: no ordering of the three may lose the answer or leave the transaction pending.
- The line restarts mid-refresh: registration identity and sequence start fresh with the new initial registration, no stale state carried over.
- Line types without a security-association layer (no IPsec): refresh still works over the plain connection; re-authentication slice degrades to the existing behaviour.
- VoLTE and VoWiFi lines share the agent: neither may regress.

## Requirements *(mandatory)*

### Functional Requirements

**Schedule (Story 1)**

- **FR-001**: The system MUST schedule registration refresh 600 s before expiry when the granted lifetime exceeds 1200 s, and at half the granted lifetime otherwise.
- **FR-002**: The schedule MUST be recomputed from the lifetime granted on every successful registration or refresh.
- **FR-003**: Refresh starts MUST be at least 60 s apart. A grant shorter than 120 s MUST refresh at that floor instead of at half the grant, and a warning MUST be logged.

**In-place refresh (Story 2)**

- **FR-004**: Scheduled refresh MUST be sent over the existing protected signalling connection, reusing the credentials from the last successful authentication, without performing a new authentication unless the carrier challenges.
- **FR-005**: All registration requests for one registration MUST share a single registration identity and use a dedicated, strictly increasing sequence number, including across authentication exchanges.
- **FR-006**: Refresh MUST run as an asynchronous transaction: while it is pending the agent MUST continue handling call signalling, registration-event notifications and other traffic.
- **FR-007**: Refresh MUST proceed during an active call and during an outgoing call set-up, and MUST NOT alter any active call's media, dialog identity, route or sequencing.
- **FR-008**: A successful refresh MUST update the recorded lifetime, expiry and registration-derived routing information for future dialogs, and MUST preserve the registration-event subscription.
- **FR-009**: Responses MUST be matched to their request by registration identity, sequence number, method and transaction identifier; non-matching responses MUST NOT confirm a refresh, and a registration response MUST NOT be accepted as the answer to a keepalive probe or vice versa.
- **FR-010**: Transaction timing MUST use the IMS values: initial retransmit 2 s, retransmit cap 16 s, overall timeout 128 s; retransmission applies to unreliable transport only; a provisional answer MUST NOT extend the overall timeout.
- **FR-011**: The granted lifetime MUST be taken from the line's own contact in the answer; the answer MUST be rejected if that contact is absent or its lifetime is malformed.
- **FR-012**: If the carrier requires a longer minimum interval, the refresh MUST be retried once with that minimum.
- **FR-013**: Both the plain-signalling and protected-signalling headers on the refresh MUST be correct per the carrier standards; the open questions below determine the exact set.

**Failure handling (Story 3)**

- **FR-014**: Refresh failures MUST pass through the same proxy-pool and failover decisions as existing renewal failures (including marking a proxy unavailable on a service-unavailable answer and honouring its retry hint without blocking failover).
- **FR-015**: A failed refresh MUST NOT by itself discard a still-valid binding or tear down active calls; actual expiry and explicit network de-registration MUST be reported promptly. The bridge MUST NOT end a call because its registration expired; refresh retries continue on backoff during the call and full re-registration MUST start as soon as no call or set-up is active.
- **FR-016**: The maintenance policy MUST distinguish ordinary refresh (never deferred by a call) from registration recovery and re-attachment (deferred until no call or set-up is active, as today).
- **FR-017**: Statuses that require falling back to a full re-registration MUST be explicitly enumerated and each triggers recovery through the deferred path.
- **FR-018**: The existing consecutive-failure budget and exit-for-restart behaviour MUST be preserved, with its call-deferral unchanged.
- **FR-018a**: After 2 consecutive rejections of the in-place refresh (final error answers; timeouts and proxy-unavailable answers do not count), the next cycle MUST use the full re-registration as a fallback, deferred until idle if a call is active; in-place refresh MUST be retried on the cycle after a successful fallback. No operator configuration switch is provided for this.

**Ringing (Story 4)**

- **FR-019**: Registration answers and transaction timers MUST be processed without waiting for any held incoming-call reply; a timely answer received while such a hold is active MUST be honoured.

**Re-authentication (Story 5, delivered separately, gated on live validation of FR-001..FR-019)**

- **FR-020**: A network-requested authentication challenge MUST be answered off the main signalling path so SIM access and security negotiation do not block call handling.
- **FR-021**: Replacement security associations MUST be staged alongside the current ones, activated only after authentication succeeds, and fully removed on failure or cancellation without affecting the current ones.
- **FR-022**: The protected server port MUST remain unchanged; only client ports and association identifiers change.
- **FR-023**: Connections superseded by a switch-over MUST remain usable for pending transactions for a drain period of at least the transaction timeout, extended while an outgoing call set-up is pending, then retired.
- **FR-024**: Active call dialogs and any pending set-up MUST use the new connection for subsequent requests without changing their identity or route.
- **FR-025**: Over UDP, authentication replies addressed to either the new client port or the advertised server port MUST be received.
- **FR-026**: Late, stale or cancelled-attempt answers MUST NOT alter registration state.

**Observability and delivery**

- **FR-027**: Logs and status MUST distinguish refresh, re-authentication and recovery, and MUST show a deferred recovery as deliberate, not as a stall. The existing status CLIs (`vowifi-status`, `volte-status`) MUST show the last refresh result and time, the next scheduled refresh, the mode in use (in-place or fallback) and the consecutive rejection count. No new metric or alert is added. No credentials or subscriber identifiers appear in logs, fixtures or docs; fixtures use synthetic identifiers only.
- **FR-028**: Each delivered slice MUST add a user-facing release-note entry and update the VoWiFi bridge documentation.
- **FR-029**: Reused reporter code MUST preserve authorship credit.

### Key Entities

- **Registration**: the carrier-side binding for the line: identity, current sequence, granted lifetime, expiry, cached authentication credentials, and health state (registered, refreshing, failed, expired).
- **Refresh transaction**: one pending registration request: its sequence, transaction id, timers, retransmit state, and requested lifetime.
- **Re-authentication attempt**: a background challenge response with staged replacement security state, a result, a cancel flag and a deadline.
- **Security association set**: the pair of protected connections with the carrier proxy (client and server side), with current, staged and draining generations.
- **Maintenance work**: refresh, registration recovery, re-attachment, with the rule for which may run during a call.
- **Proxy pool**: the set of carrier proxies with availability marks and retry hints.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: For a 3600 s grant the refresh is sent at 3000 s ± 1 s on every cycle (previously 3300 s).
- **SC-002**: In a live test, a call lasting longer than one full registration lifetime stays up end to end, and the registration is still valid when it ends; the call's audio shows no gap attributable to registration activity.
- **SC-003**: Over a 12 h live run with no calls, 100% of scheduled refreshes succeed, with one registration identity and strictly consecutive sequence numbers, and zero carrier-side revocations of the line's own binding.
- **SC-004**: At least one live refresh and one live re-authentication each occur while a call is active, with the call unaffected (neither dropped nor one-way audio).
- **SC-005**: At least one live run over each of the supported signalling transports (TCP, UDP) completes a refresh and, for Story 5, a re-authentication.
- **SC-005a**: The live evidence in SC-002 to SC-005 is collected on Jio VoWiFi and on the local Vodafone VoWiFi rig. VoLTE lines are covered by automated tests and the FR-018a fallback, and a VoLTE live run is recorded as a follow-up, not a release blocker.
- **SC-006**: When the current proxy fails a refresh with service-unavailable and another is healthy, the next registration attempt uses the other proxy within one retry cycle, not after the carrier's retry hint.
- **SC-007**: A refresh answer arriving during a 40 s incoming-call ring, with 100 s of the transaction already elapsed, is confirmed in 100% of automated runs.
- **SC-008**: No deferred refresh ever remains: with a call in progress at refresh time, the time from "due" to "request sent" is under 2 s.
- **SC-009**: Quality gates pass (format, lint across all targets, full test suite), and automated coverage exists for each acceptance scenario.
- **SC-010**: The repository holds no real subscriber identifiers after the change.
- **SC-011**: With a simulated carrier that rejects every in-place refresh, the line stays registered across at least three cycles via the fallback, with no operator action and no active call affected.

## Assumptions

- The carrier accepts a refresh over the existing protected connection with the previously computed credentials, as the reporter observed (14/14 accepted, one carrier, TCP, no call active). Behaviour on other carriers/transports is to be verified, not assumed; the automatic fallback (FR-018a) is the safety net for carriers that do not accept it. The threshold of 2 consecutive rejections is a starting default to be revisited after live testing.
- Stories 1-4 ship before Story 5; Story 5 is not started until Stories 1-4 have live evidence including a mid-call refresh.
- Disruptive recovery (replacing the connection, rebuilding the LTE attachment) keeps today's deferral during calls.
- The existing proxy-pool, backoff and failure-budget logic is reused, not redesigned.
- The reporter's PR #168 is a reference: parts that are sound (schedule helper, maintenance-policy split, response matching, test fixtures) may be adopted with credit; the defective parts (failure handling, ring-wait ordering, UDP reply reading) are re-implemented here.
- The association between the old repeated-registration behaviour and carrier revocation (478 events on a past date) is unproven; the feature is justified by standards alignment and call continuity, and does not promise to eliminate revocations.
- The pending live validations (mid-call refresh, mid-call re-authentication, UDP) need the Jio rig and are planned as part of delivery, not assumed passed.

## Open Questions (to resolve in planning/research against the standards text, not blockers for the spec)

1. Whether the protected registration request must carry the "integrity-protected" authorization parameter (neither the current code nor PR #168 sets it).
2. Whether replaying the cached nonce-count and client nonce on every refresh is acceptable to the standards and to the carriers we run on, or whether the count should advance.
3. The exact set of response statuses that require falling back to a full re-registration (the reference PR chose 403, 408, 500, 504; its basis is unverified).
