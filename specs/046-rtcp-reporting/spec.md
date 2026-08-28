# Feature Specification: RTCP reporting on the media legs

**Feature Branch**: `046-rtcp-reporting`
**Created**: 2026-08-27
**Status**: Draft
**Input**: User description: "Prepare for RTCP support (RTP-01, deferred out of batch 5). No RTCP is sent or received, while its bandwidth is declared. RFC 3550 §6 — every RTP participant sends RTCP. TS 26.114 §7.3 requires it of an MTSI client, and §6.2.10 requires the b=RS/b=RR declarations that say how much of it there will be. The answer states b=AS, b=RS:800 and b=RR:2400. No RTCP socket is bound anywhere, no sender or receiver report is ever generated, and none is read. A conformant peer sees a participant that promised reports and sends none; some networks treat prolonged RTCP silence as a dead session and release the bearer. It also forfeits the only standard source of loss, jitter and round-trip data — the exact evidence the one-way-audio investigations have been reconstructing from packet counters. Fix: bind RTP+1 (honouring a=rtcp when present), send a periodic Sender Report with the relay's own SSRC and byte/packet counts, and parse inbound RRs. The jitter and loss they carry drop straight into the existing media-stats reporting. ims/sdp.rs:684 the b= declarations · no RTCP anywhere in ims/"

## Why this exists

Every SDP answer this bridge sends states, in `b=RS:800` and `b=RR:2400`,
exactly how much RTCP bandwidth it intends to use — and then uses none.
No RTCP socket is bound anywhere in the IMS path, no report of any kind is
ever generated, and nothing reads a report the far end sends. The
declaration is a promise the implementation does not keep.

This is the last open finding from batch 5 of the terminating-side
conformance review (`docs/plans/mt-conformance-findings.md`, **RTP-01**),
deferred out of that batch by explicit decision once research showed it
needs call-wide state that exists nowhere in this codebase today. That
deferral was the right call for a batch of small fixes; it is the whole
subject of this feature.

Three things are wrong today, and they compound:

- **The promise is unkept.** RFC 3550 §6 makes RTCP obligatory for every
  RTP participant, and TS 26.114 §7.3 restates it for an MTSI client. A
  peer that reads our `b=RS:`/`b=RR:` lines is entitled to expect reports
  at roughly that rate. It gets silence.
- **Prolonged RTCP silence is a liveness signal to some networks.** A
  session that never reports can be judged dead and have its bearer
  released mid-call — a failure mode that would look, from this end, like
  an unexplained carrier-side hangup.
- **The one diagnostic that would have shortened every one-way-audio
  investigation is being thrown away.** A receiver report carries the far
  end's own view of loss, jitter and round-trip time. This project's
  one-way-audio incidents have been reconstructed painstakingly from
  packet counters precisely because that standard evidence was never
  collected. The machinery to represent it already exists here
  (`media_stats::ReceiveStats` — received/lost/reordered counts and RFC
  3550 §6.4.1 jitter, all already computed) and is simply not wired to
  the relay paths or fed by anything the far end says.

Sibling finding **SDP-06's `a=rtcp` half** — an offer's explicit RTCP port
attribute (RFC 3605) being discarded — was deferred alongside RTP-01 for
the same reason, because an explicit RTCP port has nothing to consume
without real RTCP. It is in scope here.

Removing the `b=RS:`/`b=RR:` lines instead was considered during batch 5
and rejected: it would trade this gap for a fresh violation of the TS
26.114 §6.2.10 mandate those lines exist to satisfy, closing nothing.

### What the codebase has today, and does not

Established by direct inspection (recorded here so planning starts from
fact, not assumption):

