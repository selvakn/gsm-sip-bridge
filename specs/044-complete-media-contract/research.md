# Phase 0 Research: Complete the media contract on the relay legs

No `NEEDS CLARIFICATION` markers were left in the spec. Phase 0 resolves
the exact mechanism for each finding, verified against current source
(`src/ims/agent/veth.rs`, `src/ims/transcode.rs`, `src/ims/sdp.rs`) plus a
dedicated Explore pass on RTCP feasibility.

## Decision 1: RTP-01 (RTCP) and SDP-06's `a=rtcp` half are deferred, by explicit decision

**Decision**: Not attempted in this feature. Recorded in
`docs/plans/mt-conformance-findings.md` as deferred to its own future
feature, not as `[x]` done or `[-]` won't-fix.

**Rationale**: A dedicated research pass confirmed real RTCP needs, and
this codebase has none of today:
- Send-side **octet** counts (`media_stats::MediaMeter` tracks packet
  counts only, per receive direction, not bytes, and not per send
  direction).
- A **stable, exposed SSRC** — `transcode::RtpSender` mints a fresh random
  SSRC per relay-direction thread and never returns it; the pass-through
  relay has no `RtpSender`/SSRC concept at all (raw bytes forwarded
  as-is).
- A **per-call timer with access to the live RTP socket** — the dispatch
  loop's own tick (`agent::mod::on_idle_tick`) runs every ~100ms during a
  call, but the socket is moved into the relay thread(s) at spawn time and
  never retained anywhere the tick can reach.
- A **synchronous teardown hook** — `call::handle_bye`/`hangup_carrier`/
  `end_call_attachment_lost` all flip `stop` and immediately return; none
  of them hold the socket or SSRC needed to send a final RTCP BYE, and
  nothing joins the relay threads before returning.

Building this touches all three relay call sites (inbound, outbound,
veth) and both relay implementations (pass-through, transcoding) — a
scope and risk profile the user explicitly decided was out of proportion
with the rest of this batch, choosing to defer it (see the session's
`AskUserQuestion`) rather than build it now or remove the TS
26.114-mandated bandwidth declaration (which would trade one compliance
gap for another, since that declaration is required independent of
whether RTCP is actually exchanged).

