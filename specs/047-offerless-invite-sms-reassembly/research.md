# Research: Offerless Call Answering and Multi-Part SMS Reassembly

Ten decisions, split by finding. Both findings were deferred out of batch 6
for the same stated reason — "needs new state the system doesn't hold
today" — so the point of this phase was checking how much of that state
already exists elsewhere in the codebase to be reused, the same question
batch 7's research answered for RTP-01.

## SDP-04 — offerless call answering

### Decision 1: recognize "offerless" as an empty/whitespace body, before `parse_offer` runs

Today, `handle_invite` (`ims/agent/inbound.rs:177`) calls
`sdp::parse_offer(&req.body)` unconditionally once `invite_content_type_supported`
passes. On a body with no `m=audio` line at all (the offerless case),
`parse_offer` runs to completion and returns `Err(BridgeError::Ims("SDP
offer missing c= connection address"))` — the same generic error a
genuinely malformed offer produces. That `?` propagates out of
`handle_invite` as a hard `BridgeResult` error, which (per `LoopState`'s
caller) is logged and drops the request with no SIP response at all: from
the caller's perspective indistinguishable from a line that never answers.

**Decision**: check `req.body.trim().is_empty()` immediately after the
`Content-Type` gate and before calling `parse_offer`, and branch to a new
offerless path rather than let it fall into `parse_offer`'s error. A
non-empty body that still fails `parse_offer` (genuinely malformed SDP)
keeps today's existing behavior unchanged — this finding is specifically
about the *absence* of an offer, not about being more lenient with a bad
one.

**Alternatives considered**: teaching `parse_offer` itself to return a
distinguishable `NoOffer` variant instead of a generic error. Rejected —
`parse_offer`'s contract is "parse this body as an offer"; a body with no
`m=` line is not almost an offer, it's a different RFC 3264 §3 case
entirely, and folding that into the parser's error type would make every
existing caller of `parse_offer` (including the origination path via
`ims::call`, which never encounters a genuinely offerless body) handle a
variant that doesn't apply to it.

### Decision 2: reuse `sdp::build_offer` / `sdp::parse_answer` as-is; DTMF and RTCP stay out of scope for this path

`ims::sdp` already has exactly the pair this needs — `build_offer`
(constructs a PCMU+AMR-WB offer, used today by `agent::origination.rs` and
`ims::call` when *this bridge* places an outbound call) and `parse_answer`
(the inverse: reads a peer's SDP answer down to `remote_rtp` and the
selected codec, by matching payload type against the fixed `0`/`96` this
bridge's own offers always use). Both are already unit-tested and already
carry live traffic on the origination path.

**Decision**: reuse both unchanged for the offerless-INVITE case — we are
in the same offerer role `origination.rs` is, just reached via a `200 OK`
to an inbound `INVITE` instead of via our own outbound `INVITE`.
`origination.rs`'s private `offered_chosen_codec` helper (mapping the
answer's `NegotiatedCodec` to a full `ChosenCodec` — payload type, framing,
no DTMF payload type) is promoted to `pub(crate)` in `ims::sdp` itself
(next to `build_offer`, since the two are tightly coupled: the mapping is
only correct because it mirrors exactly what `build_offer` offers) and
shared by both call sites instead of duplicated.

**Consequence, recorded as deliberate scope, not an oversight**: neither
function offers `telephone-event` (RFC 4733 DTMF) or declares RTCP
(`b=RS`/`b=RR`, `a=rtcp`) — `build_offer` never has, on the origination
path it already serves. An offerless call answered this way therefore gets
audio only: no DTMF relay, and no RTCP reporting for that specific call,
even though it is otherwise within batch 7's "carrier-facing leg of
answered calls" scope (FR-023). This is the same shape of residue that
FR-023a already recorded for RTP-01 (RTCP not reaching the originated-call
path) — an explicit, documented gap rather than a silent one. Extending
`build_offer` to also declare RTCP/DTMF was considered and rejected for
this batch: it would mean either changing a function three other call
sites depend on (risking the origination path, which is out of scope
here) or forking a second, richer offer-builder — real new work with no
evidence any carrier here has ever sent an offerless INVITE at all yet, let
alone one where DTMF-during-that-call has been observed missing. Tracked
as **FR-002a** below and carried into `docs/plans/mt-conformance-findings.md`
the same way FR-023a was.

