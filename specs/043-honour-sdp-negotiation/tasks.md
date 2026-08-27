---

description: "Task list for batch 4: honour SDP negotiation (SDP-01/02/03, MT-05)"
---

# Tasks: Honour what the far side actually offered in SDP

**Input**: Design documents from `/specs/043-honour-sdp-negotiation/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/

**Tests**: Included — every new function here is pure and colocated with
existing `#[cfg(test)]` coverage in the same files, matching this
codebase's established Integration-First Testing convention.

## Phase 1: Foundational (blocking prerequisite for all three findings)

All three findings (SDP-01/02/03) read from the same `parse_offer` walk
over an offer's `m=` sections, so the data-model extension is one cohesive
change, not three independent ones.

- [ ] T001 In `gsm-sip-bridge/src/ims/sdp.rs`: add `MediaDirection` enum
      (`SendRecv`/`SendOnly`/`RecvOnly`/`Inactive`) and `DeclinedMedia`
      struct (`kind`, `proto`, `fmts`, `before_audio`); extend `SdpOffer`
      with `direction: MediaDirection`, `proto: String`,
      `other_media: Vec<DeclinedMedia>`.
- [ ] T002 In `gsm-sip-bridge/src/ims/sdp.rs`'s `parse_offer`: restructure
      the `m=` section loop to (a) select the **first** `m=audio` section
      for real negotiation instead of letting a later one overwrite it,
      capturing its raw transport token into `proto`; (b) record every
      other `m=` section as a `DeclinedMedia` entry, in order, noting
      `before_audio`; (c) parse the selected audio section's own
      `a=sendonly`/`a=recvonly`/`a=inactive`/`a=sendrecv` line into
      `direction` (default `SendRecv`). `parse_offer` must not fail on any
      of these — only the existing structural failures (missing `c=`,
      missing `m=audio`, empty payload list) remain hard errors (research.md
      Decision 1).
- [ ] T003 Fix the `Ok(SdpOffer { ... })` construction in `parse_offer` and
      any other exhaustive `SdpOffer { .. }` literals in `sdp.rs`'s test
      module for the new fields.

**Checkpoint**: `SdpOffer` fully describes what the offer said; nothing yet
changes what the answer says.

---

## Phase 2: User Story 1 - Multi-section offers get a real answer to all of it (SDP-01, Priority: P1)

**Goal**: Every `m=` section in the offer gets a corresponding entry in the
answer — the negotiated audio section, plus an explicit decline for
everything else, in original order.

**Independent Test**: Offer with the supported audio section plus one
other section (any kind, including a second audio section) → answer
contains both the negotiated `m=audio` line and a declined `m=<kind> 0
...` line for the other, in the right relative order.

- [ ] T004 [US1] In `gsm-sip-bridge/src/ims/sdp.rs`: thread
      `other_media: &[DeclinedMedia]` through `build_answer_for` (and its
      callers `build_answer`/`build_veth_answer`, which already have
      `offer` in scope to read it from); emit one `m=<kind> 0 <proto>
      <fmts>\r\n` line per entry, placed before or after the negotiated
      `m=audio` block per `before_audio` — no `c=` line needed (the
      session-level one already covers it).
- [ ] T005 [P] [US1] Tests in `sdp.rs`'s existing `tests` module: extend
      the `PJSIP_REAL_VETH_OFFER`-based tests to assert the answer now
      contains `m=text 0 RTP/AVP 100 98\r\n`; add a new fixture with two
      `m=audio` sections proving the **first** is negotiated (port, codec)
      and the **second** appears as a declined `m=audio 0 ...` line, not
      silently overwriting the first.

**Checkpoint**: An offer with extra media sections is answered honestly;
an offer with only the one supported audio section is unaffected (existing
tests for that case must still pass unmodified).

---

## Phase 3: User Story 2 - The answer states the real direction (SDP-02, Priority: P2)

**Goal**: The answer's direction attribute mirrors what the offer's audio
section actually stated, per RFC 3264 §6.1, instead of always `sendrecv`.

**Independent Test**: Offer's audio section marked `sendonly`/`recvonly`/
`inactive` in turn → answer states the correct mirrored value in each
case; an offer stating `sendrecv` or nothing still gets `sendrecv`.

- [ ] T006 [US2] In `gsm-sip-bridge/src/ims/sdp.rs`'s `build_answer_for`:
      replace the hardcoded `a=sendrecv\r\n` with a mirror of
      `offer`'s (passed-through) `direction` —
      `SendOnly`→`a=recvonly`, `RecvOnly`→`a=sendonly`,
      `Inactive`→`a=inactive`, `SendRecv`→`a=sendrecv`.
- [ ] T007 [P] [US2] Tests in `sdp.rs`: one case per `MediaDirection`
      value proving the mirrored line appears in `build_answer`'s output;
      one case confirming an offer with no direction attribute (today's
      only real-world case) still answers `a=sendrecv`, unchanged.