**Alternatives considered** (all rejected by the user's explicit choice):
- Build minimal RTCP now — rejected: meaningfully larger and riskier than
  anything else in this review so far.
- Remove the `b=RS:`/`b=RR:` bandwidth lines — rejected: contradicts the
  TS 26.114 §6.2.10 mandate already cited in `sdp.rs` for why they were
  added; trades one gap for another rather than closing it.

## Decision 2: DTMF relabeling reuses `rtp::parse_packet`, rewrites one byte, forwards everything else untouched

**Decision**: `agent::veth::forward` gains `src_dtmf_pt: Option<u8>,
dst_dtmf_pt: Option<u8>` parameters. On each received datagram, it calls
the existing `rtp::parse_packet` (already correctly handles header
extensions/CSRC lists — the same parser the transcoding relay already
uses) to read `payload_type`. If it equals `src_dtmf_pt` and `dst_dtmf_pt`
is `Some` and differs, the payload-type byte (offset 1, low 7 bits —
RFC 3550 §5.1's marker bit in the high bit is preserved untouched) is
rewritten in place in the same buffer before forwarding; every other
packet (audio, or DTMF where the two PTs already match) is forwarded
exactly as today, unparsed beyond the one field read.

**Rationale**: `forward`'s whole design point is "both legs already agree
on the audio codec, so don't decode/re-encode" — that reasoning holds for
the audio payload type (RFC 3551 static assignment or already checked at
answer time) but never held for DTMF, which each leg's own SDP answer
picks independently (`sdp::build_answer_for`/`build_veth_answer`, each
calling `dtmf_pick` on its own offer's list). `ChosenCodec::dtmf_payload_type`
(added by batch 1's RTP-02 fix) already carries exactly what's needed at
every one of `spawn_relay`'s four call sites (verified directly:
`agent/inbound.rs:388`, `agent/veth.rs:140` (internally), and three sites
in `agent/origination.rs`) — both the carrier-facing and veth-facing
`ChosenCodec` are already in scope wherever `spawn_relay` is called today,
so threading two more `Option<u8>` parameters through is a signature
change only, no new state to compute.

**Alternatives considered**:
- Fully decode/re-encode DTMF on the pass-through path the way the
  transcoding relay does (its own `RtpSender`/independent timestamp run) —
  rejected: DTMF's *payload* (RFC 4733 event/volume/duration bytes) is
  identical regardless of codec; only the payload-type *label* differs
  between legs. Rewriting one byte is correct and strictly simpler than
  building a second DTMF re-origination path.
- Leave the passthrough relay fully unaware of RTP structure (status quo)
  — rejected, this is the bug.

## Decision 3: SSRC continuity is observability only — logged, never enforced

**Decision**: Both relay implementations track `last_ssrc: Option<u32>`
per direction (a local to each relay thread/loop, not shared state). On
each parsed packet, if `last_ssrc` is `Some` and differs from the packet's
own `ssrc`, log `info!`/`warn!` identifying the direction and old/new
SSRC; either way, update `last_ssrc` to the packet's value and continue
forwarding normally. The very first packet on a stream only sets
`last_ssrc` — it is never compared against a prior value, so it can never
itself be logged as a change.

**Rationale**: RFC 3550 treats an SSRC change mid-stream as a legitimate
signal (a source restart), not inherently an error — nothing in this
bridge's relay logic depends on SSRC continuity for correctness today
(neither relay does jitter buffering, reordering, or any other
SSRC-scoped state), so there is no existing behavior to *fix*, only
visibility to add. Dropping packets or ending the call on a change would
risk breaking a legitimate reconnection for a codebase-internal reason
nothing here currently needs — the spec's own FR-005 rules this out
explicitly.

**Alternatives considered**:
- Reject/drop packets from a new SSRC until some validation policy admits
  it (RFC 3550 §8.2's misdirected-packet guard) — rejected: no evidence
  this bridge has ever seen a spurious/hijacked stream, and the downside
  of a false positive (dropping a legitimate reconnection) is worse than
  the downside of logging-only.
- A generic multi-SSRC "source table" — rejected under Simplicity: this
  bridge only ever relays one stream per direction per call; a table
  keyed by SSRC solves a problem (concurrent sources) that doesn't exist
  here.

## Decision 4: `ptime` is a confirmed non-issue, not a bug to fix — reversed from the initial plan

**Decision**: No code change. `SdpOffer` does **not** gain a `ptime`
field, and the answer keeps stating its hardcoded `a=ptime:20`
unconditionally. A confirming test
(`the_answer_always_states_its_own_true_20ms_ptime_regardless_of_the_offers`,
`sdp.rs`) pins this as intentional, not an oversight.

**Rationale**: The initial plan for this decision (echo the offer's
`ptime` the same way `maxptime` is echoed) turned out, on closer
inspection, to be the *wrong* fix — it would have introduced a new
instance of exactly the defect this whole review exists to eliminate
(stating something the code doesn't do). The two attributes are not
analogous:

- `maxptime` is a **received-side upper bound**: the offerer stating "I
  can accept up to N ms per packet." Echoing it in our answer is a true
  statement — this bridge's fixed 20ms framing
  (`NegotiatedCodec::frame_samples`, whose own doc says "the ptime every
  leg here uses") is well under any `maxptime` an offer would plausibly
  send, so echoing it costs nothing and states a real constraint we
  genuinely respect.
- `ptime`, by contrast, describes **what the SDP body's own owner intends
  to send with**. An offer's `a=ptime` is the *offerer's* packetization
  plan for what it sends to us — not a request that our answer adopt the
  same value. This bridge's own packetization is a fixed, codec-level
  constant (`frame_samples()`, unconditionally 20ms at each codec's own
  rate) — it does not vary per call, per offer, or per anything. Echoing
  the offer's `ptime` into our own answer would state a packetization we
  do not actually use whenever the offer's value differs from 20.

Separately, decode correctness never depended on this: `transcode::relay_direction`
already decodes whatever's in each received packet directly (no
fixed-length assumption on the *input* side) and only chunks the
*output* into `dst_codec.frame_samples()`-sized frames — so an offer using
a non-default `ptime` was never actually a correctness problem to begin
with, only a value nothing read. Capturing it into `SdpOffer` with no use
for it would itself be exactly the kind of half-finished addition this
project's own conventions warn against.

**Alternatives considered**:
- Echo the offer's `ptime` into the answer (the original plan) —
  rejected once analysis showed this bridge's own packetization is fixed
  and not offer-driven: doing so would make the answer's `ptime`
  attribute false whenever it differs from 20, which is a regression, not
  a fix.
- Parse and log the offer's `ptime` for diagnostics only, without
  changing the answer — rejected: no other attribute in this file is
  captured purely for logging with no behavioral consumer; this would be
  new, unjustified surface area for a value nothing needs.
