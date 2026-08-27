---
description: "Task list for RTCP reporting on the carrier media leg (RTP-01)"
---

# Tasks: RTCP reporting on the carrier media leg

**Input**: Design documents from `/specs/046-rtcp-reporting/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/rtcp-wire-contract.md, quickstart.md

**Tests**: Following this codebase's own convention (every prior batch of
this review: in-module `#[cfg(test)] mod tests` beside the code, not
separate upfront contract-test files). Constitution Principle I is
NON-NEGOTIABLE, so pure logic tasks below include their unit tests as part
of the same task rather than a preceding TDD task — that is how
`media_stats.rs`, `rtp.rs`, and `sdp.rs` are already tested in this tree.

**Organization**: Grouped by user story per spec.md's priorities. Two
stories (US1, US2) are tied P1 by the spec's own reasoning.

## Path Conventions

Cargo workspace root is `gsm-sip-bridge/` (git repo root is one level up).
All paths below are relative to the repo root, matching plan.md's Project
Structure section.

## Implementation notes (2026-08-27)

T001-T032 and T034 landed; `make format && make lint && make test` clean
across the whole workspace. T033 (the hardware round) is **not done** —
this environment has no access to the real EC20 line; see
`docs/plans/mt-conformance-findings.md`'s batch 7 entry, which records
that honestly rather than claiming verification that didn't happen.

Two deviations from the letter of these tasks, made during implementation
and worth recording:

- **T018's routing was too narrow, and was corrected.** As written, T018
  said to route only `RtcpItem::ReceiverReport` blocks into `FarEndQuality`
  and ignore `SenderReport` items. That would have silently missed the
  report block on any real two-way call, where the carrier is also
  transmitting audio and therefore sends its own Sender Report — RFC 3550
  places receiver info in either packet type, not only in a bare RR.
  Implemented instead: `handle_inbound_item` extracts report blocks from
  *either* wrapper and routes any block matching this bridge's own SSRC.
  Pinned by `handle_inbound_item_routes_a_matching_block_from_either_sr_or_rr`.
  Recorded in `docs/plans/mt-conformance-findings.md`'s batch 7 entry too.
- **T021's `test_metric_renames.rs` addition was skipped, deliberately.**
  That file's `init_metrics()`/assertions are specifically a v4→v5 metric
  *rename* migration guard — every entry has an old name and a new name.
  The three new RTCP metrics have no old name to migrate from, so adding
  them there would exercise nothing the file's own test actually checks;
  their registration is already exercised by `make lint`/`cargo build`
  (a malformed `register_*!` call panics at startup) and by
  `ingest.rs`'s own arm being reachable from the `ObservedEvent` match.

---

## Phase 1: Setup