| Needed for RTCP | Status today |
| --- | --- |
| An RTCP socket | None bound anywhere. RTP sockets bind ephemeral (`UdpSocket::bind((ip, 0))`) at `agent/inbound.rs:320`, `agent/veth.rs:88`, `agent/origination.rs:306` — so the port is arbitrary, may be odd, and RTP+1 is not reserved |
| A stable, exposed SSRC for the sending side | `transcode::RtpSender` mints a fresh random SSRC per relay-direction thread and never returns it. The pass-through relay (`agent::veth::forward`) has no SSRC of its own at all — it forwards the source's bytes verbatim |
| Send-side **octet** counts | `media_stats::MediaMeter` counts packets only, per *receive* direction, never bytes and never per send direction |
| Receive-side loss/jitter | `media_stats::ReceiveTracker` computes all of it correctly — but is used only by the standalone `ims-call` path (`ims/call.rs:445`), never by either relay |
| A per-call timer with access to the live socket | The dispatch loop ticks ~100ms during a call, but the socket is moved into the relay thread(s) at spawn and retained nowhere the tick can reach |
| A synchronous teardown hook | `call::handle_bye`/`hangup_carrier`/`end_call_attachment_lost` flip a stop flag and return immediately; nothing joins the relay threads |
| An offer's `a=rtcp` attribute | Parsed nowhere; `sdp::parse_offer` discards it |

Scope, once resolved (FR-023): the **carrier-facing leg of answered
calls** only. That leg is still carried by *both* relay implementations
depending on what the codecs negotiated — the pass-through relay
(`agent::veth::forward`) when both legs agree, the transcoding relay
(`ims::transcode`) otherwise — so both are in scope for the carrier
direction, spawned from `agent/inbound.rs`. The internal veth leg to our
own PJSIP, the three originated-call relay sites in
`agent/origination.rs`, and the standalone `ims/call.rs` diagnostic path
are all untouched.

## Clarifications

### Session 2026-08-27

- Q: On the pass-through relay path the bridge forwards the far source's
  RTP verbatim, so the SSRC reaching the carrier is not its own — what
  source identity should its sender reports use? → A: Report under
  whichever SSRC is actually on the wire toward the carrier: the SSRC
  `RtpSender` mints on the transcoding path (exposed), and the observed
  SSRC of the forwarded stream on the pass-through path. One rule, no
  change to the media itself.
- Q: How often should reports be sent — derived from the declared
  `b=RS:` bandwidth, a fixed floor, or full RFC 3550 §6.3 scheduling? →
  A: Derive the interval from the declared bandwidth and the average
  compound packet size, randomised ±50% per RFC 3550 §6.3.1. No member
  counting and no timer reconsideration — the session is two-party by
  construction, and that machinery exists for participant counts this
  bridge cannot have.
- Q: Where should the RTCP-derived and receive-quality figures surface —
  the existing end-of-call log line, Prometheus metrics, or both? → A:
  Both. Log fields on the existing "call media verdict" line (plus a
  debug line as each far-end report arrives) for per-call forensics, and
  new Prometheus metrics for loss, jitter and round-trip time so the
  figures are trendable and alertable across the fleet rather than only
  readable per call.
- Q: FR-017 and FR-022 contradicted each other on what the SDP says when
  no RTCP port can be obtained — omit the bandwidth declaration, or keep
  it? → A: Keep `b=RS:`/`b=RR:` unconditionally and report the shortfall
  loudly (warning plus metric) instead of mutating the answer. Those
  lines declare intended bandwidth per TS 26.114 §6.2.10; a rare local
  socket failure is a fault to surface, not a capability statement to
  encode into SDP, and a per-call omission would reintroduce the very gap
  batch 5 refused to create.
- Q: Which inbound RTCP should be trusted, now that its figures feed
  persistent metrics and not just a per-call log line? → A: Accept only
  RTCP arriving from the negotiated peer address; discard anything else
  with a diagnostic, never letting it reach the recorded figures or the
  metrics. This mirrors the trust boundary the RTP path already enforces
  (its sockets are connected to the peer), needs no configuration, and
  deliberately stops short of also requiring a known SSRC — that would
  reject a legitimate report arriving right after a mid-call source
  restart.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - The bridge sends the reports its answer promised (Priority: P1)

