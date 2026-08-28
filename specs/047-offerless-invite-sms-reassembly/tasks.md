---

description: "Task list for offerless call answering (SDP-04) and multi-part SMS reassembly (SMS-05)"

---

# Tasks: Offerless Call Answering and Multi-Part SMS Reassembly

**Input**: Design documents from `/specs/047-offerless-invite-sms-reassembly/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, quickstart.md

**Tests**: Included — this project's constitution (Principle I,
Integration-First Testing, NON-NEGOTIABLE) requires them, and every unit
of new logic here is pure and directly testable. Tests live in-module
(`#[cfg(test)] mod tests`), this codebase's own convention — not a
separate `tests/contract|integration|unit/` tree.

**Organization**: US1 = SDP-04 (spec P1). US2 = SMS-05 (spec P2). The two
stories touch disjoint files (`ims/agent/inbound.rs` + `ims/sdp.rs` vs.
`volte/sms.rs` + `ims/sms_pdu.rs` + `ims/agent/mod.rs`) and share no
state, so either can be done first — see plan.md's Sequencing note. This
file lists US2 before US1 for that reason (its prerequisite fix, T004, is
the one most likely to need a second pass, so finding that out early is
cheaper — same rationale plan.md's Phase A–F ordering used), even though
US1 is spec priority P1.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: US1 (SDP-04) or US2 (SMS-05)

## Path Conventions

Single Rust workspace. All paths below are relative to `gsm-sip-bridge/`
(the crate root, itself inside the repo root).

---

## Phase 1: Setup

No setup tasks — no new dependencies, no new modules, no new tooling
(plan.md Technical Context). Both stories extend existing files.

---

## Phase 2: Foundational

No foundational phase — the two stories share no code and no new shared
infrastructure (plan.md's Sequencing note). Both can start immediately.

---

## Phase 3: User Story 2 - Deliver a multi-part text message as one complete message (Priority: P2)

**Goal**: A multi-part SMS is delivered once, joined, in order — not as
separate `[N/M]`-labelled fragments — while an ordinary single-part
message and the network-facing per-part acknowledgment stay exactly as
they are today.

**Independent Test**: Send a text long enough to be split into multiple
parts (quickstart.md). Confirm one joined message is delivered, not
several fragments; confirm an ordinary single-part message is unaffected.

### Prerequisite fix (research.md Decision 6)

- [X] T001 [US2] Change `parse_concatenation_udh` in `src/ims/sms_pdu.rs`
      to return a `ConcatPart { reference: u16, sequence: u8, total: u8 }`
      instead of the current `Option<(u8, u8)>`, keeping the reference
      byte(s) it already reads (8-bit IEI `0x00`: `ie[0]`; 16-bit IEI
      `0x08`: `ie[0..2]`) instead of discarding them. Update
      `decode_user_data` and `DecodedSms.part`'s type to
      `Option<ConcatPart>` accordingly.
- [X] T002 [US2] Update every existing call site and test in
      `src/ims/sms_pdu.rs` that matches the old `Some((seq, total))` tuple
      shape to the new `Some(ConcatPart { sequence, total, .. })` shape —
      including `decodes_a_concatenated_part_and_reports_its_sequence`
      and any other test asserting on `.part`.
- [X] T003 [US2] Update the one production call site outside
      `sms_pdu.rs` that reads `.part` — the `"[{seq}/{total}}] {}"` label
      construction in `src/ims/agent/mod.rs` (around the
      `decoded.part` match building `body`) — to read `.sequence`/`.total`
      off `ConcatPart` instead of a tuple.
- [X] T004 [US2] `make lint` (clippy `-D warnings`, whole workspace
      including test targets) to confirm no call site was missed — this
      is the check plan.md's Risks table relies on to catch a missed
      match arm as a build failure, not a silent behavior change.

### `Reassembly` (research.md Decision 7, data-model.md)

- [X] T005 [P] [US2] Add `Reassembly`, `PartialMessage`, and `PartOutcome`
      (`Complete(String)` / `Pending` / `Malformed`) to `src/volte/sms.rs`,
      alongside the existing `Dedupe` — same `Arc<Mutex<_>>`-shareable
      shape, same bounded-capacity/oldest-first-eviction posture as
      `Dedupe`'s own `VecDeque`. Core method: `admit_part(&mut self,
      sender: &str, part: &ConcatPart, text: &str) -> PartOutcome`, keyed
      on `(sender, reference, total)` (data-model.md). A `Complete`
      result does **not** remove its entry — add a companion
      `mark_delivered(&mut self, sender: &str, part: &ConcatPart)`
      (removes it, called on send success) and rely on
      re-admitting an identical already-held `sequence` being a safe
      no-op that still re-reports `Complete` (the retry-after-failed-send
      path plan.md's post-Phase-1 Constitution re-check records).
- [X] T006 [P] [US2] Add `take_expired(&mut self, now: Instant) ->
      Vec<(String, ConcatPart, String)>` to `Reassembly` — removes and
      returns every `(sender, part, text)` for a buffered entry whose
      `last_updated` is older than a new `REASSEMBLY_TIMEOUT: Duration =
      Duration::from_secs(180)` constant (3 minutes — spec.md
      Clarifications, 2026-08-28), one tuple per still-held part
      (data-model.md's expiry shape).
- [X] T007 [P] [US2] Unit tests for `Reassembly` in `src/volte/sms.rs`'s
      existing test module: completes when the last part arrives
      regardless of arrival order; two concurrent same-sender
      same-total messages (different reference) never cross-contaminate
      (FR-011); a `total == 0`, a `sequence == 0`, a `sequence > total`,
      and a part whose `total` disagrees with an already-buffered value
      for the same key all report `Malformed` (FR-016); re-admitting an
      already-held `sequence` with the same text is a no-op that still
      reports `Complete` when the set is otherwise full; `take_expired`
      returns nothing under the 3-minute bound and every held part at/over
      it; capacity eviction under a flood of distinct message identities.

### Wiring into both delivery routes (research.md Decisions 8, 9, 10)

- [X] T008 [US2] In `src/ims/agent/mod.rs`'s setup (near the existing
      `let dedupe = Arc::new(Mutex::new(...))`), construct
      `let reassembly = Arc::new(Mutex::new(crate::volte::sms::Reassembly::default()));`
      and thread it into `InboundParams` the same way `dedupe` is
      threaded in, and into whatever `run_modem_reader` is called with
      (mirroring `dedupe`'s existing parameter).
- [X] T009 [US2] In `handle_message` (`src/ims/agent/mod.rs`), insert the
      reassembly branch between the existing `Dedupe`/`decide` step and
      the `ControlMessage::SmsReceived` send: when `decoded.part` is
      `Some(part)`, call `reassembly.lock().admit_part(&sender, &part,
      &decoded.text)` —
      - `Complete(joined)`: send `SmsReceived` with `body = joined`
        (not the individual labelled fragment); on send success, call
        `mark_delivered` (mirroring the existing `Dedupe::confirm` call)
        and `Dedupe::confirm`; on failure, leave the `Reassembly` entry
        in place (do **not** call `mark_delivered`) and `Dedupe::forget`
        exactly as today's failure branch already does for the single-part
        case.
      - `Pending`: do not send `SmsReceived` yet; still `Dedupe::confirm`
        and `acknowledge(...)` this part now (FR-012 — a still-incomplete
        multi-part message's individual part is acknowledged to the
        network exactly as promptly as a single-part message is today).
      - `Malformed`: fall through to today's existing unconditional
        per-part send (the `[{seq}/{total}] {text}`-labelled body),
        unchanged (FR-016).
      A `decoded.part` of `None` (ordinary single-part message) takes the
      exact existing code path, untouched (SC-005/FR-015).
- [X] T010 [US2] In `run_modem_reader`'s sweep loop (`src/volte/sms.rs`),
      add one `reassembly.lock().take_expired(Instant::now())` call per
      pass, and for each returned `(sender, part, text)` send
      `ControlMessage::SmsReceived` with the individually-labelled body —
      the same shape/label the existing per-part send already uses —
      over the sweep thread's own `control` connection (FR-013/SC-004,
      research.md Decision 8).
- [X] T011 [P] [US2] Integration-level test: a `handle_message`-adjacent
      test (or a `volte::sms::decide`-adjacent one, matching however this
      module's existing tests exercise the modem-CS route without a real
      serial device) proving two parts submitted in sequence for the same
      `(sender, reference, total)` yield exactly one `SmsReceived`-shaped
      delivery with the joined text, not two; and that a duplicate
      retransmission of an already-admitted part (caught by `Dedupe`
      before ever reaching `Reassembly`, per data-model.md's explicit
      non-goal note) does not double-count.

**Checkpoint**: US2 (SMS-05) is fully functional and independently
testable — `make test` green, `quickstart.md`'s live-SMS step can be run.

---

## Phase 4: User Story 1 - Answer a call that arrives with no media description (Priority: P1) 🎯

**Goal**: An offerless inbound `INVITE` rings, is answered with this
bridge's own offer, and completes with working two-way audio once the
caller's device states its media in the `ACK` — instead of failing to
connect at all. An ordinary offer-carrying `INVITE` is unaffected.

**Independent Test**: Place a call using a caller known to omit its media
description on the initial request; confirm ringing and, once answered,
two-way audio (quickstart.md notes this is unit-test-only this round —
no such caller has been observed live yet).

### Shared helper promotion (research.md Decision 2)

- [X] T012 [P] [US1] Promote `offered_chosen_codec` from
      `src/ims/agent/origination.rs` (currently private) to a `pub(crate)`
      function in `src/ims/sdp.rs`, next to `build_offer`/`parse_answer`
      — unchanged behavior (maps `NegotiatedCodec::Pcmu`/`AmrWb` from a
      parsed answer to the fixed-PT `ChosenCodec` `build_offer`'s own
      offer promises; `None` for anything else). Update
      `origination.rs`'s call site to `sdp::offered_chosen_codec` and
      delete the local copy. `make test` must show zero behavior change
      on the origination path (existing tests there are the regression
      guard).

### The offerless branch (research.md Decisions 1, 5)

- [X] T013 [US1] In `handle_invite` (`src/ims/agent/inbound.rs`), after
      the existing `invite_content_type_supported` gate and before the
      `sdp::parse_offer` call, branch on `req.body.trim().is_empty()`.
      A non-empty body keeps today's exact existing path
      (`parse_offer` → codec precheck → ...) untouched. An empty body
      enters the new offerless path built by T014–T017.
- [X] T014 [US1] Offerless path, first half: spawn the veth UAS listener
      with `veth_wideband` sourced from `ctx.wideband` directly (not from
      a codec precheck, which doesn't exist yet for this path — research.md
      Decision 5), send `180 Ringing`, run the existing Agent B
      `IncomingCall`/`BridgeReady` control exchange and RTP/RTCP bind
      exactly as the existing path does (none of that depends on having
      an offer), then build our own offer with
      `sdp::build_offer(session.local_addr.ip(), ims_rtp_port, session_id,
      sdp::CodecOffer::preferring_wideband(ctx.wideband &&
      amr_safe::is_available()))` and run `await_pbx_answer` exactly as
      today, then send the `200 OK` carrying that offer (same headers as
      the existing path: `Allow`, `P-Access-Network-Info`) instead of an
      answer.
- [X] T015 [US1] Add a bounded drain-loop wait for the ACK
      (`OFFERLESS_ACK_TIMEOUT: Duration`, a new constant — a
      protocol-transaction-scale bound, distinct from `RING_TIMEOUT`,
      per research.md Decision 3/spec.md Assumptions) — same shape as
      `await_pbx_answer`'s existing drain loop over `inbound.rx`, matching
      an `ACK` whose `Call-ID` equals this call's, whose `CSeq` number
      equals the original `INVITE`'s, and whose `From` tag matches the
      caller's (mirroring `matches_caller_tag`/`names_active_dialog`'s
      existing tag-checking pattern from specs/042). Everything else on
      `inbound.rx` during this wait is ignored, same as
      `await_pbx_answer` already does for irrelevant methods.
- [X] T016 [US1] On a matching ACK: `sdp::parse_answer(&ack.body)`, then
      `sdp::offered_chosen_codec(answer.codec)` (T012). If the codec is
      unrecognized (`None`) or `parse_answer` fails, or the offer's
      `remote_rtp` connect fails, fall through to the teardown path
      (T017) with the appropriate reason — this is FR-005/SC-002's
      "explicit failure, never a silent connected-but-broken call."
      Otherwise: connect the RTP socket, spawn the RTCP report loop and
      the relay (transcoding or pass-through, exactly as the existing
      post-`200-OK` code already does), and construct the same
      `ActiveCall` the existing path returns — reusing that exact
      construction code (extract a shared tail helper if the two paths'
      remaining steps are identical enough that duplicating them would
      violate Principle V, otherwise two short cohesive tail blocks are
      fine per that same principle's "don't force a premature
      abstraction").
- [X] T017 [US1] On ACK timeout (T015's deadline) or an incompatible/
      unparseable answer (T016): build a `BYE` from
      `DialogInfo::from_invite(req, &to_tag, session).build_bye_for(&call_id)`
      (already available at this point in the function — no new dialog
      state), send it on the carrier transport, send Agent B
      `ControlMessage::CallEnded` (mirroring `RingOutcome::Abandoned`'s
      existing cleanup), report the call not-answered via the existing
      `obs.report_call_not_answered` path, and return `Ok(None)` —
      research.md Decision 4.

### Tests

- [X] T018 [P] [US1] Unit tests in `src/ims/agent/inbound.rs`'s test
      module (or the nearest existing test seam for `handle_invite`'s
      declines/branches, matching how SDP-03/SDP-05's own tests are
      structured): an empty body takes the offerless branch, not the
      existing `parse_offer`-error path; a non-empty, genuinely malformed
      body still gets today's existing error behavior unchanged
      (regression guard distinguishing Decision 1's two cases); a
      matching ACK with a compatible answer completes into an
      `ActiveCall`; an ACK naming an incompatible codec, and no ACK at
      all before the deadline, both reach the BYE/`CallEnded` teardown
      (T017) — never a silently-connected, never an indefinitely-ringing
      call (FR-005/FR-006/SC-002).
- [X] T019 [P] [US1] Regression test confirming an ordinary offer-carrying
      `INVITE` is handled by the exact pre-existing path (SC-005/FR-007)
      — extend or reuse whatever existing `handle_invite` test already
      pins the normal answer flow, asserting it is untouched by this
      feature's new branch.

**Checkpoint**: US1 (SDP-04) is fully functional per the unit-test
coverage above — `make test` green. Live verification stays unit-test-only
this round per quickstart.md (no offerless-sending peer observed by this
project to date).

---

## Phase 5: Polish & Cross-Cutting Concerns

- [X] T020 Update `docs/plans/mt-conformance-findings.md`: move SDP-04 and
      SMS-05 from the batch-6 "Deferred, not landed" list into a new
      "Batch 8" entry (matching the doc's existing per-batch structure),
      recording FR-002a's DTMF/RTCP residue (mirroring FR-023a's own
      entry) and this round's actual quickstart.md hardware-verification
      outcome (SMS-05 live-tested; SDP-04 unit-test-only, no
      offerless-sending peer observed).
- [X] T021 Full workspace gate: `make format && make lint && make test`
      (CLAUDE.md's mandatory pre-commit checklist) — whole workspace,
      including test targets, clippy `-D warnings`. Fix any fallout
      before considering either story done.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup / Foundational**: empty — nothing blocks either story.
- **US2 (Phase 3)** and **US1 (Phase 4)**: no dependency on each other;
  either can be done first or in parallel (plan.md's Sequencing note).
  This file orders US2 first only because its prerequisite fix (T001) is
  the one judged most likely to need a second pass.
- **Polish (Phase 5)**: depends on both stories being complete — T020
  documents both findings' outcomes together, T021 is the final gate over
  everything.

### Within US2 (Phase 3)

T001 → T002 → T003 → T004 (the UDH reference fix must land, lint-clean,
before anything reads the new shape). T005/T006/T007 (the `Reassembly`
type and its tests) are independent of T001–T004 and of each other's
file (all in `sms.rs`, but conceptually parallel — mark `[P]` since they
don't block each other, only need to land before T008). T008 → T009 →
T010 (the wiring, strictly ordered: construct the shared handle, then the
IMS-side hook, then the sweep-side flush). T011 last, once both routes are
wired.

### Within US1 (Phase 4)

T012 (the promotion) has no dependency on T013–T017 but must land before
T016 uses it — mark `[P]` since it's a different file
(`sdp.rs`/`origination.rs`) from T013–T017 (`inbound.rs`). T013 → T014 →
T015 → T016 → T017, strictly ordered (each extends the branch the last
one opened). T018/T019 after T017.

### Parallel Opportunities

- T005, T006, T007 (all `volte/sms.rs`, but additive/non-conflicting —
  treat as sequential in one sitting if working solo; genuinely
  parallelizable across two people).
- T012 alongside any of T013–T017 (different files).
- T018 alongside T019 (independent test cases).
- The whole of US2 (Phase 3) alongside the whole of US1 (Phase 4).

---

## Implementation Strategy

### MVP First

Given the two stories are independent and P1 (US1) is the more severe
finding (a call that cannot connect at all vs. a message that arrives
unjoined), **US1 is the MVP** per spec.md's own priority ordering —
despite this file sequencing US2's tasks first for the prerequisite-risk
reason stated above. "Sequenced first in this document" and "higher
priority to ship" are different questions; do not conflate them.

1. Phase 3 (US2) or Phase 4 (US1) — either first, per team capacity.
2. **STOP and VALIDATE** each story independently against its own
   Checkpoint before moving on.
3. Phase 5 (Polish) only once both are done — T020 documents them
   together, and splitting the findings-doc update in two would leave
   the doc momentarily implying one finding is closed when the file only
   reflects half a truth.

### Incremental Delivery

Both stories are independently deployable — either could ship alone
(spec.md's own Notes in `checklists/requirements.md` already record
this). Land whichever is ready first; the other's tasks are unaffected.

---

## Notes

- No `[Story]` label on Setup/Foundational/Polish tasks, per the
  checklist format — Phases 1, 2, and 5 have none here.
- Tests are written alongside their implementation tasks in this file
  rather than as a separate "write first, watch fail" phase — this
  project's convention (see e.g. specs/046's task history) is unit tests
  landing in the same commit as the logic they cover, not a strict
  red-green TDD split; T021's full-workspace gate is what actually
  enforces "tests pass" before any of this is considered done.
- Commit boundaries should follow plan.md's Phase A–F breakdown (which
  this file's T001–T004 / T005–T007 / T008–T011 / T012 / T013–T017 groups
  mirror one-to-one) — one compiling, `make test`-green commit per group,
  per the constitution's Frequent Atomic Commits principle.
