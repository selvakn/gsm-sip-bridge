# Feature Specification: Recognise a PBX reject by its Q.850 Reason, and make the reject list configurable

**Feature Branch**: `098-dpd-tuning` (spec drafted here; implementation branch to be decided)
**Created**: 2026-10-20
**Status**: Draft
**Input**: User description: "Follow-up to PR #110 (inbound decline signaling, issue #108). A FreePBX user reports that rejecting a call on a SIP extension still makes the carrier ring again: the extension answers 603, FreePBX turns it into 403 Forbidden with `Reason: Q.850;cause=21`, and the bridge classifies 403 as a failure and sends the carrier 480. Fix: (1) a final response whose Reason header carries Q.850 cause 21 (call rejected) or 17 (user busy) is a phone-side reject whatever its status code; (2) keep 403 out of the default reject statuses, since 403 can mean a real authorization failure; operators whose PBX rejects with a bare 403 add it themselves; (3) let operators add reject statuses in config. Cause 21 wins over the status code. Reads the Reason header from the final response in the PJSIP `on_call_state` callback."

## Why this exists

PR #110 made the bridge answer the carrier `486 Busy Here` when the
telephone side rejects an inbound call, and `480` for everything else. A
telephone-side reject is recognised only when PJSIP's final status is
`486`, `600` or `603`. Everything else is a failure and becomes `480`, which
RFC 3261 §21.4.18 lets a network treat as "try elsewhere". The carrier
(T2 in the original report) then offers the call again.

A reporter running FreePBX 16 (Asterisk 18) confirmed on PR #110 that the
fix does not help them:

- The SIP phone answers `603 Decline`.
- FreePBX does not relay it. It first answered `503`, and after the reporter
  fixed their PBX settings it answered `403 Forbidden`, which is the final
  response the bridge sees.
- That 403 carries `Reason: Q.850;cause=21`. Q.850 cause 21 is "call
  rejected": the called party refused the call. The PBX is telling us this
  is a reject; only the status code hides it.
- The bridge sees 403, classifies it as a failure, sends `480`, and the
  carrier rings again.