A call is up and media is flowing. The far end — a carrier IMS network
that read our `b=RS:800`/`b=RR:2400` declaration — receives periodic
reports from us describing what we have sent, at approximately the rate we
declared, for as long as the call lasts.

**Why this priority**: This is the finding. Everything else in this
feature either enables it or builds on it. On its own it closes the
conformance gap and removes the "prolonged silence reads as a dead
session" risk, which is the one failure mode here that can drop a real
call.

**Independent Test**: With a call up, capture what leaves the bridge's
RTCP port and confirm reports arrive periodically, at the declared rate,
carrying a consistent source identifier and counts that grow in step with
the media actually sent. Fully testable without the far end ever sending
RTCP back.

**Acceptance Scenarios**:

1. **Given** an answered call with media flowing, **When** the call has
   been up longer than one reporting interval, **Then** the bridge has
   sent at least one sender report describing the media it has
   transmitted.
2. **Given** an answered call that continues for several reporting
   intervals, **When** each interval elapses, **Then** a further report is
   sent, at an average rate consistent with the bandwidth the answer
   declared.
3. **Given** two consecutive reports describing one uninterrupted stream,
   **When** they are compared, **Then** they carry the same source
   identifier, and their packet and octet counts are non-decreasing and
   consistent with the media sent between them.
4. **Given** a call in which the source of the media the bridge forwards
   restarts mid-call (a new source identifier appears on the stream),
   **When** the next report is sent, **Then** it is sent under the new
   identifier — matching what the carrier is actually receiving — rather
   than continuing to report under an identifier no longer on the wire.
5. **Given** a call in which this bridge has sent no media at all (a
   direction that never carried audio), **When** a reporting interval
   elapses, **Then** a report is still sent, correctly stating that
   nothing was sent, rather than the bridge falling silent.

---

### User Story 2 - The far end's view of the call is captured and reported (Priority: P1)

The far end sends its own reports. The bridge reads them, and the loss,
jitter and round-trip figures they carry appear in the same place the
existing media verdict for the call already appears — so an operator
investigating a bad call sees the far end's own measurement, not just this
side's packet counts.

**Why this priority**: Equal-first with US1 because it is the half that
pays this project back directly. Every one-way-audio investigation in
this repo's history has reconstructed from packet counters what a single
receiver report states outright. It is independently valuable: even with
no report of our own being sent, reading the far end's would be an
improvement.

**Independent Test**: With a call up, have the far end send a receiver
report carrying known loss and jitter figures, and confirm those exact
figures reach the call's media reporting — visible to an operator without
reading a packet capture.

**Acceptance Scenarios**:

1. **Given** a call in progress, **When** the far end sends a receiver
   report, **Then** the loss and jitter it states are recorded against
   that call and appear in the call's media reporting.
2. **Given** a call in progress, **When** the far end's report allows a
   round-trip time to be derived, **Then** that figure is recorded and
   reported alongside loss and jitter.
3. **Given** a call whose far end never sends any report, **When** the
   call ends, **Then** the media reporting says the far end's view is
   unavailable — clearly distinguished from "the far end reported zero
   loss".
4. **Given** a malformed or truncated report arrives, **When** it is
   processed, **Then** it is discarded with a diagnostic and the call
   continues undisturbed — a bad report never affects media.
5. **Given** a series of calls with differing quality, **When** the
   metrics surface is scraped, **Then** loss, jitter and round-trip time
   are observable as trends across those calls, under names and labels
   consistent with the existing metrics and with bounded cardinality.
6. **Given** a call under live investigation, **When** each far-end
   report arrives, **Then** it is observable at diagnostic verbosity as
   it happens — quality across the call's duration, not one figure at the
   end.

---

### User Story 3 - This side's own receive quality is measured on every call (Priority: P2)

The loss, jitter and reordering this bridge observes on each incoming
stream is measured for every relayed call — both so it can be stated in
the reports the bridge sends to the far end, and so it appears in the
call's own media reporting next to the far end's figures.

