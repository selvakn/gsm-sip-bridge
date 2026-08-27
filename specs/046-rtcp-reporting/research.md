# Phase 0 Research: RTCP reporting on the carrier media leg

No `NEEDS CLARIFICATION` markers remained in the spec — five were resolved
in the `/speckit-clarify` session (see `spec.md`'s Clarifications). Phase 0
resolves the two decisions the spec deliberately left to planning (the RTCP
port mechanism, and how to satisfy FR-018 within FR-020's bound), plus the
mechanism for every other requirement, verified against current source.

Two findings below (**Decision 2** and **Decision 8**) contradict
assumptions the spec carried forward from batch 5's deferral note. Both
make the work *smaller* than the spec anticipated. They are called out
explicitly rather than quietly acted on.

---

## Decision 1: Three-tier RTCP port strategy — RTP+1, then declared, then none

**Decision**: Bind the RTCP socket in three descending tiers:

1. **RTP+1 by convention.** Bind the RTP socket ephemerally as today, then
   attempt to bind `rtp_port + 1` on the same address. If the RTP port is
   odd or its neighbour is taken, close both and retry the whole pair, up
   to a bounded number of attempts. On success the answer is byte-identical
   to today's — no `a=rtcp` attribute is emitted at all.
2. **Any ephemeral port, declared.** If the bounded retry fails, bind any
   ephemeral port and emit `a=rtcp:<port>` (RFC 3605) in the answer.
3. **None.** If even that fails, proceed with no RTCP: the answer is
   unchanged (including `b=RS:`/`b=RR:` — FR-017), and the shortfall is
   raised as a warning plus a metric (FR-017a). No `a=rtcp` is emitted
   (FR-017b).

**Rationale**: The two mechanisms have opposite failure modes, and tiering
them takes the better half of each.

RTP+1 is understood by every RTP peer that exists, including ones predating
RFC 3605 — it is the RFC 3550 §11 default. `a=rtcp` is understood only by
peers that implement RFC 3605; one that does not will send its reports to
RTP+1 regardless of what our answer said, and we would not be listening
there. So the convention is strictly safer *when we can get it*, and the
declaration is strictly better than nothing when we cannot.

Tier 1 succeeding is also the only outcome that leaves the answer
byte-identical to today's. That matters more here than it would elsewhere:
this project has already been burned once by an SDP answer change (the
omitted `telephone-event` payload types that got every Jio call torn down
2ms after ACK with `SIP;cause=503;text="PO: SIP SDP Protocol Error."` —
`sdp.rs`'s own comment on `SdpOffer::dtmf`). Keeping the common path free of
any new attribute keeps that risk at zero for the calls that matter.

The retry is bounded rather than unbounded because an exhausted or hostile
ephemeral range must degrade to tier 2, not spin. Ten attempts makes the
probability of reaching tier 2 by chance negligible (each attempt has
roughly even odds on parity alone) while capping the work at ten binds.

**Alternatives considered**:
- **`a=rtcp` always, no RTP+1 attempt.** Simpler — one bind, one code path,
  no retry loop, and it is unambiguous to any peer that reads it. Rejected:
  it makes correctness depend on carrier RFC 3605 support that this project
  has never verified for any of its carriers, and it changes the answer on
  every call to buy that dependency.
- **RTP+1 always, fail the call if unobtainable.** Rejected outright by
  SC-006 — a bridge that drops real calls over a diagnostic socket is worse
  than one with a logged gap.
- **Bind RTP to a fixed even port from a configured range** so RTP+1 is
  guaranteed. Rejected under Simplicity (Principle V): it introduces port
  range configuration, exhaustion handling, and per-line allocation state
  to solve a problem the bounded retry solves with no new configuration and
  no new failure mode.
- **`a=rtcp-mux`** (RTCP on the RTP port itself). Ruled out by the spec's
  own Out of Scope: no peer here offers it.

---

## Decision 2: The RTCP thread sends the RTCP BYE itself — teardown is not made synchronous

**Decision**: FR-018's leaving-source indication is sent by the per-call
RTCP thread when it observes `stop`, immediately before it exits and drops
its socket. The three teardown call sites (`call::handle_bye`,
`call::hangup_carrier`, `call::end_call_attachment_lost`) are **not
modified at all**.

**This contradicts the spec.** US5's *Why this priority* says the RTCP BYE
"needs teardown to become synchronous, which is a change to how every call
ends," and batch 5's deferral note listed "a synchronous teardown hook" as
one of four things RTCP would require. Direct inspection shows that is not
so, and the spec's cost estimate for US5 is therefore too high.

**Rationale**: All three teardown paths do exactly one thing to the media
layer — `call.stop.store(true, Ordering::Relaxed)` — and return
(`agent/call.rs:396`, `:434`, `:497`). The relay threads own their sockets
outright (moved in at spawn) and poll `stop` every `RELAY_POLL_INTERVAL`
(200ms, `transcode.rs:42` and `agent/veth.rs:27`).

An RTCP thread built the same way inherits that: it owns the RTCP socket,
it already knows the SSRC it has been reporting under, and it already wakes
on a short read timeout. Sending a BYE on the way out is a few lines inside
a thread that already exists, and it happens *after* the teardown caller has
already returned. Concretely:

- **FR-018** (sent before the sockets close) — satisfied: the socket is
  owned by that thread and closes when the thread drops it, strictly after
  the BYE is written.
- **FR-019** (a failure never blocks teardown) — satisfied trivially: the
  teardown path is not on this code path at all and cannot observe the
  failure, let alone block on it.
- **FR-020** (teardown stays as fast as today) — satisfied exactly, not
  approximately: teardown does the same single atomic store it does today.
  There is no new work on the hangup path to measure.

The BYE leaves within one poll interval (~200ms) of the stop flag being
set, asynchronously. RFC 3550 §6.6 sets no deadline on it.

**Consequence for planning**: US5 loses its "severable if riskier than
worth it" character. It is now among the *cheapest* items in the feature
rather than the most dangerous, and there is no longer a reason to
sequence it last for risk. It stays P3 by value, but it can land with the
RTCP thread that US1 has to build anyway.

**Alternatives considered**:
- **Join the relay threads at teardown and send the BYE from the caller.**
  This is what the spec assumed. Rejected: it puts a thread join on three
  hangup paths — including `end_call_attachment_lost`, which runs when the
  carrier attachment is already gone and a socket write may block — to buy
  nothing the thread-local approach does not already provide. It is the
  version of this change that could genuinely wedge a line, and it is
  avoidable.
- **Send the BYE from `on_idle_tick`.** Rejected: the tick has no access to
  the socket (moved into the relay thread at spawn), which is the original
  obstacle batch 5 identified. The RTCP thread sidesteps it by *being* the
  socket's owner.

**Follow-up (PR #66 review)**: this same not-joining-threads decision has
a second-order consequence Greptile's review caught twice: because
`report_answered_call_ended` signals `stop` but never joins the relay/
RTCP threads, the "final" figures it logs and reports as metrics can miss
whatever those threads process in the following ~200ms. Reordering to
signal `stop` before reading (landed in the PR) narrows this window but
cannot close it without joining — which is exactly what this Decision
already rejected, for the same reason (a join on `end_call_attachment_lost`
in particular risks blocking on a socket whose peer is already gone).
Documented as a permanent, deliberate trade-off on
`agent::call::report_answered_call_ended`'s own doc comment rather than
fixed, on the same reasoning as above: `MediaMeter`'s `carrier_rx`/
`pbx_rx` have had this identical characteristic since specs/016, five
batches before this one, with no prior finding against it.

---

## Decision 3: One RTCP thread per call, owning the socket, doing all four jobs

**Decision**: A single thread per call, spawned alongside the relay,
owning the RTCP socket, with a short read timeout, looping on:

1. `recv_from` → validate source → parse → update far-end quality.
2. If the report interval has elapsed → build and send a compound SR.
3. If `stop` is set → send the BYE (Decision 2) and exit.

**Rationale**: It is the smallest structure that satisfies the
requirements, and it is the structure the codebase already uses everywhere
media is handled — a thread per concern, owning its socket, polling a stop
flag on a read timeout (`transcode::relay_direction`, `veth::forward`,
`call.rs`'s receive thread all have exactly this shape).

Crucially it resolves batch 5's "per-call timer with access to the live
socket" obstacle without a timer at all: the read timeout *is* the timer.
A loop that wakes every few hundred milliseconds to service a socket can
check an interval deadline on the same wakeup for free.

**Alternatives considered**:
- **Two threads (one send, one receive).** Rejected: the send side needs
  the receive side's `last SR` bookkeeping for RTT, so they would share
  mutable state that a single thread holds as locals.
- **Drive reporting from the dispatch loop's existing ~100ms tick.**
  Rejected — the original obstacle: the tick cannot reach the socket. It
  would require passing a socket handle out to the dispatch loop, widening
  `ActiveCall` for no benefit over a thread that already owns one.
- **An async runtime for the media path.** Rejected under Simplicity: this
  codebase's media path is deliberately synchronous threads throughout;
  introducing a runtime for one socket is a structural change unrelated to
  the finding.

---

## Decision 4: Send accounting is an `Arc` of atomics, mirroring `MediaMeter`

**Decision**: A new shared `SendAccounting` — `AtomicU64` packets,
`AtomicU64` octets, `AtomicU32` SSRC, and the last RTP timestamp with the
`Instant` it was observed — cloned into whichever relay direction sends
toward the carrier, and read by the RTCP thread.

**Rationale**: This is precisely `MediaMeter`'s existing design
(`media_stats.rs`: `Arc<AtomicU64>` counters handed to each relay
direction, read elsewhere at teardown), which already solves the identical
problem for receive-side packet counts across both relay implementations.
Reusing the established shape means no new concurrency reasoning, no locks
on the per-packet path, and one pattern for a reader to learn rather than
two.

It also resolves two more of batch 5's four obstacles at once:

- **"Send-side octet counts don't exist"** — true, and this adds them.
  `MediaMeter` counts packets only, per *receive* direction.
- **"The SSRC is minted per thread and never returned"** —
  `transcode::RtpSender::new` does `ssrc: rand::random()` (`transcode.rs:298`)
  and keeps it. Publishing it into `SendAccounting` on first send exposes
  it without changing how it is generated.

**Alternatives considered**:
- **`Arc<Mutex<SendStats>>`.** Rejected: it puts a lock acquisition on
  every relayed packet to protect fields that are independent counters.
  Atomics are both faster and simpler here.
- **A channel from the relay to the RTCP thread.** Rejected: the RTCP
  thread needs a *snapshot at report time*, not a stream of every packet.
  A channel would make it drain thousands of messages per report for
  figures three atomic loads produce directly.

---

## Decision 5: The reported SSRC comes from `SendAccounting`, filled differently per relay path

**Decision**: Both relay paths publish into the same `SendAccounting.ssrc`
field; only the source differs.

- **Transcoding path**: `RtpSender` publishes its own minted SSRC on first
  send toward the carrier.
- **Pass-through path**: `veth::forward`'s carrier-bound direction
  publishes the SSRC it observes on each packet it forwards, which is the
  one actually reaching the carrier.

**Rationale**: This implements FR-002/002a/002b with one field and no
branching in the RTCP thread — it reads an SSRC, it does not care which
relay produced it. Per FR-002b the pass-through path must not substitute an
identity of its own, because that would mean rewriting the media, which
FR-021 forbids.

The mid-call source restart FR-002a permits is already detected: batch 5's
`rtp::SsrcTracker` (`rtp.rs`, with its own 5-second rate limit on logging)
sits in `veth::forward` today for exactly this. Publishing the new value
into `SendAccounting` when it changes is one line at a site that already
computes the change. FR-021's added clause is honoured: the tracker's own
detection and logging behaviour is untouched, it merely gains a consumer.

**Alternatives considered**:
- **Have the RTCP thread parse the media stream itself** to learn the
  SSRC. Rejected: it would need its own copy of the RTP socket and would
  duplicate parsing the relay already does per packet.
- **Reset send counters on an SSRC change.** Rejected: RFC 3550's sender
  report counts are per-source, but this bridge relays one continuous
  logical stream; resetting would misreport the call's totals. The counts
  stay cumulative and the SSRC field follows the wire — the honest reading
  of what a receiver correlates against.

---

## Decision 6: Receive quality reuses `ReceiveTracker` behind a mutex, per the existing precedent

**Decision**: An `Arc<Mutex<media_stats::ReceiveTracker>>` fed by the
carrier→veth relay direction on each received packet, read by the RTCP
thread when building a report and at teardown for the end-of-call figures.

**Rationale**: `ReceiveTracker` already computes everything US3 and the
report's receiver block need — received/lost/reordered counts and RFC 3550
§6.4.1 jitter — and is already unit-tested against clean streams, gaps,
reordering, sequence wraparound and uneven arrival (`media_stats.rs`'s test
module). It is used today only by `ims/call.rs:445`, which wraps it in
exactly this `Arc<Mutex<...>>` and feeds it from its receive thread. The
precedent is established; this extends it to the two relay paths.

A mutex is right here where atomics were right in Decision 4:
`on_packet` is `&mut self` over interdependent state (extended sequence
numbers, cycle count, running jitter), not independent counters. The lock
is uncontended in practice — one writer, and a reader that runs once per
report interval.

FR-011's "same measurement for both paths" is satisfied by construction:
one tracker type, fed identically from both.

**Alternatives considered**:
- **Reimplement the tracking inside the RTCP module.** Rejected outright —
  a second implementation of jitter and loss that could drift from the
  tested one, for no gain.
- **Have the RTCP thread read the media socket itself** to do its own
  tracking. Rejected: two readers on one socket is a race, and it would
  duplicate work the relay already does.

---

## Decision 7: Source validation is `recv_from` plus an IP check, not `connect()`

**Decision**: The RTCP socket is left unconnected. Each datagram arrives
via `recv_from`, and its source **IP address** is compared against the
call's negotiated peer IP. A mismatch is discarded with a rate-limited
diagnostic before any parsing.

**Rationale**: FR-010a says "the peer address". Checking the IP and not the
port is the literal reading, and it is the correct one: a peer that sends
its RTCP from a source port other than the one it receives on is unusual
but not wrong, and RFC 3550 does not require symmetry. Rejecting on port
would silently discard legitimate reports and reintroduce, at a different
layer, the "we see silence and cannot tell why" problem this feature
exists to end.

`connect()`ing the socket — as the RTP sockets do — would push filtering
into the kernel for free, but it filters on address *and* port, which is
the stricter behaviour just rejected. The RTP sockets can afford it because
they must send to exactly one place anyway; the RTCP socket has a genuine
reason to be more permissive about what it accepts than about where it
sends.

Per FR-010b, acceptance deliberately does **not** also require a known
SSRC. A report naming an SSRC we have only just started seeing is exactly
what arrives right after a legitimate mid-call source restart.

Discard logging is rate-limited on the model of `SsrcTracker`'s existing
5-second interval, so a misdirected or hostile sender cannot turn one
diagnostic into a log line per packet — the same reasoning recorded in
`rtp.rs`'s `SSRC_CHANGE_LOG_INTERVAL` comment.

**Alternatives considered**:
- **`connect()` the RTCP socket.** Rejected above (over-strict on port).
- **Accept anything on the port** (the spec's option C). Rejected in
  clarification: these figures now feed persistent metrics that drive
  alerting, so anything that can reach the port could write into the
  evidence base.
- **Cryptographic authentication of RTCP.** Not applicable — the carrier
  leg already runs inside the IPsec tunnel, which is what makes the address
  check meaningful rather than cosmetic; SRTCP is explicitly out of scope.

---

## Decision 8: Metrics travel as a new closed `ObservedEvent` variant — cardinality is bounded by construction

**Decision**: A new `ObservedEvent::MediaQuality { ... }` variant in
`src/control/protocol.rs` carrying `f64` measurements and closed-enum
label fields, a matching arm in `metrics::ingest::apply_event`, new metric
definitions in `src/metrics/mod.rs` following the existing
`HistogramVec`/`CounterVec` patterns, and coverage in
`tests/test_metric_renames.rs`.

**Rationale**: Agent A runs inside a network namespace and serves no
`/metrics` endpoint of its own. Everything it observes reaches Prometheus
through the control protocol's `ObservedEvent` enum, which
`metrics/ingest.rs:554`'s `apply_event` translates into metric operations.
There is no shortcut — a metric cannot be set directly from the IMS agent.

FR-008b (bounded label cardinality) needs no new mechanism, because the
protocol already enforces it architecturally. `ObservedEvent`'s own doc
comment states the rule: *"Every enumerated field below is a closed Rust
enum rather than a free string — the mechanism that keeps metric label
cardinality bounded regardless of what an agent observes."* Following the
established pattern satisfies FR-008b by construction; violating it would
require deliberately introducing a `String` field, which the surrounding
code makes conspicuous.

**Note on scope**: this chain — protocol variant, ingest arm, metric
definitions, rename-test coverage — is the concrete cost of the
clarification session's Q3 answer (metrics *and* logs, rather than logs
alone). It is recorded in the spec's Dependencies. It is four small edits
in four files, but they span a serialized wire protocol, so they belong in
one commit with the rename test.

**Alternatives considered**:
- **Have Agent A export its own `/metrics`.** Rejected: it would need a
  second HTTP endpoint reachable from outside a network namespace, which is
  the exact problem the control-protocol reporting channel was built to
  solve (recorded in `ObservedEvent::OutboundAttempt`'s own comment).
- **A free-string label for the quality dimension.** Rejected: violates
  FR-008b and the protocol's stated design rule.
- **Per-call labels** (call ID, caller). Rejected: unbounded cardinality,
  explicitly forbidden by FR-008b. Per-call detail belongs in the log
  line (FR-008), which is why the clarification chose both surfaces.

---

## Decision 9: Report cadence is computed once per call, randomised per interval

**Decision**: Derive the base interval from the declared sender bandwidth
(`b=RS:800`) and the measured average compound packet size, then randomise
each individual interval within ±50% of it (RFC 3550 §6.3.1). No member
count, no timer reconsideration (FR-004b).

**Rationale**: This is the clarification's answer, and the mechanism is
small: the RTCP thread already wakes on its read timeout, so "is the
current randomised deadline past?" is one comparison per wakeup. The
average packet size is knowable because the thread builds the packets it
sends and can keep a running mean of their sizes.

Skipping reconsideration is justified the way batch 3 justified skipping a
generic RFC 3261 §17 transaction table: every mechanism it adds exists to
manage participant counts this bridge cannot have. A media session here is
two-party by construction — one carrier leg, one relay.

SC-002 is directly measurable from a capture: mean interval within 10% of
the derived value, every individual interval inside the ±50% band.

**Alternatives considered**:
- **Fixed 5s** (RFC 3550's nominal floor). Rejected in clarification: it
  would leave the declaration overstating actual usage roughly fivefold.
- **Full RFC 3550 §6.3 scheduling.** Rejected above and in clarification.

---

## Decision 10: Parameter growth goes into structs, not more positional arguments

**Decision**: The new per-call handles (`SendAccounting`, the receive
tracker, the far-end quality sink) are passed to the relays as a single
struct rather than as additional positional parameters.

**Rationale**: A practical constraint, not a stylistic preference.
`make lint` runs clippy `-D warnings` across the whole workspace including
test targets, and CLAUDE.md records that lint failures have broken commits
here before. Both relay entry points are already at the limit and carry
explicit suppressions: `transcode::relay_direction` has
`#[allow(clippy::too_many_arguments)]` at `transcode.rs:440` with ten
parameters, and `veth::forward` has one at `veth.rs:232` with seven.

Adding three more parameters to each would deepen a suppression that is
already papering over a signature that has outgrown positional arguments —
across `spawn_relay`, `relay_rtp`, `forward`, `spawn_transcoding_relay` and
`relay_direction`, and every call site in `agent/inbound.rs`. Bundling the
new handles into one struct adds a single parameter to each instead, and
leaves room to retire the existing suppressions later without touching this
feature's logic.

**Alternatives considered**:
- **Three more positional parameters each.** Rejected above.
- **Refactor all five signatures onto structs as part of this feature.**
  Rejected as scope creep — this feature adds one parameter and does not
  restructure what is already there. Noted as a follow-on.

---

## Resolved: what batch 5 said RTCP would need

Batch 5's `research.md` Decision 1 listed four blockers. All four are
resolved, three of them more cheaply than that note anticipated:

| Batch 5's blocker | Resolution |
| --- | --- |
| Send-side octet counts don't exist | Added as atomics alongside the existing packet counters (Decision 4) |
| SSRC minted per thread, never returned | Published into shared accounting on first send; pass-through observes it instead (Decision 5) |
| Per-call timer with live socket access | No timer needed — the RTCP thread's own read timeout is the clock (Decision 3) |
| Synchronous teardown hook | **Not needed at all** — the RTCP thread sends the BYE itself on its way out (Decision 2) |

The scope reduction from the clarification session (carrier leg of
answered calls only, FR-023) removes the other half of batch 5's stated
cost: "all three relay call sites and both relay implementations." Both
relay *implementations* are still in scope — the carrier leg takes either
one depending on codec negotiation — but only the one call site in
`agent/inbound.rs`, not the three in `agent/origination.rs`.