The maintainer first agreed to add `403` to the default reject list, and
then decided against it: `403` also means a real authorization failure
(for example a PBX refusing the bridge's own registration or trunk), and
reporting that as "busy" would hide a fault. A user whose PBX rejects with
a bare `403` adds it in config. Adding a code to the defaults is also not
enough on its own:

- **Status codes are unreliable across PBXs.** The same human action shows up
  as 603, 503 or 403 depending on the PBX, its dialplan and its trunk
  settings. Chasing codes one report at a time means every new PBX is a bug
  report and a release.
- **The status code is the weaker signal.** The Q.850 cause is the PBX's
  own statement of why the call ended. When it is present it should decide.

A reject misclassified as `480` has a costly symptom: the called person's
phone rings again after they declined, and the caller is left on ringback.
The reverse mistake (a failure classified as a reject) is also bad, since
callers get busy treatment for a broken PBX. That is why the two new
signals are kept narrow.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - A reject is a reject, whatever status the PBX uses (Priority: P1)

A person on a PBX extension rejects an incoming mobile call. The PBX ends
the telephone-side leg with a final response that carries
`Reason: Q.850;cause=21` (or cause 17, user busy). The carrier receives
`486 Busy Here` and does not offer the call again.

**Why this priority**: This is the reported, still-open defect.

**Independent Test**: Make the telephone side answer the bridge's INVITE
with `403 Forbidden` plus `Reason: Q.850;cause=21`, and, separately, with
`503` plus the same header. In both cases the carrier receives `486` on the
early dialog's To-tag, and no second INVITE follows.

**Acceptance Scenarios**:

1. **Given** an inbound call ringing on the telephone side, **When** the
   telephone side's final response has `Reason: Q.850;cause=21`, **Then**
   the carrier receives `486` and the call is recorded as rejected by the
   telephone side, whatever the status code is.
2. **Given** the same, **When** the cause is `17`, **Then** the result is
   the same.
3. **Given** a final response with a Reason cause other than 21 or 17 (for
   example `16` normal clearing or `19` no answer) and a status that is not
   a reject, **Then** it is classified from the status code as before.
4. **Given** a final response of `486`, `600` or `603` with no Reason
   header, **Then** it is a reject, as before this feature.
5. **Given** a final response with a Reason header that is not Q.850, or
   whose cause cannot be parsed, **Then** the header is ignored and the
   status code decides.

---

### User Story 2 - A bare 403 is a failure unless the operator opts in (Priority: P1)

`403` is not a default reject status. With no Reason header, a `403` from
the telephone side is a failure and the carrier receives `480`. An operator
whose PBX rejects with a bare `403` lists it in the config setting from
Story 3.

**Why this priority**: `403` can mean the bridge is not authorised on the
PBX. Calling that "busy" would send callers to busy-forwarding for a
configuration fault. Opt-in keeps the default honest.

**Independent Test**: Final response `403`, no Reason header: `480`. Same
response with `403` in the configured list: `486`.

**Acceptance Scenarios**:

1. **Given** a final response of `403` and no Reason header, and no
   configured list, **Then** the carrier receives `480`.
2. **Given** the same response and `403` in the configured list, **Then**
   the carrier receives `486`.
3. **Given** a `403` with `Reason: Q.850;cause=21`, with or without the
   list, **Then** the carrier receives `486` (Story 1).
4. **Given** a reject on `403`, **When** the call record is written,
   **Then** it reads `declined:pbx_rejected:403` (plus `:q850=<cause>` if a
   cause was present).

---

### User Story 3 - Operators can add reject statuses (Priority: P2)

An operator whose PBX rejects with another status (for example `503`, or
`404` on a PBX that answers that for a do-not-disturb extension) lists it in
config. It is treated as a reject from the next start.

**Why this priority**: It removes the need for a code change per PBX. It is
P2 because the Q.850 check in Story 1 already fixes the reported case.

**Independent Test**: Set the extra statuses to include 410. A telephone-
side final response of `410` with no Reason header produces `486`. Without
the setting it produces `480`.

**Acceptance Scenarios**:

1. **Given** extra reject statuses configured, **When** the telephone side
   ends with one of them, **Then** the carrier receives `486`.
2. **Given** extra statuses configured, **When** the bridge starts,
   **Then** the built-in defaults (`486`, `600`, `603`) are still
   treated as rejects. The setting adds to the defaults and cannot remove
   them.
3. **Given** a configured status outside 400-699 (or not a number),
   **When** the config loads, **Then** startup fails with an error naming
   the setting and the bad value.
4. **Given** the setting absent, **Then** behaviour is the defaults only.

---

### Edge Cases

- **Reason header missing or the event carries no response.** If the
  disconnect was caused by a timer, a transport error or a locally sent
  request, no received response is available. Classification falls back to
  the status code alone, exactly as today.
- **Several Reason headers.** A message can carry more than one Reason
  (for example one `SIP` and one `Q.850`). Use the Q.850 one; ignore the
  rest.
- **A reject cause on a success or provisional status.** Only failure
  responses (4xx-6xx) are classified. A Reason header on a `200` or on a
  BYE the telephone side sends after answering is not a pre-answer reject.
- **Cause 21 on `408` or `480`.** Cause 21 still wins. The PBX said the
  call was rejected.
- **503 without a Reason.** Stays a failure (`480`), unless the operator
  has added 503 to the reject statuses. A 503 normally means the PBX is
  failing and the carrier should be told "unavailable".
- **Capacity and ordering.** The stored per-call result grows from a status
  to a status plus an optional cause. The existing bounded store, and the
  rule that the reader sees the result of its own call, are unchanged.
- **Mock or stub builds.** Builds without `pjsip-linked` have no real
  header. Tests inject the cause through the same stub that injects the
  status.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: When a call to the telephone side ends with a failure final
  response whose `Reason` header has `protocol=Q.850` and `cause=21` or
  `cause=17`, the bridge MUST classify the outcome as a telephone-side
  reject, whatever the status code.
- **FR-002**: A telephone-side reject MUST be reported to the carrier as
  `486 Busy Here`, with the same To-tag as the earlier `180 Ringing`
  (unchanged from #110).
- **FR-003**: The Q.850 cause MUST take precedence over the status code.
  A status that would otherwise be a failure (`403`, `408`, `480`, `503`,
  and so on) is a reject when cause 21 or 17 is present.
- **FR-004**: A response with no usable Q.850 cause MUST be classified from
  its status code alone, exactly as before. In particular `486`, `600` and
  `603` remain rejects with no Reason header.
- **FR-005**: The default reject statuses MUST be `486`, `600` and `603`.
  `403` is NOT a default; it is a reject only if listed in config or
  accompanied by a reject cause.
- **FR-006**: The bridge MUST accept a `[sip]` config setting, beside the
  PBX settings (`server`, `password` and so on), listing extra reject
  statuses. It is global: one list for all lines and both transports. The effective set is the union of the defaults and the setting.
  The setting MUST NOT be able to remove a default.
- **FR-007**: Config validation MUST reject any extra status outside
  400-699, and report the setting name and offending value.
- **FR-008**: The `Reason` value MUST be read from the final response
  received for the telephone-side INVITE, inside the call-state callback
  where PJSIP delivers it, and stored with the status for the thread that
  classifies the call. If the event carries no received response, only the
  status is stored.
- **FR-008a**: The cause check MUST read only the failure final response
  (4xx-6xx) to the telephone-side INVITE. A `Reason` header on a BYE, a
  CANCEL, a provisional response or a 2xx MUST be ignored for
  classification.
- **FR-009**: Parsing MUST be tolerant. A missing header, a non-Q.850
  protocol, an unparsable or out-of-range cause, extra parameters, quoted
  `text=` values, and mixed-case names MUST NOT cause a failure. The result
  is "no cause".
- **FR-010**: The call record MUST keep the form
  `declined:pbx_rejected:<status>` and MUST append `:q850=<cause>` whenever
  a Q.850 cause was present on the response, whether or not it decided the
  outcome (for example `declined:pbx_rejected:403:q850=21`; a bare 603 stays
  `declined:pbx_rejected:603`). Metrics MUST still count it as a PBX reject.
  Anything that parses this string MUST tolerate the optional suffix.
- **FR-011**: When a call falls into the `480` path while a Reason header
  was present, the log line MUST include the status and the cause, so an
  unexpected PBX behaviour is visible without a packet capture.
- **FR-011a**: When the disconnect event carries no received response, the
  bridge MUST classify from the status code alone, and the `480`-path log
  line MUST say that no response was available, so the case is countable.
- **FR-012**: Behaviour for statuses the feature does not touch MUST NOT
  change: no answer, 404, 408 and 5xx without a reject cause still produce
  `480`.

### Key Entities

- **Disconnect result**: what the call-state callback records for a
  telephone-side leg: the final status (optional) and the Q.850 cause
  (optional). Read once by the thread that classifies the call.
- **Reject status set**: the statuses classified as a reject when no cause
  decides. Defaults plus operator additions.
- **Q.850 cause**: the number in `Reason: Q.850;cause=N`. Only 21 and 17
  mean reject here.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With the reporter's response (`403` + `Reason: Q.850;cause=21`)
  the carrier receives `486` and no second INVITE arrives within 60 seconds.
- **SC-002**: With `503` + cause 21 the carrier receives `486`; with `503`
  and no Reason the carrier receives `480`.
- **SC-003**: All existing #110 classification tests pass unchanged.
- **SC-004**: A table-driven test over (status, cause, extra statuses)
  covers every acceptance scenario above, and runs without PJSIP.
- **SC-005**: Operators can tell from the log, without a capture, which
  status and cause turned a call into a reject.

## Assumptions

- PJSIP delivers the received final response in the `on_call_state`
  event for a rejected outgoing INVITE (event type `TSX_STATE`, source
  `rdata`), and `Reason` is exposed as a generic string header. Static
  reading of the code and bindings supports this; a live call has not yet
  confirmed it. Verification is task zero of the plan.
- The reject set applies to calls the bridge places to the PBX or SIP
  server. It does not affect how the bridge treats the carrier side.
- 17 (user busy) is a reject for the same reason 486 is: the carrier
  should be told "busy". Confirmed in clarification.

## Out of scope

- Mapping a reject to `603 Decline` for the carrier. `486` is what #110
  sends and what T2 acted on.
- Other Q.850 causes (19 no answer, 16 normal clearing, 34 no circuit).
  They keep their current handling.
- Fixing the PBX. The reporter's first answer was `503` from a
  misconfigured FreePBX; that is the operator's to fix, though cause 21
  may incidentally make it work.
- The failed-INVITE response cache and Timer G retransmission (already out
  of scope in #108).

## Open questions (not blocking the spec)

- **Setting name.** The section is settled (`[sip]`); the plan picks the
  key (for example `reject_statuses = [410]`) after reading the config
  module and `docs/configuration.md`.
- **Whether PJSIP always hands us the response.** If a disconnect caused by
  the transaction layer carries no `rdata`, the cause check is skipped and
  the status decides. A live capture against the privileged test container
  settles how often that happens.
- **Branch.** This spec sits on `098-dpd-tuning` for now. Implementation
  should go on its own branch off `main`, since PR #110's code is the
  baseline and DPD tuning is unrelated.

## Clarifications

### Session 2026-10-20

- Q: Where does the extra-reject-statuses setting apply? → A: In the `[sip]` group with the PBX settings (server, password, ...); global, not per line.
- Q: Is Q.850 cause 17 (user busy) a reject alongside 21? → A: Yes; causes 21 and 17 are both rejects (confirmed, not provisional).
- Q: How does a cause-decided reject appear in the call record? → A: Always append `:q850=<cause>` when a cause was present, whether or not it decided the outcome.
- Q: Which messages does the cause check read? → A: Only the failure final response (4xx-6xx) to the telephone-side INVITE; BYE/CANCEL/provisional/2xx are ignored.
- Q: What if the disconnect event carries no received response? → A: Fall back to the status code alone (as in #110) and log that no response was available.
- Q (revised): Is 403 a default reject status? → A: No. Operators who need it add it to the `[sip]` list; it is a reject by default only with a reject cause.