**Why this priority**: The measurement machinery already exists and is
already correct (`media_stats::ReceiveTracker`, including RFC 3550 §6.4.1
jitter); it is simply not connected to the relay paths, which see only
packet counts today. Lower than P1 because a sender report alone satisfies
the conformance obligation — but without this, the bridge cannot state
what *it* received, and half the diagnostic value is still missing.

**Independent Test**: Relay a call with deliberately lossy and jittery
input, and confirm the call's media reporting states loss, reordering and
jitter figures matching what was actually injected — on both relay paths
(pass-through and transcoding).

**Acceptance Scenarios**:

1. **Given** a relayed call with packet loss on an incoming stream,
   **When** the call ends, **Then** the media reporting states the loss
   observed on that stream, distinct from the existing both-ways verdict.
2. **Given** a relayed call with out-of-order arrivals, **When** the call
   ends, **Then** reordering is reported as reordering, not as loss.
3. **Given** a call relayed by the pass-through path and an otherwise
   identical call relayed by the transcoding path, **When** each ends,
   **Then** both report receive quality in the same form — the two paths
   do not diverge in what they measure.

---

### User Story 4 - The far end's stated RTCP port is honoured (Priority: P3)

An offer that names an explicit RTCP port sends and receives RTCP on that
port, rather than on whatever port the default convention would imply.

