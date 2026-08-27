---

description: "Task list for batch 5: complete the media contract (RTP-03/04, SDP-06 ptime)"
---

# Tasks: Complete the media contract on the relay legs

**Input**: Design documents from `/specs/044-complete-media-contract/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/

## Phase 1: User Story 1 - DTMF payload type is relabeled on the pass-through relay (RTP-03, Priority: P1)

- [ ] T001 [US1] In `gsm-sip-bridge/src/ims/agent/veth.rs`: add
      `src_dtmf_pt: Option<u8>, dst_dtmf_pt: Option<u8>` parameters to
      `forward`; parse each received datagram with `rtp::parse_packet`
      (already handles header extension/CSRC correctly); when
      `payload_type == src_dtmf_pt` and `dst_dtmf_pt` is `Some` and differs
      from `src_dtmf_pt`, rewrite the payload-type byte (offset 1, low 7
      bits, preserving the marker bit) in place before forwarding; every
      other packet is forwarded unchanged.
- [ ] T002 [US1] Thread the same two parameters through `relay_rtp` (which
      calls `forward` twice, once per direction — each call gets the pair
      in the opposite order) and `spawn_relay` (`veth.rs`).
- [ ] T003 [US1] Update all four `spawn_relay` call sites to pass
      `chosen.dtmf_payload_type`/the veth-side codec's
      `.dtmf_payload_type` (both already in scope at every call site,
      confirmed in research.md): `gsm-sip-bridge/src/ims/agent/inbound.rs`
      (1 site), `gsm-sip-bridge/src/ims/agent/origination.rs` (3 sites).
- [ ] T004 [P] [US1] Tests in `veth.rs`'s existing test module: a keypress
      with differing negotiated DTMF PTs is relabeled to the destination's
      PT; a keypress with matching PTs passes through unchanged; an
      ordinary audio packet passes through unchanged regardless of the
      DTMF PT parameters.

**Checkpoint**: A DTMF keypress on a pass-through call always arrives on
the receiving leg's own negotiated payload type.

---

## Phase 2: User Story 2 - SSRC changes are logged, never enforced (RTP-04, Priority: P2)

- [ ] T005 [US2] In `gsm-sip-bridge/src/ims/agent/veth.rs`'s `forward`: add
      a local `last_ssrc: Option<u32>`; on each parsed packet, if
      `last_ssrc` is `Some` and differs from `pkt.ssrc`, log (identifying
      the direction and old/new SSRC); update `last_ssrc` either way;
      never drop the packet or stop forwarding because of this.
- [ ] T006 [US2] In `gsm-sip-bridge/src/ims/transcode.rs`'s
      `relay_direction`: the same `last_ssrc` local and log, added right
      after `rtp::parse_packet` succeeds (before the existing DTMF/audio
      branching), using the existing `direction: &'static str` parameter
      for the log's identification, consistent with the existing DTMF
      log's style.
- [ ] T007 [P] [US2] Tests: `veth.rs` and `transcode.rs` — a stream whose
      SSRC changes mid-relay logs the change and every packet still
      reaches the destination; a stream's first packet is never logged as
      a change; a stream with a constant SSRC logs nothing.

**Checkpoint**: An SSRC change is visible after the fact; no legitimate
call is ever disrupted by one.

---

## Phase 3: User Story 3 - The answer's ptime stays honest (SDP-06 ptime half, Priority: P3)

Reversed from the original plan on closer inspection (research.md
Decision 4): echoing the offer's `ptime` would have made the answer state
a packetization this bridge doesn't actually use. No `SdpOffer` field, no
`build_answer_for` change — this is a confirming test only, the same
resolution shape as batch 4's MT-05.

- [ ] T008 [US3] In `gsm-sip-bridge/src/ims/sdp.rs`'s test module: add a
      test proving the answer states its own true, fixed `a=ptime:20`
      regardless of what the offer's own `a=ptime` says (e.g. an offer
      stating `a=ptime:40` still gets `a=ptime:20` in the answer) — pins
      today's behavior as intentional, not an oversight.

**Checkpoint**: All three findings land (two implemented, one confirmed
as already correct); RTCP/`a=rtcp` remain explicitly deferred (no code
change, tracking-doc entry only).

---

## Phase 4: Polish & Cross-Cutting

- [ ] T011 Update `docs/plans/mt-conformance-findings.md`: mark
      RTP-03/RTP-04/SDP-06(ptime half) `[x]` with a "Landed" writeup
      matching batches 1-4's style; record RTP-01 and SDP-06's `a=rtcp`
      half as explicitly deferred (not `[x]`, not `[-]` — a distinct
      status noting the scope decision and pointing at this feature's
      research.md Decision 1 for the reasoning), so the doc doesn't imply
      either "done" or "won't ever do."
- [ ] T012 Add one entry to `RELEASE_NOTES.md` under `## Unreleased`.
- [ ] T013 `make format && make lint && make test` (whole workspace,
      clippy `-D warnings`).
- [ ] T014 Hardware round on the `test/` docker rig per `quickstart.md`:
      rebuild/retag, redeploy, re-register, drive a real call, confirm no
      regression on the ordinary path; use `/discord-notify`.

## Dependencies & Execution Order

Each user story is independent (different concern, though US1/US2 share
`forward`'s loop body in `veth.rs` — implement US1 first, since its
signature change is the more invasive edit, then add US2's logging into
the already-modified function). US3 is fully independent (different
file). Polish depends on all three.
