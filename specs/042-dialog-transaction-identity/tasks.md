---

description: "Task list for feature 042: Match in-dialog SIP requests to the call they name"
---

# Tasks: Match in-dialog SIP requests to the call they name

**Input**: Design documents from `/specs/042-dialog-transaction-identity/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/sip-response-contract.md, quickstart.md

**Tests**: Included — colocated `#[cfg(test)] mod tests`, matching this codebase's existing convention in every file touched (verified against `src/ims/agent/mod.rs`, `call.rs` during planning).

**Organization**: Tasks are grouped by user story from `spec.md`: US1 = P1 (BYE mismatch, MT-08), US2 = P2 (retransmission/CANCEL/ACK identity, MT-01), US3 = P3 (re-INVITE honest decline, MT-02).

## Path Conventions

Single Rust crate at `gsm-sip-bridge/`. All paths below are relative to that crate root (i.e. `src/ims/agent/mod.rs` = `gsm-sip-bridge/src/ims/agent/mod.rs`).

---

## Phase 1: Setup

- [X] T001 Confirm a clean baseline before starting: `make format && make lint && make test` passes on `042-dialog-transaction-identity` at its current HEAD (no repo-root changes needed — this only verifies the branch starts green per the project's Green-on-Commit principle).

---

## Phase 2: Foundational (blocking prerequisites for all three user stories)

**⚠️ CRITICAL**: US1, US2, and US3 all call `names_active_call`; US2 and US3 both call `classify_in_dialog_invite`. Neither exists until this phase lands.

- [X] T002 [P] In `src/ims/agent/call.rs`: add `pub(super) struct CachedInviteAnswer { invite_cseq: String, contact: String, answer_sdp: String }`, `pub(super) enum InDialogInvite { RetransmittedOriginal, ReInvite }`, and `pub(super) fn classify_in_dialog_invite(req: &SipRequest, answered_invite: Option<&CachedInviteAnswer>) -> InDialogInvite` (matches on `req.header("CSeq") == Some(cached.invite_cseq.as_str())`, per `data-model.md`). Add `answered_invite: Option<CachedInviteAnswer>` to the `ActiveCall` struct. Add tests in `call.rs`'s existing `#[cfg(test)] mod tests`: `classify_in_dialog_invite_recognizes_an_identical_cseq_as_a_retransmission`, `classify_in_dialog_invite_treats_a_higher_cseq_as_a_re_invite`, `classify_in_dialog_invite_treats_no_cached_answer_as_a_re_invite`.
- [X] T003 [P] In `src/ims/agent/mod.rs`: add `fn names_active_call(req: &SipRequest, active_call_id: Option<&str>) -> bool` next to `unserved_method_response`. Add tests: `names_active_call_matches_the_same_call_id`, `names_active_call_rejects_a_different_call_id`, `names_active_call_is_false_with_no_active_call`.
- [X] T004 In `src/ims/agent/inbound.rs`: at the `ActiveCall { .. }` construction site (~line 387), add `answered_invite: Some(CachedInviteAnswer { invite_cseq: req.header("CSeq").unwrap_or_default().to_string(), contact, answer_sdp })`, moving the already-owned `contact`/`answer_sdp` locals. (Depends on T002.)
- [X] T005 [P] In `src/ims/agent/origination.rs`: at the second `ActiveCall { .. }` construction site (~line 1536), add `answered_invite: None` with a one-line comment pointing at `CachedInviteAnswer`'s doc comment (an outbound-placed call has no inbound INVITE of its own to have answered). (Depends on T002; parallel with T004 — different file.)

**Checkpoint**: `cargo test --lib ims::agent::call::tests ims::agent::mod::tests` passes; `ActiveCall` compiles with the new field wired at both construction sites. User story work can now begin.

---

## Phase 3: User Story 1 — A stray hangup can't end the wrong call (Priority: P1) 🎯 MVP

**Goal**: A `BYE` naming a call other than the active one (or arriving with no call active) is refused `481`, never tearing down the wrong call.

**Independent Test**: With a call in progress, send a `BYE` naming a different Call-ID and confirm the live call is untouched and the `BYE` is refused; confirm a `BYE` naming the live call still ends it normally.

- [X] T006 [US1] In `src/ims/agent/mod.rs`: add `fn bye_response_if_unmatched(req: &SipRequest, active_call_id: Option<&str>) -> Option<String>` (uses `names_active_call` from T003; `None` when matched, `Some(build_uas_response(481, "Call/Transaction Does Not Exist", req, Some(&random_hex(4)), None, None))` otherwise). Refactor `handle_carrier_bye` to check it first and only `self.active_call.take()` when `None` is returned. Add tests: `bye_response_if_unmatched_is_none_for_the_active_calls_own_call_id`, `bye_response_if_unmatched_refuses_481_for_a_different_call_id`, `bye_response_if_unmatched_refuses_481_with_no_active_call`.

**Checkpoint**: User Story 1 is fully functional and independently testable — `cargo test --lib ims::agent::mod::tests`, then a live/manual BYE-mismatch check per `quickstart.md` step 2.

---

## Phase 4: User Story 2 — Repeated or late signaling doesn't cause double effects (Priority: P2)

**Goal**: A retransmitted `INVITE` (while ringing or already answered) gets the same response already given; a `CANCEL` for an already-answered call gets an explicit `200 OK`; an `ACK` naming the wrong call is not treated as confirming the active one.

**Independent Test**: Resend the same offer while ringing and confirm the same ringing response, not a fresh ring; resend it after answer and confirm the same answer, not a new call attempt; send a `CANCEL` after answer and confirm an explicit reply distinct from the "no such transaction" refusal.

- [X] T007 [P] [US2] In `src/ims/agent/inbound.rs`: in `await_pbx_answer`'s existing drain loop, add an `INVITE` branch before the current fallthrough — when `req.method == "INVITE" && req.header("Call-ID") == Some(call_id)`, resend `build_180_ringing(&req, to_tag, contact)` instead of logging and dropping. Add a `contact: &str` parameter to `await_pbx_answer` and update its one call site to pass it. (No unit test — this branch needs a live socket/session harness; covered by `quickstart.md`'s hardware round.)
- [X] T008 [US2] In `src/ims/agent/mod.rs`, in `handle_inbound_invite`: before the existing `Admission::for_current` busy check, add — if `names_active_call(req, self.active_call.as_ref().map(|c| c.call_id.as_str()))` (T003) is true, call `classify_in_dialog_invite` (T002) on `self.active_call.as_ref().unwrap().answered_invite.as_ref()`; on `InDialogInvite::RetransmittedOriginal`, resend the cached `200 OK` via `build_uas_response_with_headers(200, "OK", req, Some(&call.to_tag), Some(&cached.contact), Some(&cached.answer_sdp), UAS_EXTRA_HEADERS)` and `return`. On `InDialogInvite::ReInvite`, do **not** handle it yet in this task — fall through unchanged to the existing busy-check logic below (US3 replaces this fallthrough in T011). (Depends on T002, T003, T004.)
- [X] T009 [US2] In `src/ims/agent/mod.rs`: add a `CANCEL` dispatch arm in `dispatch_loop` (before the generic catch-all) calling a new `fn handle_carrier_cancel(&self, req: &SipRequest, sink: &SipSink)`, which sends `build_uas_response(200, "OK", req, Some(&call.to_tag), None, None)` when `names_active_call` matches `self.active_call`, otherwise falls back to the existing `unserved_method_response` (still `481`). Add tests: `cancel_response_answers_200_ok_on_the_calls_own_to_tag_when_it_names_the_active_call`, `cancel_response_falls_back_to_481_for_an_unrelated_call_id`, `cancel_response_falls_back_to_481_with_no_active_call`. Verify the existing `a_stray_cancel_is_answered_481_not_405` test still passes unchanged (it exercises `unserved_method_response` directly).
- [X] T010 [US2] In `src/ims/agent/mod.rs`: replace the unconditional ACK debug log in `dispatch_loop` with `st.log_ack(&req)`, a new method that logs `debug!` when `names_active_call` matches and `warn!` (naming both the request's and the active call's Call-ID) otherwise. No SIP response either way. (Reuses T003's tested predicate — no new dedicated test required beyond a smoke check that both branches compile and log at the intended level.)

**Checkpoint**: User Stories 1 and 2 both work independently — `make test`, then the retransmission/CANCEL/ACK checks in `quickstart.md` steps 3–4 (best-effort on hardware; unit-test coverage stands in for anything not reproducible live).

---

## Phase 5: User Story 3 — A mid-call re-invitation is declined honestly, not refused as busy (Priority: P3)

**Goal**: A genuine re-INVITE (same Call-ID, new CSeq) on the active call gets `488 Not Acceptable Here`, never `486 Busy Here`.

**Independent Test**: With a call in progress, send a second offer using the same Call-ID but a new CSeq (not a retransmission) and confirm `488`, not `486`; confirm a genuinely separate call attempt is still refused `486` unchanged.

- [X] T011 [US3] In `src/ims/agent/mod.rs`, in the `handle_inbound_invite` pre-check added in T008: change the `InDialogInvite::ReInvite` arm from "fall through" to explicitly sending `build_488_not_acceptable(req, &call.to_tag, &format_sip_addr(session.contact_addr))` and returning, instead of continuing into the busy-check. (Depends on T008.)

**Checkpoint**: All three user stories are independently functional. `docs/plans/mt-conformance-findings.md`'s existing test `a_second_call_is_rejected_busy_and_the_first_is_undisturbed` (different Call-IDs) must still pass unchanged — confirms genuinely separate calls are still busied.

---

## Phase 6: Polish & Cross-Cutting Concerns

- [X] T012 [P] Update `docs/plans/mt-conformance-findings.md`: mark MT-01, MT-02, and MT-08 `[x]` with a "Landed" writeup in the same style as batches 1/2 (file/function names, RFC citations, ruled-out items from `research.md`'s Decisions).
- [X] T013 [P] Add one entry to `RELEASE_NOTES.md` under `## Unreleased`: a same-dialog re-INVITE is no longer refused as busy; a retransmitted request gets the answer already given instead of being reprocessed; a BYE naming a call the bridge doesn't have no longer ends whichever call happens to be active.
- [X] T014 Run `make format && make lint && make test` on the whole workspace (all test targets, `-D warnings`) — MANDATORY gate before any commit per `CLAUDE.md`.
- [X] T015 Hardware verification round per `quickstart.md`: rebuild/retag the `test/` docker image, redeploy, re-register the real line, and work through the BYE-mismatch, retransmission, CANCEL-after-answer, and regression checks — using `/discord-notify` to coordinate with the user for live call placement, matching the batch 1/2 pattern. Record results in `docs/plans/mt-conformance-findings.md`'s "Hardware test log" section.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies.
- **Foundational (Phase 2)**: Depends on Setup. BLOCKS all three user stories (T003's `names_active_call` is used by US1/US2/US3; T002's `classify_in_dialog_invite` is used by US2/US3).
- **User Story 1 (Phase 3)**: Depends on Foundational only (T003). Independent of US2/US3.
- **User Story 2 (Phase 4)**: Depends on Foundational (T002, T003, T004). T008 is written so US2 lands with the fallthrough for a genuine re-INVITE left as today's `486` (unchanged) — US2 is fully shippable on its own.
- **User Story 3 (Phase 5)**: Depends on T008 specifically (extends the same `match` arm) — this is the one place two stories touch the same code, and it's a widening edit (add a branch), not a rewrite, so US2 stays correct and tested before US3 extends it.
- **Polish (Phase 6)**: Depends on all three stories being complete.

### Parallel Opportunities

- T002 and T003 (Phase 2): different files (`call.rs`, `mod.rs`), no cross-dependency — run in parallel.
- T004 and T005 (Phase 2): different files (`inbound.rs`, `origination.rs`), both depend only on T002 — run in parallel with each other.
- T007 (Phase 4, `inbound.rs`) can run in parallel with T008/T009/T010 (all `mod.rs`) — different files.
- T008, T009, T010 all edit `mod.rs`: implement sequentially (any order among themselves), not concurrently, to avoid merge conflicts within one file.
- T012 and T013 (Phase 6): different files (`docs/plans/mt-conformance-findings.md`, `RELEASE_NOTES.md`) — run in parallel.

---

## Implementation Strategy

### MVP First (User Story 1 only)

1. Phase 1: Setup (T001).
2. Phase 2: Foundational (T002–T005) — required even for US1 alone, since it needs `names_active_call`.
3. Phase 3: User Story 1 (T006).
4. **STOP and VALIDATE**: `cargo test --lib ims::agent::mod::tests`, then the BYE-mismatch check in `quickstart.md`. This alone fixes MT-08, the highest-severity finding (a live call getting silently ended by unrelated signaling).

### Incremental Delivery

1. Setup + Foundational → foundation ready.
2. Add User Story 1 → validate → this is MT-08 fixed and shippable alone.
3. Add User Story 2 → validate → MT-01 fixed; a genuine re-INVITE still gets `486` (pre-existing MT-02 behavior, unchanged, not yet a regression).
4. Add User Story 3 → validate → MT-02 fixed; all three findings closed.
5. Polish (T012–T015) → tracking doc, release notes, full-workspace gate, hardware round.
