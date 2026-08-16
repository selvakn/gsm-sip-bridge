# Feature Specification: siptest — SIP softphone for agent-driven end-to-end testing

**Feature Branch**: `037-siptest-softphone`
**Created**: 2026-08-15
**Status**: Draft
**Input**: User description: "build a sip (voip) client, preferably based on rust or golang, its sort of a sub project / new project, but the sip client's purpose is for you (coding agent) to do e2e test, it will register with our bridge, you should be able to place/receive call, check the audio in different directions, log the status, errors for debugging. add ability to record the audio."

## Clarifications

### Session 2026-08-15

- Q: What guard should siptest enforce on outbound destinations, given an agent could loop and dial real phones repeatedly? → A: Both a configured destination allow-list and rate limiting (minimum interval plus hourly cap).
- Q: How should call records and recordings be retained, given the daemon runs indefinitely? → A: Cap the number of retained calls; on exceeding it, evict the oldest, deleting its recordings and dropping its record.

## User Scenarios & Testing *(mandatory)*

The "user" throughout this spec is a **coding agent** debugging the bridge, not a
human operator. That framing drives every interface decision: machine-readable
output, stable identifiers, poll-friendly endpoints, and a semantic exit code.

### User Story 1 - Place an outbound call and learn whether audio flowed (Priority: P1)

An agent investigating a call-path defect asks the tool to dial a mobile number
through the bridge, hold the call for a fixed duration, and report what
happened. It gets back a machine-readable verdict naming, per direction,
whether media actually flowed — not merely whether the call connected.

**Why this priority**: This is the smallest slice that replaces a physical
handset. Outbound calling exercises the registrar, the digest exchange, the
`302` redirect to the telephony agent, SDP negotiation and the RTP path — the
majority of the local SIP leg — and it needs no bridge reconfiguration.

**Independent Test**: Register against the bridge's registrar, dial a known
destination, and confirm the report distinguishes "answered with two-way audio"
from "answered but silent in one direction".

**Acceptance Scenarios**:

1. **Given** the bridge's registrar is reachable and credentials are valid,
   **When** the agent requests a call to a valid destination,
   **Then** the tool registers, follows the `302` redirect, completes the call,
   and returns a report containing a per-direction media verdict.
2. **Given** a call is answered but media flows in one direction only,
   **When** the report is produced,
   **Then** the verdict distinguishes send-only from receive-only, and the
   overall result is a failure.
3. **Given** the destination is refused by the bridge,
   **When** the refusal arrives,
   **Then** the reported error names the specific cause (invalid destination,
   no idle line, untrusted source) rather than a generic failure.

---

### User Story 2 - Stay registered and be driven over a control interface (Priority: P1)

The tool runs as a long-lived process that holds its registration, and the
agent drives it by issuing requests and polling for state. Registration
survives across many probes, and the agent can discover asynchronous
occurrences without having been blocked waiting for them.

**Why this priority**: Equal-first with US1 because inbound calls are
undeliverable to a process that is not already registered when the call
arrives. A one-shot invocation structurally cannot receive a call, so the
long-running form is a precondition for US3 rather than a convenience.

**Independent Test**: Start the daemon, confirm the bridge reports a live
binding for its account, leave it idle past one refresh interval, and confirm
the binding is still live and the reported registration state is accurate.

**Acceptance Scenarios**:

1. **Given** the daemon is running and registered,
   **When** the agent queries status,
   **Then** it receives registration state, the advertised contact address,
   time until renewal, and any recent error.
2. **Given** the registration is approaching expiry,
   **When** the refresh interval elapses,
   **Then** the tool re-registers without agent involvement and without
   dropping its binding.
3. **Given** the registrar is unreachable,
   **When** registration fails repeatedly,
   **Then** the tool backs off, keeps reporting an accurate failure state and
   a failure count, and recovers automatically once the registrar returns.

---

### User Story 3 - Receive an inbound carrier call and verify it (Priority: P2)

An inbound call from the mobile network rings the tool. The agent discovers the
call by polling, sees the caller identity, and — depending on policy — the tool
answers automatically or waits to be told. The same per-direction audio verdict
is produced as for outbound.

**Why this priority**: This is the path with the open defect (inbound rings and
answers, but the caller hears ringback). It is P2 only because it depends on
US2 and requires a bridge-side configuration change to redirect ringing away
from the real handset.