**Checkpoint**: Direction is honestly reported; the RTP relay's actual
behavior is untouched (out of scope per spec Assumptions).

---

## Phase 4: User Story 3 - An unsupported transport is refused honestly (SDP-03, Priority: P3)

**Goal**: An offer naming a transport profile this bridge doesn't
implement is declined with a response distinct from both a successful
answer and the existing codec-mismatch decline.

**Independent Test**: Offer's audio section names `RTP/SAVP` (or garbage)
→ `488 Not Acceptable Here` with `Warning: 305 ... "incompatible network
protocol used"`, never an answer. An offer naming `RTP/AVP` is unaffected.

- [ ] T008 [US3] In `gsm-sip-bridge/src/ims/sip_client.rs`: add
      `build_488_incompatible_transport(request, to_tag, agent) -> String`,
      mirroring `build_488_not_acceptable`'s shape but with `Warning: 305
      {agent} "incompatible network protocol used"` (RFC 3261 §20.43).
- [ ] T009 [US3] In `gsm-sip-bridge/src/ims/agent/inbound.rs`'s
      `handle_invite`: immediately after `sdp::parse_offer` succeeds
      (before the existing codec-selection check), if `offer.proto !=
      "RTP/AVP"`, send `build_488_incompatible_transport` and return
      `Ok(None)` (mirroring the existing codec-mismatch decline's shape:
      log, report `CallStatus::Failed`/`BridgeFailureReason::BridgeSetupFailed`,
      return without reaching the codec check).
- [ ] T010 [P] [US3] Tests: `sip_client.rs` — the new builder states the
      305 warning correctly, distinct text from the existing 304 builder;
      `sdp.rs` — `parse_offer` captures a non-`RTP/AVP` token into
      `offer.proto` without failing (permissive per Decision 1);
      `inbound.rs` — (if testable without a live harness; otherwise note
      as hardware/manual-only, consistent with `handle_invite`'s existing
      untestable branches) the decline path is reached before the codec
      check for a mismatched-transport offer.

**Checkpoint**: All three SDP findings land; codec-mismatch (`488`/304)
and transport-mismatch (`488`/305) are both reachable and distinguishable.

---

## Phase 5: Polish & Cross-Cutting

- [ ] T011 [P] In `gsm-sip-bridge/src/ims/agent/inbound.rs`'s test module:
      add a confirming test that an inbound INVITE carrying
      `Session-Expires` still gets a `200 OK` with no `Session-Expires` and
      no `Supported: timer` header (MT-05 — pins already-correct,
      already-shipped behavior per research.md Decision 5; no production
      code change).
- [ ] T012 Update `docs/plans/mt-conformance-findings.md`: mark
      SDP-01/SDP-02/SDP-03/MT-05 `[x]` under batch 4, with a "Landed"
      writeup per finding matching the style of batches 1-3 (file/function
      names, RFC citations, explicitly-ruled-out items stated as such,
      MT-05's "confirmed resolved, no new code" note).
- [ ] T013 Add one entry to `RELEASE_NOTES.md` under `## Unreleased`
      describing the user-facing behavior change (an offer with extra
      media sections, a stated direction, or an unsupported transport now
      gets an honest answer instead of a silently wrong or incomplete
      one).
- [ ] T014 `make format && make lint && make test` (whole workspace,
      clippy `-D warnings`) — must be clean before any commit, per
      `CLAUDE.md`.
- [ ] T015 Hardware round on the `test/` docker rig per `quickstart.md`:
      rebuild/retag, redeploy, re-register the real line, drive a real
      inbound call, confirm no regression on the ordinary (one audio
      section, no direction, `RTP/AVP`) path; use `/discord-notify` to ask
      the user to place a call and wait for confirmation before treating
      this as done. Record in the tracking doc which of the three new
      decline paths (extra section, non-default direction, bad transport)
      could not be safely exercised live, matching batch 3's precedent for
      unit-test-only coverage of low-probability live scenarios.

---

## Dependencies & Execution Order

- Phase 1 (Foundational) blocks all of Phase 2-4 — every finding reads
  `SdpOffer`'s new fields.
- Phases 2, 3, and 4 all touch `build_answer_for` but on non-overlapping
  concerns (decline lines, direction line, and the transport check lives
  entirely in `inbound.rs`/`sip_client.rs` instead) — implement in
  priority order (SDP-01 → SDP-02 → SDP-03) to match the spec's user-story
  priorities, rather than in parallel, since they land as sequential
  commits per Constitution Principle III (Frequent Atomic Commits).
- Phase 5 depends on Phases 1-4 being complete.

## Implementation Strategy

Sequential, one finding per commit (matching batches 1-3's own commit
pattern): Foundational → SDP-01 → SDP-02 → SDP-03 → MT-05 test → docs →
release notes → gate → hardware round → PR.
