---

description: "Task list for Early Media Relay for Outbound Calls"
---

# Tasks: Early Media Relay for Outbound Calls

**Input**: Design documents from `/specs/037-p-early-media/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/agent-outbound-protocol-delta-early-media.md, quickstart.md

**Tests**: Included — this project's constitution (Principle I,
Integration-First Testing) requires real-component tests for new logic;
`plan.md`'s Constitution Check commits to unit tests for the state
machine and wire format, plus live verification for the PJSIP/RTP path
that can't run in CI.

**Organization**: Tasks are grouped by user story (spec.md priorities
P1/P2/P3) so each can be implemented and validated independently.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to (US1/US2/US3)
- File paths are exact and repo-relative to `gsm-sip-bridge/`

---

## Phase 1: Setup

- [X] T001 Confirm branch `037-p-early-media` builds clean (`make build`) and the full suite passes (`make test`) before starting, as a baseline to diff against.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: The shared control-protocol message and state hooks both
user stories build on. No user story work starts before this phase is
done.

- [X] T002 [P] Add `CallEarlyMedia { call_id: String }` to the `ControlMessage` enum in `gsm-sip-bridge/src/vowifi/control.rs`, including it in the existing `call_id()` match arm (see the `PlaceCall`/`CallAttempting` arms for the pattern).
- [X] T003 Add a wire round-trip test for `CallEarlyMedia` in `gsm-sip-bridge/src/vowifi/control.rs`, following the existing `CallRinging`/`CallAnswered` test pattern (control.rs:283-391) (depends on T002; same file, not parallel with it).
- [X] T004 [P] Extract `pair_veth_leg` (the `Call::make` + `endpoint.pair_calls(...)` steps only, no `answer()`) out of `bridge_outbound_leg` in `gsm-sip-bridge/src/vowifi/mod.rs`. Keep `bridge_outbound_leg` as a thin wrapper: call `pair_veth_leg`, then `call.answer(200)` — pure refactor, no behavior change yet. (Also wired `try_place_on_line`'s `CallEarlyMedia`/`CallPlaced` handling and a new `finalize_paired_outbound_leg` here since the two are inseparable in this codebase's structure — `PlaceCallOutcome::Placed` had to grow an `Option<Call>` regardless; see T009/T010 notes.)
- [ ] T005 [P] Add early-media bookkeeping fields to `PendingOrigination` in `gsm-sip-bridge/src/ims/agent/origination.rs`: `early_media_rtp_connected: bool`, `early_veth_rx: Option<mpsc::Receiver<BridgeResult<VethUasResult>>>`, `early_media_sent: bool` — all unset in the constructor, alongside the existing `provisional_answer`/`ringing_relayed` fields.

**Checkpoint**: Foundation ready — both user stories can now be implemented.

---

## Phase 3: User Story 1 - Caller hears the carrier's pre-answer announcement (Priority: P1) 🎯 MVP

**Goal**: Relay any pre-answer SDP audio the carrier sends to the caller,
starting the moment it arrives, with a zero-gap handoff into the answered
call. Carriers that send no pre-answer audio are unaffected.

**Independent Test**: Place an outbound call through a carrier known to
send a pre-answer announcement (e.g. Jio) and confirm the caller hears it,
timed against when the carrier sent it (`quickstart.md` steps 1-2).

### Tests for User Story 1 ⚠️

> Write these first; confirm they fail before implementing T007-T010.

- [ ] T006 [P] [US1] Unit test in `gsm-sip-bridge/src/ims/agent/origination.rs`: a synthetic response sequence — `183` with SDP → `183` again (retransmit/duplicate) → `200 OK` with an empty body — asserts the carrier RTP socket connects and the veth UAS listener spawns exactly once, at the *first* SDP-bearing provisional, not again at `200 OK`.

### Implementation for User Story 1

- [ ] T007 [US1] In `on_carrier_response`'s `resp.status < 200` branch (`origination.rs`), the moment a provisional's body parses via `sdp::parse_answer`: `self.rtp_socket.connect(answer.remote_rtp)`, call `spawn_veth_uas_listener`, store its receiver in `early_veth_rx`, set `early_media_rtp_connected`/`early_media_sent`, and send `CallEarlyMedia{call_id}` on the control connection — guarded so this only happens once per attempt (depends on T005, T002).
- [ ] T008 [US1] In `finish_origination`'s `200 OK` handling (`origination.rs`), branch on `early_media_rtp_connected`: when set, skip the RTP-connect and `spawn_veth_uas_listener` calls and consume `early_veth_rx` in their place; when unset, keep today's exact path unchanged (depends on T007).
- [X] T009 [US1] Add a `CallEarlyMedia` arm to `try_place_on_line`'s poll loop (`vowifi/mod.rs`): call `pair_veth_leg` (T004), then `call.answer(183)` instead of `180`, and retain the resulting veth `Call` in the loop's local state (depends on T004, T003). Done together with T004/T010 — also added `abandon_early_veth` cleanup on every non-`Placed` exit from the loop (Committed/Abandoned/timeout), and made `CallRinging` skip `answer(180)` once early media already answered `183`, per the contract.
- [X] T010 [US1] Update the `CallPlaced` arm in `try_place_on_line` (`vowifi/mod.rs`): if T009 already produced a paired veth `Call` for this attempt, do only `call.answer(200)`; otherwise run `bridge_outbound_leg` exactly as today (depends on T009). Implemented as `PlaceCallOutcome::Placed` growing an `Option<Call>`, dispatched in `run_outbound_listener` to `finalize_paired_outbound_leg` (new) or `bridge_outbound_leg` (unchanged).
- [ ] T011 [P] [US1] Run `specs/037-p-early-media/quickstart.md` steps 1-2 live against a carrier known to send pre-answer audio; confirm SC-001 (audible within 1s), SC-002 (no-early-media carriers unchanged), and SC-005 (zero-gap handoff at answer) (depends on T007-T010).

**Checkpoint**: US1 is fully functional and independently testable — pre-answer audio is now audible, with no regression for carriers that never send it.

---

## Phase 4: User Story 2 - Abandoning a call during the announcement leaves nothing stuck (Priority: P2)

**Goal**: Clean, symmetric teardown from the new early-media-paired state
— no leaked local leg or veth leg when the caller hangs up or the carrier
fails mid-announcement.

**Independent Test**: Place an outbound call to a carrier that sends a
pre-answer announcement, hang up partway through, confirm both legs end
and nothing is left active (`quickstart.md` step 3).

### Tests for User Story 2 ⚠️

> Write these first; confirm they fail before implementing T013-T014.

- [ ] T012 [P] [US2] Unit test(s) in `gsm-sip-bridge/src/ims/agent/origination.rs` covering (a) caller-abandon while early media is active and (b) carrier-fail (4xx/5xx or CANCEL-response) while early media is active — assert exactly one `CallFailed`/`CallEnded` is sent and no early-media state (`early_veth_rx`, connected socket) is left dangling.

### Implementation for User Story 2

- [ ] T013 [US2] Extend `fail()`'s `AwaitingCancel` path in `origination.rs` to send `CallFailed{call_id, reason}` to Agent B when this attempt already had early media active — today this path sends nothing to Agent B before `CallPlaced` because there was nothing to tear down; now there can be (depends on T007).
- [ ] T014 [US2] On Agent B, handle a `CallFailed` (or the caller's own hangup) arriving while a veth `Call` is already paired but `CallPlaced` never arrived (`vowifi/mod.rs`): hang up the paired veth `Call` via `Call::hangup()` in addition to today's local-leg handling (depends on T009).
- [ ] T015 [P] [US2] Run `specs/037-p-early-media/quickstart.md` step 3 (and step 4 if a carrier failure-after-early-media scenario is reproducible) live; confirm SC-003 and a clean `vowifi-status` call-history entry, not a stuck line (depends on T013, T014).

**Checkpoint**: US1 + US2 both work — pre-answer audio is audible and abandoning/failing mid-announcement leaves nothing stuck.

---

## Phase 5: User Story 3 - Carrier-side setup problems are audible on the first attempt (Priority: P3)

**Goal**: Confirm the diagnostic payoff — this story adds no new code, it
validates that US1's mechanism already delivers it.

**Independent Test**: Reproduce the originally diagnosed scenario (an
outbound attempt to a carrier that plays an announcement instead of
completing the call) and confirm the announcement is audible without
inspecting logs or captures.

- [ ] T016 [US3] Run `specs/037-p-early-media/quickstart.md`'s full walkthrough end-to-end as the original reproduction case; confirm SC-004 — the announcement is identifiable by ear alone, no log or packet-capture analysis needed (depends on T011).

**Checkpoint**: All three user stories independently validated.

---

## Phase 6: Polish & Cross-Cutting Concerns

- [ ] T017 [P] Run `make format && make lint && make test` clean, per this repo's mandatory pre-commit checklist (`CLAUDE.md`) — required before any commit, not optional.
- [ ] T018 [P] Re-read `contracts/agent-outbound-protocol-delta-early-media.md` and `data-model.md` against the final field/function names used in implementation (T002-T014); fix any drift between the docs and the code.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies.
- **Foundational (Phase 2)**: Depends on Setup. Blocks both user stories.
- **User Story 1 (Phase 3)**: Depends on Foundational only.
- **User Story 2 (Phase 4)**: Depends on Foundational **and** on US1's T007/T009 (the early-media trigger and the paired-veth-leg state it reads) — not independently implementable before US1, though its user value (clean teardown) is a separate story.
- **User Story 3 (Phase 5)**: Depends on US1's T011 (the live verification US3 reproduces). No new code.
- **Polish (Phase 6)**: Depends on all implemented stories.

### Within Each Story

- Tests (T006, T012) MUST be written and fail before their implementation tasks.
- Agent A changes (origination.rs) before Agent B changes that consume the new message (control.rs's `CallEarlyMedia` variant, T002, is a hard prerequisite for T007 and T009 alike).
- Live verification (T011, T015, T016) last, once the unit-tested logic is in place.

### Parallel Opportunities

- T002, T004, T005 (Foundational) touch three different files — run in parallel.
- T006 (US1 test) can be written in parallel with T004/T005 since it targets logic added in T007, not T004/T005 directly.
- T012 (US2 tests) can be drafted in parallel with US1 implementation once T007/T009 are far enough along to know the exact state shape, but should still fail until T013/T014 land.
- T011, T015 are both live/manual verification — can run back-to-back in one test session rather than truly parallel (single test line).

---

## Parallel Example: Foundational Phase

```bash
# Launch together — three different files:
Task: "Add CallEarlyMedia to ControlMessage in gsm-sip-bridge/src/vowifi/control.rs"
Task: "Extract pair_veth_leg from bridge_outbound_leg in gsm-sip-bridge/src/vowifi/mod.rs"
Task: "Add early-media fields to PendingOrigination in gsm-sip-bridge/src/ims/agent/origination.rs"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Complete Phase 1: Setup.
2. Complete Phase 2: Foundational.
3. Complete Phase 3: User Story 1 — this alone closes the diagnosability
   gap that motivated the feature (Story 1 is explicitly "without this
   story there is no feature").
4. **STOP and VALIDATE**: run `quickstart.md` steps 1-2 live.

### Incremental Delivery

1. Setup + Foundational → foundation ready.
2. US1 → validate live → this is the deployable MVP.
3. US2 → validate live → hardens the new early-media state against the
   abandon/fail edge cases US1 alone doesn't need to handle correctly to
   *demonstrate* value, but does need to handle correctly to *ship*.
4. US3 → no new code, confirms the diagnostic payoff end-to-end.
5. Polish → `make format`/`make lint`/`make test`, doc sync.

---

## Notes

- [P] tasks touch different files with no dependency between them.
- Commit after each task or logical group, per this repo's constitution
  (Principle III, Frequent Atomic Commits) — e.g. T002+T003 as one
  commit, T004 as one commit, T005 as one commit, T007+T008 together
  (they're two halves of one behavior change), T009+T010 together, etc.
- `make test` must stay green after every commit (Principle II) — do not
  land T007 without T006 passing against it, etc.
- US2 is not independently implementable before US1 (it extends state US1
  introduces) even though it is a separate, independently *testable* and
  *valuable* increment once US1 exists — noted as a deliberate exception
  to "most stories should be independent," not an oversight.