**Independent Test**: With the bridge configured to ring the tool's account,
place a call from another phone and confirm the agent can detect the call,
observe the answer, and read the audio verdict — using polling alone.

**Acceptance Scenarios**:

1. **Given** the daemon is registered and inbound policy is auto-answer,
   **When** the bridge sends a call setup request,
   **Then** the tool signals ringing, answers after the configured delay, and
   records the call with an audio verdict.
2. **Given** the call setup request arrives from a network port other than the
   one the tool registered to,
   **When** it is received,
   **Then** it is accepted and answered normally.
3. **Given** inbound policy is manual,
   **When** a call arrives,
   **Then** the tool holds it ringing, publishes a discoverable event carrying
   the caller identity, and answers or rejects only when instructed.
4. **Given** the caller abandons the call before it is answered,
   **When** the cancellation arrives,
   **Then** the tool terminates the call cleanly and records the outcome as
   caller-cancelled rather than as a fault.

---

### User Story 4 - Prove the audio is ours, not noise, and record it (Priority: P2)

Packet counting can only say that *something* arrived. The tool emits a known
signal and detects that same signal returning, so it can distinguish our audio
from comfort noise, static or an unrelated stream — and writes each direction
to its own audio file for later inspection.

**Why this priority**: It converts a weak signal ("packets arrived") into a
strong one ("our audio arrived"), and yields a round-trip delay measurement the
bridge currently cannot produce at all. Depends on US1's media path existing.

**Independent Test**: Run a call whose far end loops audio back, and confirm
the tool reports the signal as detected with a plausible round-trip delay;
then run one where the return path is silent, and confirm it reports the signal
as absent while still reporting that packets arrived.

**Acceptance Scenarios**:

1. **Given** a call with a working return audio path,
   **When** the call completes,
   **Then** the report states the signal was detected, with a round-trip delay
   and a symbol error rate.
2. **Given** a call where the far end sends only noise or silence,
   **When** the call completes,
   **Then** the report states the signal was not detected, and separately
   reports the received energy level — so "no audio" and "audio but not ours"
   are never conflated.
3. **Given** recording is enabled,
   **When** the call completes,
   **Then** two separate audio files exist, one per direction, at the
   negotiated audio rate.
4. **Given** no audio arrives at all,
   **When** the report is produced,
   **Then** the transmitted audio is byte-identical to what would have been
   transmitted had audio arrived.

---

### Edge Cases

- **The tool's account is the one the bridge rings.** Registering under an
  account already held by a physical handset silently displaces that handset,
  because the bridge holds one binding per account. The tool must not do this
  accidentally.
- **Call setup arrives from an unexpected network port.** The bridge rings from
  its telephony agent, not from the registrar the tool registered to. Rejecting
  on source port would make every inbound call invisible.
- **The redirect target moves.** Which port the bridge redirects outbound calls
  to depends on which subsystem is enabled. Hardcoding it breaks silently under
  a different deployment.
- **An acknowledgement is lost.** On an unreliable transport, a final response
  that is never acknowledged must be retransmitted, or the call establishes on
  one side only.
- **The far end requires a protocol extension the tool does not implement.**
  The tool must refuse explicitly rather than stall.
- **The carrier does not pass the test signal.** Voice-activity detection,
  comfort noise, transcoding, and handset gain control can all suppress a
  synthetic signal. A signal-detection failure must never be reported as "no
  audio".
- **Audio never loops back.** Round-trip delay is unmeasurable without a return
  path, and its absence must not be reported as a call failure.
- **The advertised contact address is unroutable.** If the tool advertises a
  wildcard or wrong address, inbound calls silently never arrive.
- **Two calls at once.** A second inbound call while one is active must be
  refused predictably rather than corrupting state.
- **Registration lapses mid-session.** A subsequent outbound attempt is refused
  by the bridge in a way that resembles an authentication fault; the reported
  cause must not mislead.
- **An agent retries in a loop.** Outbound calls cost money and ring real
  people. A failing call that the agent immediately re-attempts must hit the
  local rate limit rather than the carrier, and the refusal must be
  distinguishable from a call that the bridge itself rejected.
- **A mistyped destination.** A syntactically valid number that nobody
  authorised must be refused locally by the allow-list, before any signalling
  leaves the host.
- **A long-lived daemon accumulating calls.** Recordings and reports must not
  grow without bound, and an agent asking for a call that has since been
  evicted must be told so rather than receiving a not-found that it cannot
  distinguish from a bad identifier.

## Requirements *(mandatory)*