- [x] T001 Add `pub mod rtcp;` to `gsm-sip-bridge/src/ims/mod.rs` and
      create `gsm-sip-bridge/src/ims/rtcp.rs` with a module doc comment
      stating its scope (RFC 3550 SR/RR/SDES/BYE for the carrier leg only,
      two-party sessions, no XR/AVPF — mirroring `rtp.rs`'s and
      `sdp.rs`'s existing header-comment style). Empty otherwise; must
      compile.

**Checkpoint**: `cargo build` succeeds with the new empty module wired in.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: The wire format, the endpoint, and the accounting/scheduling
primitives every user story needs. No story-specific behavior lives here —
nothing in this phase sends a real report or reads one.

**⚠️ CRITICAL**: No user story phase can begin until this phase is
complete.

- [x] T002 [P] In `gsm-sip-bridge/src/ims/rtcp.rs`, define the RTCP packet
      type constants (SR=200, RR=201, SDES=202, BYE=203, version=2 per
      RFC 3550 §6.4-6.6) and a `ReportBlock` struct (ssrc, fraction_lost,
      cumulative_lost, highest_seq, jitter, lsr, dlsr) matching RFC 3550
      §6.4.1's receiver report block layout.
- [x] T003 [P] In `gsm-sip-bridge/src/ims/rtcp.rs`, implement
      `build_sender_report(ssrc, ntp_timestamp, rtp_timestamp,
      packet_count, octet_count, report_block: Option<&ReportBlock>) ->
      Vec<u8>` (RFC 3550 §6.4.1) and `build_receiver_report(ssrc,
      report_block: Option<&ReportBlock>) -> Vec<u8>` (§6.4.2). Unit tests:
      each builder produces the correct RFC 3550 header (V=2,P=0,type,
      length) and field byte order for a known input; an SR/RR with no
      report block states RC=0 and omits the block bytes (contract C-2.7's
      building half).
- [x] T004 [P] In `gsm-sip-bridge/src/ims/rtcp.rs`, implement
      `build_source_description(ssrc, cname: &str) -> Vec<u8>` (RFC 3550
      §6.5, one CNAME chunk) and `build_bye(ssrc) -> Vec<u8>` (§6.6, no
      reason string). Add `compound(parts: &[&[u8]]) -> Vec<u8>` that
      concatenates RTCP packets into one compound packet (§6.1: every
      compound packet begins with SR or RR). Unit test: a compound of
      SR+SDES parses back (via T005) into exactly those two members in
      order.
- [x] T005 In `gsm-sip-bridge/src/ims/rtcp.rs`, implement
      `parse_compound(data: &[u8]) -> Vec<RtcpItem>` where `RtcpItem` is a
      closed enum (`SenderReport{ssrc, ntp, rtp_ts, packet_count,
      octet_count, blocks: Vec<ReportBlock>}`, `ReceiverReport{ssrc,
      blocks}`, `SourceDescription{ssrc, cname: Option<String>}`,
      `Bye{ssrcs: Vec<u32>}`, `Unknown{pt: u8}`). Walk the compound packet
      by each member's own length field rather than assuming a fixed
      shape; a truncated or malformed member stops parsing at that point
      and returns what was already understood — never panics, never
      returns an error type the caller has to unwrap (contract C-4.3/
      C-4.4). Unit tests: an SR-only packet parses to one item; an
      SR+SDES compound parses to two, in order; a packet truncated mid-RR
      returns the items parsed before the truncation, not an error; an
      unrecognised payload type (e.g. APP=204, XR=207) parses to
      `Unknown` and does not abort parsing of subsequent members;
      zero-length input returns an empty vec, not a panic.
- [x] T006 [P] In `gsm-sip-bridge/src/ims/rtcp.rs`, implement
      `derive_round_trip(now_ntp_mid: u32, lsr: u32, dlsr: u32) ->
      Option<Duration>` per RFC 3550 §6.4.1 (`RTT = now - LSR - DLSR`, all
      in Q16.16 NTP middle-32 units). Return `None` when `lsr == 0` (no SR
      from us has been acknowledged yet — contract C-5.2, the case a
      naive implementation gets wrong by returning a zero RTT instead).
      Unit tests: a known LSR/DLSR/now triple yields the expected
      duration; `lsr == 0` yields `None`; a `now` that would make the
      subtraction go negative (clock skew / malformed DLSR) saturates to
      `None` rather than wrapping to a huge duration.
- [x] T007 [P] In `gsm-sip-bridge/src/ims/media_stats.rs`, add
      `SendAccounting`: `Arc`-cloneable, holding `AtomicU64` packets,
      `AtomicU64` octets, `AtomicU32` ssrc, `AtomicBool` ssrc_known, and
      `Mutex<Option<(u32, Instant)>>` for the last RTP timestamp and when
      it was observed — mirroring `MediaMeter`'s existing
      `Arc<AtomicU64>`-per-counter shape in the same file. Methods:
      `record_sent(&self, ssrc: u32, payload_octets: u64,
      rtp_timestamp: u32)` (increments packets by 1, octets by
      `payload_octets`, updates ssrc/ssrc_known/last-timestamp) and
      `snapshot(&self) -> Option<SendSnapshot>` (`None` while
      `ssrc_known` is false — contract C-2.7). Unit tests: two calls to
      `record_sent` accumulate both counters; `ssrc` updates to the
      latest value passed (supports the pass-through path's per-packet
      publish); `snapshot()` is `None` before the first `record_sent` and
      `Some` after.
- [x] T008 In `gsm-sip-bridge/src/ims/rtcp.rs`, implement
      `bind_rtp_and_rtcp(ip: IpAddr) -> BridgeResult<RtpRtcpBind>` per
      research.md Decision 1's three tiers: `RtpRtcpBind { rtp_socket:
      UdpSocket, rtp_port: u16, rtcp: Option<(UdpSocket, u16, bool)> }`
      where the `bool` is `declared` (true only for tier 2, meaning the
      answer must emit `a=rtcp:<port>`). Loop up to 10 attempts binding
      RTP ephemeral and, when its port is even, RTP+1; on success return
      tier 1 (`declared: false`). On exhausting attempts, keep the last
      RTP bind and try one more ephemeral bind for RTCP (tier 2,
      `declared: true`); if that also fails, return tier 3
      (`rtcp: None`) — the RTP socket is always returned, since media must
      proceed regardless (contract C-1.4/SC-006). Never fails the call:
      the `BridgeResult` only errors if even the final RTP bind fails
      (matching today's existing `UdpSocket::bind` failure mode at the
      call site, unchanged). Unit test: run it 20 times against
      `127.0.0.1` and assert every result is tier 1 or tier 2, never an
      error, and tier-1 results always have `rtp_port % 2 == 0` with
      `rtcp` port equal to `rtp_port + 1`.
- [x] T009 [P] In `gsm-sip-bridge/src/ims/rtcp.rs`, implement
      `ReportSchedule::new(bandwidth_bps: u32)`, `record_packet_size(&mut
      self, bytes: usize)` (running mean), and `is_due(&mut self, now:
      Instant) -> bool` which, when true, also rolls the next deadline
      forward using a freshly randomised ±50% interval around
      `mean_packet_size * 8 / bandwidth_bps` (RFC 3550 §6.3.1). No member
      count, no reconsideration (FR-004b). Unit tests: `is_due` is false
      immediately after construction (before the first interval
      elapses) then true once mocked/injected time passes the deadline;
      over many draws, the resulting intervals cluster within the ±50%
      band and their mean lands near the base value (statistical
      tolerance, not exact equality — use a fixed RNG seed or a wide
      tolerance to avoid flakiness).
- [x] T010 [P] In `gsm-sip-bridge/src/ims/rtcp.rs`, implement
      `SourceGuard::new(peer_ip: IpAddr)` and `SourceGuard::accept(&mut
      self, from: SocketAddr) -> bool`, checking only the IP (never the
      port — research Decision 7) and rate-limiting its own rejection
      log the same way `rtp::SsrcTracker` rate-limits its change log
      (reuse or mirror its 5-second-interval pattern). Unit tests: a
      datagram from the peer IP on a different port is accepted; one
      from a different IP is rejected; ten rapid rejections from the
      same wrong address produce at most the number of log lines the
      rate limit allows (assert via a counted tracing subscriber or by
      exposing a rejection counter for the test).
- [x] T011 In `gsm-sip-bridge/src/ims/agent/call.rs`, add
      `pub(super) struct RtcpHandle` (holds what teardown needs: the
      `Arc<AtomicBool>` stop flag it already gets from the shared `stop`,
      and nothing else owned by the caller — the thread owns its own
      socket) and give `ActiveCall` a `pub(super) rtcp: Option<RtcpHandle>`
      field alongside its existing `meter: MediaMeter` field. `None` is
      the tier-3 case (no RTCP on this call). No behavior yet — this is
      the plumbing site every later phase wires into.

**Checkpoint**: `cargo test -p gsm-sip-bridge ims::rtcp::` passes; the
whole workspace still builds; no relay or call-site behavior has changed
yet (nothing calls any of this code).

---

## Phase 3: User Story 1 - The bridge sends the reports its answer promised (Priority: P1) 🎯 MVP

**Goal**: For the duration of every answered call, the bridge sends
periodic sender reports on the carrier leg describing what it has
transmitted, at a rate consistent with the declared `b=RS:800`.

**Independent Test**: Answer a real or simulated call, capture the
carrier-facing RTCP port, and confirm SRs arrive periodically with a
consistent SSRC and non-decreasing counts — fully verifiable without the
far end ever sending anything back (spec's own Independent Test for US1).

### Implementation for User Story 1

- [x] T012 [P] [US1] In `gsm-sip-bridge/src/ims/transcode.rs`, give
      `RtpSender` an `Option<Arc<SendAccounting>>` (or a thin wrapper) and
      call `record_sent` with its own minted SSRC on every `send()` toward
      the carrier — i.e. only the `veth->carrier` direction's `RtpSender`,
      not the `carrier->veth` one (contract C-2.2, transcoding path).
      Thread it through `spawn_transcoding_relay`'s parameters using the
      parameter-bundle struct from research.md Decision 10 rather than
      adding another positional argument to the already-`#[allow(
      clippy::too_many_arguments)]` `relay_direction`. Unit tests: after N
      sends, `SendAccounting::snapshot()` reports N packets and the sum of
      payload octets sent; the SSRC in the snapshot matches
      `RtpSender`'s own (extend the existing `transcode.rs` test module
      rather than writing a parallel one).
- [x] T013 [P] [US1] In `gsm-sip-bridge/src/ims/agent/veth.rs`, in
      `forward`'s `carrier`-bound direction only, call `record_sent` on a
      `SendAccounting` using the **observed** SSRC of the packet being
      forwarded (via the existing `rtp::parse_packet`/`SsrcTracker` call
      already in that function) and the packet's own payload length —
      never a bridge-minted SSRC (contract C-2.2, pass-through path;
      FR-002b). Thread it through the same parameter bundle as T012 so
      `forward`'s signature gains one parameter, not several. Unit test:
      extend the existing `veth.rs` relay tests — after forwarding a
      sequence of packets including a mid-stream SSRC change, the
      snapshot's SSRC reflects the *latest* observed value and the packet/
      octet counts do not reset across the change (contract C-2.5).
- [x] T014 [US1] In `gsm-sip-bridge/src/ims/rtcp.rs`, implement
      `run_report_loop` (or similar): the per-call thread body that owns
      the RTCP socket from T008, polls it with the same
      `RELAY_POLL_INTERVAL` read timeout the relay threads use, and on
      each `ReportSchedule::is_due` tick builds and sends a compound
      packet — an SR when `SendAccounting::snapshot()` is `Some`,
      otherwise an RR with an empty report block (contract C-2.7) —
      followed by an SDES with a CNAME, to the endpoint's remote address.
      Depends on T003/T004/T007/T009. No far-end reading yet (US2) and no
      BYE yet (US5) — this task only sends. Unit test: with a fake/loopback
      socket pair, drive the loop for a few schedule ticks and assert SRs
      (or RRs, per C-2.7) arrive at roughly the scheduled cadence with
      growing counts.
- [x] T015 [US1] In `gsm-sip-bridge/src/ims/agent/inbound.rs`, at the
      point `ims_rtp_socket` is bound (around where `spawn_relay`/
      `spawn_transcoding_relay` are called today), call
      `rtcp::bind_rtp_and_rtcp` in place of the plain `UdpSocket::bind`
      for the carrier RTP socket, use its `rtp_port` for
      `sdp::build_answer`, and — when `rtcp` is `Some` — spawn the T014
      report loop, storing the resulting `RtcpHandle` on
      `ActiveCall.rtcp` (T011). When `rtcp` is `None` (tier 3), log a
      warning identifying the call and leave `ActiveCall.rtcp` as `None`
      (contract C-5.8's warning half; the metric half is T028 in Polish,
      since it needs the `ObservedEvent` machinery from US2's phase).
      This is the integration point where US1 becomes observable on a
      real call.
- [x] T016 [US1] In `gsm-sip-bridge/src/ims/sdp.rs`, in `build_answer_for`,
      emit `a=rtcp:<port>` immediately after the existing `m=audio` line
      only when the endpoint's `declared` flag (T008) is true; when false
      (tier 1 or tier 3), the answer is byte-for-byte identical to today's
      (contract C-1.1/C-1.2/C-1.3). Unit tests: extend the existing
      `sdp.rs` answer tests — tier-1/tier-3 style input produces an
      answer identical to the pre-feature fixture; tier-2 style input adds
      exactly one `a=rtcp:` line and nothing else changes; `b=AS:`/
      `b=RS:800`/`b=RR:2400` are present in all three cases (pins the
      FR-017 contradiction the clarification session caught).

**Checkpoint**: A real or loopback-simulated answered call now sends
periodic SRs on its carrier leg. `cargo test` green. This alone is
independently demonstrable per the spec's own Independent Test for US1.

---

## Phase 4: User Story 2 - The far end's view of the call is captured and reported (Priority: P1)

**Goal**: Reports the far end sends are read, validated, and their loss/
jitter/RTT reach both the existing end-of-call log line and new metrics.

**Independent Test**: With a call up, have the far end send an RR with
known figures and confirm those exact figures reach the call's media
reporting (spec's own Independent Test for US2).

### Implementation for User Story 2

- [x] T017 [US2] In `gsm-sip-bridge/src/ims/rtcp.rs`, add `FarEndQuality`
      (per data-model.md: `reports_received: u64`,
      `fraction_lost/cumulative_lost: Option<_>`, `jitter: Option<Duration>`,
      `round_trip: Option<Duration>`) behind an `Arc<Mutex<_>>`, and
      `record_receiver_report(&self, block: &ReportBlock, our_last_sr:
      Option<(u32, Instant)>)` that fills it in and calls
      `derive_round_trip` (T006) when possible. Unit tests: a first RR
      moves `reports_received` from 0 to 1 and fills every field it can;
      a report with no matching prior SR (no `our_last_sr`) leaves
      `round_trip` at `None` rather than deriving a bogus one (contract
      C-5.2).
- [x] T018 [US2] Extend T014's report loop (`gsm-sip-bridge/src/ims/rtcp.rs`)
      to also `recv_from` on the RTCP socket each poll: run every inbound
      datagram through `SourceGuard::accept` (T010) first; on acceptance,
      `parse_compound` (T005) it and route `ReceiverReport` items to
      `FarEndQuality::record_receiver_report` and `SenderReport` items
      (the far end reporting on *its own* sends, which this bridge
      doesn't otherwise need) to nothing — ignored, not an error.
      Anything rejected by `SourceGuard` or unparseable never reaches
      `FarEndQuality` (contract C-4.1/C-4.3). Unit test: feed the loop a
      well-formed RR from the right address and confirm `FarEndQuality`
      updates; feed one from the wrong address and confirm it does not;
      feed a truncated packet and confirm the loop keeps running
      afterward (doesn't panic or exit).
- [x] T019 [US2] In `gsm-sip-bridge/src/ims/agent/call.rs`, thread
      `FarEndQuality` and the T007 `SendAccounting`'s companion receive
      side (US3, T022 — read-only here) into
      `report_answered_call_ended`: add fields to the existing
      `tracing::info!("call media verdict", ...)` call — far-end loss/
      jitter/RTT when present, and an explicit marker (e.g.
      `far_end_reported = false`) when `reports_received == 0`, never a
      defaulted zero (contract C-5.1/C-5.3, FR-009). Unit test: with a
      `FarEndQuality` that has never received a report, assert the log
      call's fields (via a test tracing subscriber, matching however
      this file's existing tests capture log output — check `call.rs`'s
      current test module for the established pattern) show the
      never-reported marker, not zeros.
- [x] T020 [US2] In `gsm-sip-bridge/src/control/protocol.rs`, add
      `ObservedEvent::MediaQuality { source: QualitySource, loss_percent:
      f64, jitter_seconds: f64, round_trip_seconds: Option<f64> }` with
      `QualitySource` a new closed `enum { Local, Remote }` — both
      `#[derive(Serialize, Deserialize, ...)]` matching the file's
      existing variants. Unit test: round-trips through
      serde_json (or whatever this protocol already uses — check an
      existing variant's test) unchanged.
- [x] T021 [US2] In `gsm-sip-bridge/src/metrics/mod.rs`, add
      `RTP_LOSS_PERCENT` (`HistogramVec`, labels `["module", "source"]`),
      `RTP_JITTER_SECONDS` (`HistogramVec`, same labels), and
      `RTP_ROUND_TRIP_SECONDS` (`HistogramVec`, labels `["module"]` —
      RTT has no meaningful "source", it's inherently the bridge's own
      measurement of the round trip), following the existing
      `register_histogram_vec!` pattern next to `CALL_DURATION_SECONDS`.
      In `gsm-sip-bridge/src/metrics/ingest.rs`'s `apply_event`, add the
      `ObservedEvent::MediaQuality` arm observing the three histograms
      (skip `RTP_ROUND_TRIP_SECONDS` when `round_trip_seconds` is
      `None`). In `gsm-sip-bridge/tests/test_metric_renames.rs`, add the
      three new metrics to `init_metrics` per the file's existing
      one-line-per-metric pattern.
- [x] T022 [US2] Wire T017-T019 together at the call site: in
      `gsm-sip-bridge/src/ims/agent/inbound.rs`, pass the `FarEndQuality`
      handle created alongside the T015 report-loop spawn into
      `ActiveCall` (extend `RtcpHandle`, T011) so
      `report_answered_call_ended` (T019) can read it at teardown, and
      emit `ObservedEvent::MediaQuality { source: Remote, .. }` (T020)
      from the same call, once, using the final `FarEndQuality` snapshot
      when it is non-empty (skip entirely when `reports_received == 0` —
      there is nothing to trend).

**Checkpoint**: A far-end RR now reaches both the log line and Prometheus.
Independently verifiable per the spec's own test for US2, on top of US1's
already-working send side.

---

## Phase 5: User Story 3 - This side's own receive quality is measured on every call (Priority: P2)

**Goal**: Loss, reordering and jitter on the incoming carrier stream is
measured on both relay paths, using the existing `ReceiveTracker`, and
reaches the same reporting US2 established.

**Independent Test**: Relay a call with deliberately lossy/jittery input
on both relay paths and confirm matching figures in the call's media
reporting (spec's own Independent Test for US3).

### Implementation for User Story 3

- [x] T023 [P] [US3] In `gsm-sip-bridge/src/ims/transcode.rs`, feed the
      `carrier->veth` direction's incoming packets to a shared
      `Arc<Mutex<media_stats::ReceiveTracker>>` (`on_packet` per packet,
      exactly as `ims/call.rs:445` already does), threaded through the
      same parameter bundle as T012. Unit test: extend the existing
      `transcode.rs` tests — a sequence with a deliberate sequence gap
      and jitter yields a `ReceiveTracker::stats()` matching what
      `media_stats.rs`'s own tests already prove the tracker computes
      correctly for that input (reuse fixtures, don't re-derive the
      math).
- [x] T024 [P] [US3] In `gsm-sip-bridge/src/ims/agent/veth.rs`, do the
      same for `forward`'s carrier-bound receive direction. Unit test:
      same shape as T023, on the pass-through path.
- [x] T025 [US3] In `gsm-sip-bridge/src/ims/agent/call.rs`, extend
      `report_answered_call_ended` (already touched by T019) to also log
      this side's own `ReceiveStats` (received/lost/reordered/jitter) as
      fields distinct from the far-end ones (contract C-5.4/C-5.5), and
      emit a second `ObservedEvent::MediaQuality { source: Local, .. }`
      (reusing T020/T021's machinery — no new metric definitions here,
      just the second call site) whenever the call carried any media at
      all. Unit test: with a `ReceiveTracker` that recorded a gap and a
      reordering, confirm the log shows reordering as reordering, not as
      additional loss (contract C-5.5 — the case FR-013 exists
      specifically to prevent).
- [x] T026 [US3] Wire T023/T024's tracker into the same `ActiveCall.rtcp`
      plumbing at the `agent/inbound.rs` call site (T015/T022), passing
      it to whichever relay function is actually invoked
      (`spawn_transcoding_relay` or `spawn_relay`) so exactly one of
      T023/T024 is active per call, matching the existing `transcoding`
      boolean branch already at that call site.

**Checkpoint**: Receive quality is measured and reported identically
regardless of which relay implementation carried the call — the
uniformity contract C-5.4 requires, and the kind of divergence this
review already found once (RTP-03) and must not repeat.

---

## Phase 6: User Story 4 - The far end's stated RTCP port is honoured (Priority: P3)

**Goal**: An offer naming an explicit `a=rtcp` port has its reports sent
there instead of the default convention.

**Independent Test**: Answer an offer with an explicit non-default RTCP
port and confirm reports go there (spec's own Independent Test for US4).

### Implementation for User Story 4

- [x] T027 [US4] In `gsm-sip-bridge/src/ims/sdp.rs`, add
      `pub rtcp: Option<u16>` to `SdpOffer` (joining `maxptime`,
      `direction`, `proto`, `other_media` from batches 4-5) and have
      `parse_offer` read the audio section's `a=rtcp:<port>` line,
      yielding `None` on a missing, zero, or unparseable value (contract
      C-1.7/FR-016) rather than erroring the offer. Unit tests: an offer
      with `a=rtcp:30001` yields `Some(30001)`; one with `a=rtcp:0` or a
      non-numeric value yields `None`; one with no `a=rtcp` line is
      unaffected (extend `sdp.rs`'s existing offer-parsing test module).
- [x] T028 [US4] In `gsm-sip-bridge/src/ims/agent/inbound.rs`, use
      `offer.rtcp` (T027) when present as the RTCP remote port instead
      of `offer.remote_rtp.port() + 1`, still against the same peer IP
      the RTP media already trusts (never a different address from
      `a=rtcp` — contract C-1.8, this module intentionally reads only
      the port form of RFC 3605, per data-model.md). Unit test: with a
      fake report loop and both an offer carrying an explicit port and
      one without, confirm the destination address used for sending
      differs only in port, matching each case.

**Checkpoint**: FR-015/016 satisfied; the sibling deferred finding
(SDP-06's `a=rtcp` half) is now fully closed alongside RTP-01.

---

## Phase 7: User Story 5 - A call that ends says so on the media path too (Priority: P3)

**Goal**: A leaving-source BYE goes out on the RTCP path before the socket
closes, without making any teardown call site synchronous (research
Decision 2 — this turned out far cheaper than the spec anticipated).

**Independent Test**: End a call and confirm a BYE is sent on the RTCP
path before the socket closes (spec's own Independent Test for US5).

### Implementation for User Story 5

- [x] T029 [US5] Extend T014's report loop (`gsm-sip-bridge/src/ims/rtcp.rs`)
      so that when it observes the shared `stop` flag set, it sends
      `build_bye` (T004) using the current `SendAccounting` SSRC (falling
      back to silently skipping the BYE if no SSRC was ever known —
      nothing was sent, so there is no source to say goodbye from) and
      then exits, dropping its socket — satisfying FR-018 without any
      change to `call::handle_bye`/`hangup_carrier`/
      `end_call_attachment_lost` (contract C-6.7). Any send failure here
      is logged and swallowed (contract C-2.9), never propagated. Unit
      test: drive the loop with `stop` set from the start and confirm a
      BYE is sent and the loop returns; simulate a send failure (e.g. an
      already-closed socket) and confirm the loop still returns cleanly
      with only a log line, no panic and no error surfaced to a caller.

**Checkpoint**: All five user stories independently functional.
`hangup_carrier`/`handle_bye`/`end_call_attachment_lost` are
byte-for-byte unchanged from before this feature (verify with `git diff`
on those three functions specifically — contract C-6.7 is the one
easiest to violate by accident).

---

## Phase 8: Polish & Cross-Cutting Concerns

**Purpose**: The one deferred piece from US1's integration task, plus
whole-feature verification and the tracking-doc update the constitution's
commit discipline expects to land with the code, not after it.

- [x] T030 In `gsm-sip-bridge/src/ims/agent/inbound.rs`, complete T015's
      deferred half: when `bind_rtp_and_rtcp` returns tier 3, in addition
      to the warning already logged, emit a metric for "RTCP unavailable
      on this call" (either a dedicated `CounterVec` in
      `metrics/mod.rs`/`ingest.rs` following T021's pattern, or a reused
      `QualitySource`-style variant — pick whichever the T020/T021 shape
      accommodates more simply once both exist) — contract C-5.8/FR-017a
      completed. Unit test covering the metric increments on a simulated
      tier-3 result.
- [x] T031 [P] Run `make format && make lint && make test` across the
      whole workspace (including test targets — CLAUDE.md's mandatory
      pre-commit gate) and fix anything flagged. Pay particular attention
      to the `#[allow(clippy::too_many_arguments)]` sites this feature
      touches (`relay_direction`, `forward`) — confirm the parameter
      bundle from research Decision 10 kept them from growing further
      rather than adding raw parameters.
- [x] T032 [P] Byte-diff the pre-feature and post-feature SDP answer for
      the ordinary case (RTP+1 succeeds) against the existing
      `PJSIP_REAL_VETH_OFFER`-style fixtures in `sdp.rs` — confirm zero
      difference (contract C-1.1). This is the single highest-value
      regression check given `sdp.rs`'s own documented history of an SDP
      change silently breaking real calls.
- [ ] T033 Execute `specs/046-rtcp-reporting/quickstart.md`'s hardware
      round on the real EC20 line: verify no regression on an ordinary
      answered call, confirm which port tier was taken, capture and
      confirm SRs leaving on schedule, check whether the carrier sends
      RRs back (record the outcome either way — quickstart.md explicitly
      calls out "no RRs arrived" as itself a finding, not a silent gap),
      and confirm hangup is not perceptibly slower (SC-007).
- [x] T034 Update `docs/plans/mt-conformance-findings.md`: move RTP-01 and
      SDP-06's `a=rtcp` half from "deferred" to landed under a new batch
      entry, in the same style as batches 1-6, **explicitly recording
      FR-023a's residue** — the internal veth leg still declares RTCP
      bandwidth it does not back, and the originated-call path
      (`agent/origination.rs`) still has no RTCP at all. Neither is
      closed by this feature; the doc must not read as if RTP-01 were
      closed everywhere (this is the exact mistake the spec's FR-023a was
      written to prevent at the requirements stage — carry the same
      discipline into the tracking doc).

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies.
- **Foundational (Phase 2)**: Depends on Setup. **Blocks every user
  story** — T008's endpoint and T007's accounting type are load-bearing
  for US1, US2 and US5; T005's parser is load-bearing for US2; T009 for
  US1's cadence.
- **US1 (Phase 3)**: Depends on Foundational only. This is the MVP slice.
- **US2 (Phase 4)**: Depends on Foundational. **Also depends on T014/T015
  from US1** — the report loop and its call-site spawn are extended in
  place (T018) rather than duplicated; there is one loop per call, not
  one per story.
- **US3 (Phase 5)**: Depends on Foundational only (`ReceiveTracker` is
  pre-existing). Independent of US1/US2's report loop — it only touches
  the relay receive paths and the same end-of-call log line US2 extended,
  which pre-dates this feature. Can be implemented in parallel with
  US1/US2 by a different contributor.
- **US4 (Phase 6)**: Depends on Foundational (T008's endpoint needs
  somewhere to read a remote port override from) and on T015 (the call
  site it modifies). Independent of US2/US3.
- **US5 (Phase 7)**: Depends on T014 (extends the same loop) and T007
  (needs an SSRC to say goodbye from). Independent of US2/US3/US4.
- **Polish (Phase 8)**: T030 depends on US1+US2 (needs both the tier-3
  path and the metrics machinery). T031-T034 depend on everything above.

### Sequencing note (differs from a typical independent-stories layout)

Unlike a web app where each user story adds its own model/service/
endpoint, four of these five stories (US1, US2, US4, US5) extend **one
shared per-call thread and one shared endpoint** rather than adding
parallel infrastructure. So "independent" here means independently
*testable and valuable*, per each story's own Independent Test in
spec.md — not independently *implementable by parallel teams* for
US1/US2/US4/US5, which touch the same few files in sequence (T014→T018,
T015→T022/T028, etc.). US3 is the one story that is genuinely parallel:
different files (the relay receive paths), no shared loop.

### Parallel Opportunities

- Within Foundational: T002, T003, T004, T006, T007, T009, T010 touch
  either different functions in the same new file or a different file
  entirely (`media_stats.rs`) with no interdependency — safe to
  parallelize. T005 depends on T002's types; T008 depends on nothing in
  this phase but is easiest to write last, after the packet/accounting
  shapes it will eventually be used alongside are settled.
- T012 and T013 (the two relay paths' `SendAccounting` publishing) are
  different files, independent of each other.
- T023 and T024 (the two relay paths' `ReceiveTracker` wiring) are
  likewise different files, independent of each other.
- US3 (Phase 5) as a whole can proceed in parallel with US1+US2
  (Phases 3-4) once Foundational is done, since it shares no code path
  with the report loop.

---

## Implementation Strategy

### MVP First

The spec ties US1 and US2 at P1 — sending reports without ever reading
any leaves the conformance obligation half-met (a peer sees reports but
this bridge still can't state what the far end saw), so the practical MVP
is **Foundational + US1 + US2** together, not US1 alone. Stop and validate
there: real hardware round (T033) becomes meaningful once both directions
work, since it's the point where "does the carrier send RRs at all"
(quickstart.md's flagged open question) gets its first real answer.

### Incremental Delivery After MVP

1. Foundational + US1 + US2 → hardware round → this is RTP-01 substantively closed.
2. Add US3 → own receive quality alongside the far end's, on both relay
   paths.
3. Add US4 → the sibling SDP-06 finding closes too.
4. Add US5 → the RTCP BYE, cheap once the loop exists (research Decision 2).
5. Polish (T030-T034) → tracking-doc update, the item every batch of this
   review has ended with.

### Constitution compliance while executing

Per `make lint`'s whole-workspace `-D warnings` scope (CLAUDE.md), run
`make format && make lint && make test` at every phase checkpoint above,
not only at T031 — catching a violation one phase late costs less than
catching it after five phases of code have been layered on top of it.