### Decision 3: wait for the ACK's answer by reusing the existing `inbound.rx` drain-loop pattern

`await_pbx_answer` already does exactly the shape of thing this needs:
while blocked (there, waiting on Agent B's control channel), it drains
`inbound.rx` in a loop, matching specific in-dialog requests (a `CANCEL`,
a retransmitted `INVITE`) by `Call-ID` and `CSeq`/tag, and ignoring
everything else. `LoopState` (`agent/mod.rs`) is confirmed single-threaded
around this: `handle_invite` is called synchronously from the same loop
that otherwise owns `inbound.rx`, so nothing else consumes it while
`handle_invite` is running.

**Decision**: after sending the `200 OK` that carries our own offer, add a
second bounded drain loop (same shape as `await_pbx_answer`'s) watching for
an `ACK` naming this call's `Call-ID`, whose `CSeq` number matches the
original `INVITE`'s (RFC 3261 §17.1.1.3: an ACK to a 2xx reuses the
INVITE's CSeq number with method `ACK`) and whose `From` tag matches the
caller's. Its body is handed to `parse_answer`.

**Alternatives considered**: a generic "any in-dialog request" waiter
shared with `await_pbx_answer`. Rejected as premature — the two waits
watch for different things (a `CANCEL`/retransmit vs. one specific `ACK`)
and merging them would add a parameterized abstraction for two call sites,
against Principle V.

### Decision 4: on ACK timeout or an incompatible answer, tear down with a BYE built from the already-available `DialogInfo`

By the time we are waiting for the ACK, the PBX extension has already
answered (the `200 OK` was sent) and Agent B has already been told
`IncomingCall`/bridged. If the ACK never arrives, or its answer names no
codec we offered, RFC 3261 gives no mechanism to retract the `200 OK` —
the dialog is confirmed. The conventional recovery (and the one this
codebase already has the pieces for) is to end the now-orphaned dialog with
a `BYE`.

**Decision**: build the `BYE` from `DialogInfo::from_invite(req, &to_tag,
session)` — already computed earlier in `handle_invite` for the eventual
`ActiveCall` — via its existing `build_bye_for`, send it on the carrier
transport, and send Agent B `ControlMessage::CallEnded` (the same message
`RingOutcome::Abandoned`'s handling already sends) so the PBX leg is torn
down too. No new BYE-building code; `hangup_carrier` itself isn't reusable
here because it takes an already-constructed `ActiveCall`, which doesn't
exist yet at this point — its two callers (`dialog`, `call_id`) are already
in scope directly.

### Decision 5: everything before codec/RTP-connect stays exactly as today; `veth_wideband` for the offerless path falls back to `ctx.wideband`

Ringing, the veth UAS listener, the control-channel exchange with Agent B,
and `await_pbx_answer` itself do not read anything from `offer` except one
line: `let veth_wideband = precheck.codec == NegotiatedCodec::AmrWb;` —
and `precheck` (the codec pre-selection) only exists because there was an
offer to select from. For the offerless path there is no `precheck` at
this point.

**Decision**: for the offerless path, seed `veth_wideband` from
`ctx.wideband` directly — the same signal `CodecOffer::preferring_wideband`
already uses to decide what *our own offer* leads with. This keeps the
veth leg's wideband/narrowband choice consistent with what we're about to
offer the carrier, without needing a codec decision that doesn't exist
yet.

## SMS-05 — multi-part reassembly

### Decision 6: `parse_concatenation_udh` must start returning the reference value it already reads and discards

`DecodedSms.part: Option<(u8, u8)>` is `(sequence, total)` only.
`parse_concatenation_udh` (`ims/sms_pdu.rs`) already reads the
concatenation IE's reference byte (`ie[0]` for the 8-bit IEI `0x00`, or
`ie[0..2]` for the 16-bit IEI `0x08`) but never returns it — only
`(seq, total)` survive. Reassembly's own correctness requirement
(FR-011: tell apart two concurrent multi-part messages from the same
sender) needs exactly the value that's being thrown away — TS 23.040
§9.2.3.24.1 states the reference, not the sender alone, is what scopes one
multi-part message's parts.

**Decision**: this is a prerequisite fix, not new functionality.
`parse_concatenation_udh` returns a small struct (reference as `u16` so
both IEI widths fit; total; sequence) instead of a bare tuple, and
`DecodedSms.part` carries it through. Every existing caller/test that
matches `Some((seq, total))` updates to the new shape — mechanical, but
touches real call sites (`ims/sms_pdu.rs`'s own tests, and wherever
`part` is read downstream for the "part N of M" label).

**Alternatives considered**: keying the reassembly buffer on
`(sender, total, first-seen-sequence)` instead of the real reference,
avoiding the parser change. Rejected — two back-to-back multi-part
messages from the same sender with the same total-part count (a common
case: someone sending two 3-part texts in a row) would collide under that
key, silently interleaving unrelated text. The reference field exists in
the standard precisely to prevent this; approximating around it would
reintroduce the exact class of bug this feature exists to fix.

### Decision 7: a new `Reassembly` type in `volte::sms`, shared cross-route the same way `Dedupe` already is

`Dedupe` already solves an adjacent problem (state that must be shared
between the IMS `MESSAGE` route and the modem-storage sweep route, per
CS-02's "the routes agree" requirement) with a proven shape: a small
struct behind `Arc<Mutex<_>>`, constructed once in `agent::mod`'s setup and
handed to both `InboundParams` (for `handle_message`) and
`run_modem_reader`.

**Decision**: add `Reassembly` next to `Dedupe` in `volte::sms`, following
the same construction/sharing pattern exactly — one
`Arc<Mutex<Reassembly>>`, built alongside `dedupe` in `agent::mod`'s setup,
threaded into both routes' existing parameter structs. Its core operation,
`admit_part(sender, reference, total, seq, text, rp_mr) -> PartOutcome`
(`Complete(String)` | `Pending` | `Malformed`), is pure and unit-testable
without a socket or a modem, matching `Dedupe::admit`'s own testing shape
and this project's Integration-First Testing principle (pure logic gets
direct tests; only the true I/O boundary — the sweep thread's timing —
is hardware/live-only).

**Alternatives considered**: a reassembly buffer local to each route
(IMS-side only). Rejected — the same underlying `ims::sms_pdu` TPDU
decoder already produces identical `DecodedSms` values from both routes
(CS-02's own regression guard proves this), and a real concatenated
message can arrive over either bearer; splitting the buffer would let a
message with one part over IMS and another over the modem's own storage
route never reassemble at all — an even worse version of the
now-labelled-but-unjoined status quo.

### Decision 8: expiry piggybacks on `LoopState::on_idle_tick`; no new timer

**Revised post-implementation (code review, 2026-08-28)** — the original
version of this decision piggybacked on `run_modem_reader`'s sweep loop,
reasoning that it "already runs unconditionally, once per line." That
premise is false: `wants_modem_sms_reader` (`agent::mod`) never spawns
that thread at all for a `pcsc_reader` line, which has no AT port to
sweep. A multi-part message buffered via the IMS `MESSAGE` route on such a
line would then never expire — only ever evicted by `Reassembly`'s
capacity bound under an unrelated flood — silently violating FR-013/
SC-004 for that line type. Caught and fixed before this batch was
reported done, not after.

`LoopState::on_idle_tick` is the periodic wakeup that actually satisfies
this decision's original intent: every line's dispatch loop calls it
unconditionally on every `recv_timeout` timeout, whether or not that line
has a modem — at `ACTIVE_CALL_POLL_INTERVAL` (100ms) during a call or
`IDLE_POLL_INTERVAL` (1s) otherwise, both comfortably under the 3-minute
(`SC-004`) expiry window.

**Decision**: `on_idle_tick` calls a new `flush_expired_reassembly(p)` — a
new free function in `agent::mod`, not a method, since it needs only
`&DispatchParams` — first thing, unconditionally, before any of the
function's own early returns (Gm probe skipped while busy, renewal not
due, deferred, backed off, ...) could otherwise skip it. It calls
`Reassembly::expire_due(now)`, moving every buffered entry whose
`last_updated` is older than the 3-minute bound into `Reassembly`'s own
retry queue (see the **second revision** immediately below), then attempts
delivery for everything currently queued. No new thread, no new timer —
the same "the existing wakeup already doubles as the clock" reasoning RTCP
batch 7's Decision 3 used for its own report cadence, just anchored to the
wakeup that's actually universal.

**Revised again (Greptile review, 2026-08-28, second round)** — the
version above (and the matching fix for capacity eviction/reference
reuse, Decisions recorded in `data-model.md`'s `PartOutcome` section)
still made only a *single* delivery attempt per flushed part, having
already removed it from `Reassembly` first. A transient control-channel
failure at that exact moment lost the content permanently — the same
failure shape the original eviction bug had, just narrowed to one unlucky
moment instead of removed. Fixed by giving `Reassembly` a small internal
retry queue (`QueuedFlush`, `pending: Vec<QueuedFlush>`): `expire_due`,
capacity eviction, and reference-reuse all move their content into it
rather than returning it directly; `on_idle_tick` drains it via a
non-destructive `ready_for_delivery()` snapshot and only dequeues an
entry (`mark_flush_delivered`) once its delivery actually succeeds — a
failed attempt leaves it queued for the next ~1s tick, self-healing
instead of losing the content on the first try. This is the same
peek-then-confirm shape `PartOutcome::Complete`/`mark_delivered` already
established for the ordinary completion path, applied consistently to
every other way a buffer's content leaves `Reassembly`.

**Alternatives considered**: a dedicated reassembly-expiry thread.
Rejected outright as the one thing this decision exists to avoid — a
second thread with its own lifecycle, watchdog registration, and shutdown
handling for a check that an already-running, already-watchdog-registered
thread can absorb for the cost of one more field read per buffered entry
per pass.

### Decision 9: completion and expiry-flush both deliver through the exact code path a single-part message already uses today

**Decision**: `handle_message` (IMS route) and the modem-sweep decode path
(CS route) both keep calling `Dedupe`/build `InboundMessage` exactly as
today for admission; the only new step is that when `part` is `Some`,
`admit_part` runs *before* the existing `ControlMessage::SmsReceived` send,
and that send only happens immediately if `admit_part` returned
`Complete` (with the joined text replacing the individual part's) — otherwise
nothing is sent yet (`Pending`), and Decision 8's sweep-flush is what
eventually sends it (individually, per part, on the malformed/timeout
path) or never (if it completes first). No new delivery mechanism, no new
`ControlMessage` variant.

### Decision 10: per-part network acknowledgment is untouched

Both routes' delivery-report step (RP-ACK for IMS, the implicit
modem-storage clear for CS) already happens independent of the
`Dedupe`/forward decision — it acknowledges *receipt*, not *forwarding*.
Reassembly only gates the forwarding step (Decision 9), so this needs no
change at all; it's recorded here only because FR-012 makes it an explicit
requirement and the review shouldn't have to re-derive that it's already
satisfied.

## Summary of scope reductions vs. the batch-6 deferral note

The original note estimated both findings as needing "new state the system
doesn't hold today" comparable to RTP-01. Phase 0 found:

- SDP-04 needs no new SDP-building/parsing code (Decision 2) and no new
  concurrency primitive (Decision 3 reuses an existing drain-loop shape) —
  the actual new code is the branch/wait/teardown wiring in one function,
  `handle_invite`.
- SMS-05 needs one real prerequisite fix (Decision 6) plus one new type
  that is a near-structural twin of one that already exists and is already
  shared exactly the way this feature needs (Decision 7), and no new
  thread (Decision 8).

No `NEEDS CLARIFICATION` remains.