### Functional Requirements

**Registration**

- **FR-001**: The tool MUST register to the bridge's registrar using the same
  digest authentication scheme the registrar enforces, and MUST NOT rely on any
  scheme the registrar refuses.
- **FR-002**: The tool MUST refresh its registration before expiry without
  agent involvement, and MUST de-register on clean shutdown.
- **FR-003**: The tool MUST back off and retry on registration failure, expose
  a consecutive-failure count, and recover automatically.
- **FR-004**: The tool MUST advertise a contact address that is routable from
  the bridge, and MUST surface that address so a misconfiguration is visible
  rather than silent.
- **FR-005**: **WON'T DO** (user decision, 2026-08-15). Originally: the tool
  MUST refuse to start when configured with the account the bridge is
  currently configured to ring, unless the operator explicitly opts in to
  displacing it. Descoped rather than implemented — the tool has no way to
  read the bridge's `ring_aor` value at all (only whether *some* account is
  currently registered, via metrics), so there was no data to check against.
  Provisioning a dedicated account (never the operator's own handset's) stays
  a documented operator responsibility (quickstart.md) instead of an enforced
  check.

**Outbound calling**

- **FR-006**: The tool MUST place an outbound call to a caller-supplied
  destination through the bridge.
- **FR-006a**: The tool MUST refuse to dial any destination not matching a
  configured allow-list, with a distinct error naming the reason. An empty or
  absent allow-list MUST refuse everything rather than permit everything.
- **FR-006b**: The tool MUST enforce a minimum interval between outbound call
  attempts and a maximum number of attempts per hour, refusing excess attempts
  with a distinct error that states when the caller may retry. Both limits MUST
  be configurable. Inbound calls MUST NOT be affected by either limit.
- **FR-007**: The tool MUST follow the bridge's redirect to its telephony
  agent, taking the target from the redirect itself rather than from
  configuration, and MUST acknowledge the redirect.
- **FR-008**: The tool MUST send every request of a dialog from the same local
  network endpoint it registered from.
- **FR-009**: The tool MUST map each documented refusal to a distinct,
  named error rather than a generic failure.
- **FR-010**: The tool MUST terminate a call that is not answered within a
  configurable ring timeout, and MUST report that outcome distinctly.
- **FR-011**: The tool MUST end a call cleanly after a configurable duration
  and report why it ended.

**Inbound calling**

- **FR-012**: The tool MUST accept inbound call setup from the bridge
  regardless of which source port it originates from, and MUST NOT validate on
  source port.
- **FR-013**: The tool MUST support auto-answer, auto-reject, and manual
  inbound policies, changeable at runtime without a restart.
- **FR-014**: The tool MUST retransmit an unacknowledged final response until
  acknowledged or a bounded limit is reached.
- **FR-015**: The tool MUST capture and report every caller-identity field the
  bridge supplies, separately, so disagreements between them are visible.
- **FR-016**: The tool MUST handle caller cancellation before answer, and
  report it as a caller-side outcome rather than a fault.
- **FR-017**: The tool MUST refuse a concurrent call beyond its configured
  limit with a well-defined response.

**Media and verification**

- **FR-018**: The tool MUST negotiate audio with the bridge and exchange media
  for the duration of the call.
- **FR-019**: The tool MUST report, per direction, the count of media packets
  sent and received, and derive a direction verdict that keeps send-only and
  receive-only distinct.
- **FR-020**: The tool MUST report packet loss, reordering and jitter for the
  received stream.
- **FR-021**: The tool MUST transmit a known signal and detect that same signal
  in the received stream, reporting detection as a verdict separate from the
  packet-count verdict.
- **FR-022**: The tool MUST report received audio energy independently of
  signal detection, so "nothing arrived" and "something arrived that was not
  ours" are always distinguishable.
- **FR-023**: The transmitted signal MUST NOT depend in any way on what is
  received, so that the two directions remain independently attributable.
- **FR-024**: The tool MUST measure round-trip audio delay when the signal
  returns, and MUST report its absence as unmeasured rather than as a failure.
- **FR-025**: The tool MUST record each direction to its own audio file at the
  negotiated audio rate, and report the file locations.
- **FR-025a**: The tool MUST retain at most a configured number of completed
  calls. On exceeding it, the oldest call's recordings and report MUST be
  deleted from disk and its record dropped, so both disk and memory stay
  bounded without operator involvement. A request for an evicted call MUST
  report it as no longer available, distinctly from a call that never existed.