**Why this priority**: The default convention covers the overwhelming
majority of peers, and no carrier this project talks to has been observed
sending an explicit RTCP port attribute. But sending reports to the wrong
port is worse than a clean gap: the far end still sees silence, and this
side believes it is compliant. Small, contained, and closes the sibling
finding (SDP-06's `a=rtcp` half) that was deferred alongside RTP-01.

**Independent Test**: Answer an offer that names an explicit RTCP port
different from the default, and confirm reports are sent to that port.

**Acceptance Scenarios**:

1. **Given** an offer naming an explicit RTCP port, **When** the bridge
   sends a report, **Then** it goes to that port rather than the
   convention-derived one.
2. **Given** an offer with no explicit RTCP port, **When** the bridge
   sends a report, **Then** it goes to the conventional port, exactly as
   for every call today.
3. **Given** an offer whose explicit RTCP port is malformed or
   unusable, **When** the offer is processed, **Then** the bridge falls
   back to the convention rather than failing the call.

---

### User Story 5 - A call that ends says so on the media path too (Priority: P3)

When a call ends, the bridge signals on the RTCP path that its source is
leaving, rather than simply going silent.

**Why this priority**: RFC 3550 §6.6 expects it, and it lets a far end
release resources promptly instead of waiting out a timeout. But the
practical cost of omitting it is small — the session is already being torn
down by SIP — and it is the one part of this feature that needs teardown
to become synchronous, which is a change to how every call ends. Lowest
priority, and severable if it proves to carry more risk than value.

**Independent Test**: End a call and confirm a leaving-source indication
is sent on the RTCP path before the media sockets are closed.

**Acceptance Scenarios**:

1. **Given** a call in progress, **When** it ends by any route (either
   side hanging up, or the bridge tearing it down), **Then** a
   leaving-source indication is sent on the RTCP path before the socket
   closes.
2. **Given** a call whose teardown cannot send that indication (the socket
   is already gone, the network is unreachable), **When** it ends,
   **Then** teardown completes normally — the call still ends, promptly,
   and the failure is logged rather than propagated.
3. **Given** any call, **When** it ends, **Then** teardown completes
   within the time it takes today plus a small bounded margin — adding
   this must not make hanging up feel slower or risk wedging a line.

---

### Edge Cases

- **The RTP socket's port is odd, or its neighbour is taken.** Sockets
  bind ephemeral today, so neither the parity the convention assumes nor
  the availability of the adjacent port is guaranteed. The bridge must
  end up with a usable RTCP port and an answer that truthfully states
  where it is — it must never declare one port and listen on another.
- **The RTCP port cannot be obtained at all.** Media must still flow: a
  call that would have worked before this feature must not now fail
  because a second socket could not be bound. The call proceeds, the
  answer is unchanged (the bandwidth declaration stays — FR-017), and the
  shortfall surfaces as a warning and a metric rather than as an altered
  SDP the far end could not interpret anyway.
- **The far end sends RTCP we did not ask for, or of a type we do not
  handle** (application-defined, extended reports, compound packets with
  unfamiliar members). It is ignored without disturbing the call, and
  never mistaken for a report we do understand.
- **RTCP arrives from an address that is not the call's negotiated
  peer.** Discarded before parsing, with a diagnostic; it never reaches
  the call's figures or the metrics they feed (FR-010a). This matters more
  than it would for a log line alone — those metrics persist and drive
  alerting.
- **A correctly-addressed report names a source identifier not yet seen
  on the media stream.** Accepted, not rejected — this is exactly what a
  legitimate report looks like immediately after a mid-call source restart
  (FR-010b).
- **A source restart mid-call** (the SSRC change batch 5's RTP-04 already
  logs). Reporting must survive it and stay coherent rather than reading
  the restart as catastrophic loss.
- **A very short call** — shorter than one reporting interval. It ends
  without a periodic report having been due; that is correct, and must not
  be logged as a fault.
- **A very long call** — past the point where sequence numbers wrap
  (about 22 minutes at 20ms framing). Counts must stay correct across the
  wrap, as the existing receive tracker already handles.
- **Media never establishes** (an answered call where no audio arrives).
  The reporting path must not depend on having received a first packet in
  order to function.
- **Both relay paths.** Every guarantee above holds identically whether
  the call took the pass-through relay or the transcoding relay. Divergence
  between the two is itself a defect — this review has already found one
  (RTP-03) and must not introduce another.

## Requirements *(mandatory)*

### Functional Requirements

**Sending reports**

- **FR-001**: The bridge MUST send periodic sender reports for each media
  leg in scope, for the whole duration of every answered call, describing
  the media it has transmitted on that leg.
- **FR-002**: Each report MUST be sent under the source identifier that is
  actually present on the media the bridge is sending to the carrier, so
  the carrier can correlate the report with the stream it describes.
- **FR-002a**: That identifier MUST remain stable for as long as the
  stream it describes does. It MAY change only when the underlying source
  itself changes (a mid-call source restart, which the bridge already
  detects and logs) — never for any other reason, and never between two
  reports describing one uninterrupted stream.
- **FR-002b**: Where the bridge originates the media it sends (the
  transcoding path), the identifier MUST be the one it generates for that
  stream. Where it forwards media unchanged (the pass-through path), the
  identifier MUST be the one observed on the stream being forwarded — the
  bridge MUST NOT substitute an identifier of its own, because doing so
  would mean altering the media, which FR-021 forbids.
- **FR-003**: Each report MUST state cumulative packet and octet counts
  for what the bridge has sent on that leg, and those counts MUST be
  non-decreasing across the call.
- **FR-004**: The interval between reports MUST be derived from the RTCP
  sender bandwidth the bridge's own SDP declared and the average size of
  the compound packets it actually sends, so the declaration and the
  behaviour agree by construction rather than by coincidence.
- **FR-004a**: Each interval MUST be randomised within ±50% of that
  derived value (RFC 3550 §6.3.1), so reports from independent
  participants cannot fall into lockstep.
- **FR-004b**: The bridge MUST NOT implement dynamic member counting or
  timer reconsideration. Every media session it establishes has exactly
  two participants by construction, and that machinery exists to manage
  participant counts this bridge cannot have.
- **FR-005**: The bridge MUST keep sending reports on a leg that has
  carried no media, correctly stating that nothing was sent, rather than
  falling silent.

**Reading reports**

- **FR-006**: The bridge MUST receive and interpret reports the far end
  sends, extracting at minimum the loss and jitter the far end observed.
- **FR-007**: The bridge MUST derive a round-trip time from the far end's
  reports where the report contains what is needed to do so, and record
  it.
- **FR-008**: Figures taken from the far end's reports MUST reach the
  call's existing end-of-call media reporting, alongside this side's own
  measurements, without an operator needing to read a packet capture.
- **FR-008a**: The bridge MUST additionally record loss, jitter and
  round-trip time as metrics on its existing metrics surface, so the
  figures are trendable and alertable across calls and across lines —
  not only readable one call at a time.
- **FR-008b**: Those metrics MUST follow the naming and labelling
  conventions the existing metrics surface already uses, and MUST have
  bounded label cardinality — no label may carry a per-call, per-caller,
  or otherwise unbounded value.
- **FR-008c**: Each far-end report received MUST also be observable as it
  arrives, at a diagnostic verbosity, so a call being investigated live
  shows quality changing over its duration rather than only a single
  figure at the end.
- **FR-009**: The bridge MUST distinguish "the far end never reported"
  from "the far end reported zero loss" everywhere those figures are
  presented.
- **FR-010**: A malformed, truncated, unrecognised, or unexpected RTCP
  packet MUST be discarded with a diagnostic, and MUST NOT affect media,
  end the call, or corrupt the figures recorded for it.
- **FR-010a**: The bridge MUST accept RTCP only from the peer address the
  call's media is negotiated with. A packet from any other source MUST be
  discarded with a diagnostic before it is parsed, and MUST NOT reach the
  figures recorded for the call or the metrics derived from them.
- **FR-010b**: Acceptance MUST NOT additionally require the packet to
  name a source already seen on the media stream. A legitimate report can
  name a source the bridge has only just begun receiving — immediately
  after a mid-call source restart — and rejecting it would discard the
  very evidence such a restart makes valuable.

**Measuring this side**

- **FR-011**: The bridge MUST measure loss, reordering and jitter on the
  media stream arriving from the carrier, whichever relay implementation
  carries the call (pass-through or transcoding), using the same
  measurement for both.
- **FR-012**: Those measurements MUST appear in the call's media
  reporting, and MUST be reported in the same form regardless of which
  relay path carried the call.
- **FR-013**: Reordered arrivals MUST be reported as reordering, not as
  loss.

**Ports and negotiation**

- **FR-014**: The bridge MUST attempt to obtain a port for RTCP on each
  media leg in scope, and wherever its SDP answer names an RTCP port, that
  MUST be a port it is actually listening on — the port it declares and
  the port it uses MUST always be the same. Failure to obtain one is
  governed by FR-017.
- **FR-015**: The bridge MUST use an RTCP port the far end's offer
  explicitly names, when it names one, in preference to the default
  convention.
- **FR-016**: An offer's RTCP port attribute that is malformed or
  unusable MUST cause a fall back to the default convention, never a
  failed call.
- **FR-017**: If no RTCP port can be obtained at all, the call MUST still
  proceed with media, and the SDP answer MUST be exactly what it would
  have been anyway — including the `b=RS:`/`b=RR:` declaration, which
  states intended bandwidth per TS 26.114 §6.2.10 and MUST NOT be made
  conditional on a local socket succeeding.
- **FR-017a**: That shortfall MUST instead be surfaced as a fault: a
  warning identifying the affected call, and a metric, so a bridge
  silently running without RTCP is visible rather than indistinguishable
  from a healthy one.
- **FR-017b**: Where no RTCP port was obtained, no explicit RTCP port
  attribute MUST be emitted — the bridge MUST NOT name a port it is not
  listening on (FR-014 holds unconditionally).

**Teardown**

- **FR-018**: When a call ends, the bridge MUST signal on the RTCP path
  that its source is leaving, before the media sockets are closed.
- **FR-019**: A failure to send that indication MUST NOT delay or block
  teardown; the call MUST still end promptly and the failure MUST be
  logged.
- **FR-020**: Teardown time MUST remain bounded and comparable to today's
  — adding RTCP MUST NOT introduce a path where a call takes materially
  longer to end, or where a line can wedge waiting on media cleanup.

**Non-regression**

- **FR-021**: Media relaying MUST be unaffected: audio and DTMF forwarding
  behaviour, the existing both-ways verdict, and the existing SSRC-change
  logging MUST all behave exactly as they do today. FR-002a consumes the
  SSRC-change signal to decide which identity to report under; consuming
  it MUST NOT alter what it detects or logs.
- **FR-022**: Existing SDP answers MUST be unchanged except for the
  addition of a truthful RTCP port statement where one is needed —
  including the `b=AS:`/`b=RS:`/`b=RR:` lines, which remain as they are.

**Scope boundaries**

- **FR-023**: The feature MUST apply to the **carrier-facing media leg of
  answered (terminating) calls** and to nothing else. The internal
  host-side leg to this project's own PJSIP, the originated (outbound)
  call path, and the standalone diagnostic call tool are all out of scope.
- **FR-023a**: The internal host-side leg MUST be left exactly as it is,
  including its own SDP answer's unbacked RTCP bandwidth declaration.
  That leg's peer is this project's own software on a lossless
  point-to-point link inside the host — no conformance obligation is owed
  to it and no diagnostic value is being lost — so the declaration
  remaining unbacked there is a known, deliberate residue of this feature,
  to be recorded as such rather than silently left implying RTP-01 is
  fully closed everywhere.
- **FR-024**: RTCP extended reports (XR), feedback profiles (AVPF), and
  any RTCP-derived adaptation of the media itself (rate adaptation,
  reactive codec change) are explicitly out of scope. The bridge measures
  and reports; it does not react.

### Key Entities

- **Report interval**: how often the bridge emits a report on a leg —
  derived from the declared sender bandwidth and the average compound
  packet size, then randomised ±50% around that value for each individual
  interval. Carries no member count and no reconsideration state.
- **Send accounting**: per media leg and per sending direction, the
  cumulative packets and octets the bridge has transmitted, plus the
  timing needed to place a report on the media clock.
- **Reported source identity**: the identifier a report is sent under —
  always the one present on the media the bridge is actually sending to
  the carrier. Sourced two ways depending on the relay path: generated by
  the bridge and exposed for reporting where it originates the media, or
  observed from the forwarded stream where it does not. Follows a mid-call
  source restart rather than outliving it.
- **Receive quality**: what this side observed on an incoming stream —
  received, lost and reordered counts, and jitter. Already modelled and
  computed in the codebase; currently unconnected to the relay paths.
- **Far-end quality**: what the far end reported it observed — loss,
  jitter, and a derived round-trip time. Distinguishable from "never
  reported".
- **RTCP endpoint**: the local port RTCP is served on and the remote port
  it is sent to, per leg — either derived by convention or taken from the
  peer's explicitly stated port.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: On every answered call lasting longer than one reporting
  interval, reports are observed leaving the bridge — 100% of such calls,
  with no call where the RTCP path is silent for the whole call.
- **SC-002**: Over a call of at least two minutes, the mean observed
  interval between reports is within 10% of the value derived from the
  declared sender bandwidth, and every individual interval falls inside
  the ±50% randomisation band around it — measurable from a capture of one
  call, with no reference to the implementation.
- **SC-003**: Where the far end sends reports, the loss and jitter it
  states are visible in the call's media reporting for 100% of such calls,
  without reading a packet capture.
- **SC-004**: An operator diagnosing a one-way-audio call can state, from
  the call's reporting alone, whether the far end saw loss on what we sent
  — a question that today requires reconstructing from packet counters.
- **SC-004a**: Loss, jitter and round-trip time are queryable as trends
  over time across calls and lines, so a line degrading gradually is
  visible before any single call is reported as bad — impossible today at
  any granularity.
- **SC-005**: Calls that succeed today continue to succeed: audio flows
  both ways, DTMF registers, and the both-ways verdict is unchanged, on
  both relay paths, verified on real hardware.
- **SC-006**: No call fails, and no call's media degrades, because RTCP
  could not be set up — a bridge that cannot obtain an RTCP port still
  carries the call, and the fact that it is running without RTCP is
  visible from its own warnings and metrics rather than having to be
  inferred from silence on the wire.
- **SC-007**: Hanging up remains as fast as it is today, within a small
  bounded margin, with no case where a line is left occupied waiting on
  media teardown.

## Assumptions

- The obligation this closes is toward the peer that reads our SDP; the
  bridge is not required to *act* on what reports say (no rate adaptation,
  no reactive codec change) — measuring and reporting is the whole scope.
- The declared values `b=RS:800`/`b=RR:2400` stay as they are; they are
  the customary 3GPP defaults already cited in the code, and this feature
  makes them true rather than changing them.
- Report content follows the standard's own minimum for a sender report
  plus the source description needed to make it well-formed; nothing
  beyond that is required to satisfy the finding.
- The existing receive-quality measurement in the codebase is correct as
  written (it is already unit-tested against loss, reordering, wraparound
  and jitter) and is reused rather than reimplemented.
- Real hardware verification will exercise the ordinary path — one
  answered call on the real line, both relay paths if the negotiation
  allows — consistent with how every prior batch of this review was
  verified. Scenarios needing a specific offer shape no carrier here has
  been observed sending remain unit-tested only.
- The pass-through relay forwards the source's own stream identity rather
  than originating one, and continues to (FR-002b) — reporting adapts to
  the media, never the other way round. The mid-call source-restart
  detection this relies on already exists (batch 5, RTP-04) and is reused
  rather than rebuilt.

## Dependencies

- Batch 5 (`specs/044-complete-media-contract/`) established the
  SSRC-change logging and the DTMF payload-type handling this feature must
  leave undisturbed; its `research.md` Decision 1 records why RTCP was
  deferred to here and what it will need.
- `docs/plans/mt-conformance-findings.md` tracks RTP-01 and SDP-06's
  `a=rtcp` half as open; both are closed by this feature and must be
  updated when it lands.
- The existing metrics surface (`src/metrics/`) and the rename-guard test
  suite that governs it (`tests/test_metric_renames.rs`) — FR-008a's new
  metrics join both, following the established `HistogramVec`/`GaugeVec`
  patterns and their bounded label sets.
- The end-of-call reporting path (`ims::agent::call::report_answered_call_ended`)
  emits both the "call media verdict" log line and the observability call
  today; FR-008 and FR-008a extend that one place rather than adding a
  second reporting route.

## Out of Scope

- Reacting to reported quality in any way — rate adaptation, codec
  renegotiation, or ending a call on reported loss.
- RTCP extended reports (XR), the AVPF feedback profile, and any secure
  profile (SRTCP): this bridge negotiates plain `RTP/AVP` only and
  declines anything else (batch 4, SDP-03).
- RTCP multiplexed onto the RTP port (`a=rtcp-mux`): not offered by any
  peer here, and it interacts with the port handling this feature is
  establishing.
- Changing the declared `b=AS:`/`b=RS:`/`b=RR:` values.
- RFC 3550 §6.3's multiparty scheduling machinery — dynamic member
  counting, timer reconsideration, reverse reconsideration on BYE
  (FR-004b). Every session here is two-party by construction.
- The internal host-side (veth) leg to this project's own PJSIP, in both
  directions — no RTCP is added there, and its answer keeps declaring
  RTCP bandwidth it does not use (FR-023a).
- The originated (mobile-originated) call path and its three relay call
  sites. RFC 3550 makes RTCP obligatory of every participant regardless of
  which side placed the call, so this is a deliberate scope cut rather
  than a claim that no obligation exists — it stays tracked as such.
- The standalone `ims-call` diagnostic CLI path's own RTP session, which
  is a separate tool rather than a relayed call.