- **FR-026**: The tool MUST report the negotiated codec and, where a codec's
  advertised rate differs from its true audio rate, use each in its correct
  role.

**Agent-facing interface**

- **FR-027**: The tool MUST run as a long-lived process that holds registration
  across many operations.
- **FR-028**: The tool MUST expose a control interface over which an agent can
  query status, place calls, answer/reject/hang up, change inbound policy, and
  retrieve call reports and recordings.
- **FR-029**: The tool MUST publish an ordered, replayable event log addressable
  by cursor, so an agent can discover asynchronous occurrences — inbound calls
  above all — by polling alone.
- **FR-030**: Every report MUST be available in a machine-readable form, and
  MUST also carry a human-readable rendering.
- **FR-031**: The tool MUST expose recent log output through the control
  interface, so an agent can diagnose without locating the process's output
  stream.
- **FR-032**: A single-shot invocation MUST return a semantic exit code:
  success only when the call met the configured success requirement, where an
  answered-but-silent call is a failure.
- **FR-033**: Diagnostic output MUST go to the error stream so the answer
  occupies the output stream alone.
- **FR-034**: Credentials MUST be referenceable indirectly from the environment
  and MUST NOT appear in logs or debug output.

### Key Entities

- **Account**: The identity the tool registers with — address of record,
  credentials, realm, requested registration lifetime.
- **Registration**: The live binding — its state, advertised contact, expiry,
  renewal time, last response, consecutive failure count.
- **Call**: One dialog — stable identifier, direction, peer identity, state,
  timestamps, negotiated codec, end reason, and its report.
- **Call report**: The verdict bundle for one call — signalling timings, media
  counters, loss/reordering/jitter, direction verdict, signal-detection
  verdict, energy profile, round-trip delay, recording locations, overall
  success against the configured requirement.
- **Inbound policy**: How unsolicited calls are treated — mode, answer delay,
  rejection status, call duration.
- **Event**: One entry in the ordered log — monotonic sequence number,
  timestamp, kind, and payload.
- **Signal plan**: The definition of what is transmitted and expected back —
  which tones, symbol duration, level.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An agent can place a call through the bridge to a mobile number
  and obtain a pass/fail audio verdict without any human touching a handset.
- **SC-002**: A call that connects but carries audio in only one direction is
  reported as a failure, naming the affected direction.
- **SC-003**: An agent can detect that an inbound call arrived, and read its
  caller identity, using only polling — no streaming connection, no log
  scraping.
- **SC-004**: Round-trip audio delay is reported whenever the return path
  carries the signal — a measurement the existing tooling reports as
  unavailable in all cases today.
- **SC-005**: Every call produces two per-direction recordings that open in a
  standard audio player at the correct rate.
- **SC-006**: The full test suite for this feature runs with no modem, no SIM,
  no carrier and no running bridge, and completes within the project's
  per-test time limit.
- **SC-007**: A registration lasts indefinitely without agent involvement, and
  survives a registrar restart without manual intervention.
- **SC-008**: Reproducing a call-path defect takes one command and yields a
  report that names the failing direction — replacing a manual handset session.
- **SC-009**: No sequence of control-interface requests, however malformed or
  repetitive, can cause the tool to dial a destination outside the allow-list
  or to exceed the configured hourly call cap.
- **SC-010**: Disk and memory used by call records and recordings stay bounded
  by the configured retention cap no matter how long the daemon runs or how
  many calls it handles.

## Assumptions

- The tool and the bridge are on the same LAN with no address translation
  between them; the bridge does not support traversal and it is out of scope.
- The bridge is deployed with its embedded registrar enabled and outbound
  calling permitted; without the latter the tool can only receive.
- The tool is given its own account, distinct from any physical handset's,
  because the bridge holds a single binding per account.
- Exercising inbound calls requires pointing the bridge's ring target at the
  tool's account, which takes the physical handset out of service for the
  duration. Inbound test runs are therefore deliberate, not continuous.
- Only the transport the registrar supports is required; other transports are
  out of scope.
- The tool acts only as a handset. Standing in for the telephone system the
  bridge registers outward to is a separate capability and is out of scope.
- One call at a time is sufficient; concurrency beyond that is not a
  requirement.
- Carrier-path behaviour (whether a synthetic signal survives transcoding and
  noise suppression) is an unknown to be characterised by using the tool, not a
  requirement the tool must guarantee.
