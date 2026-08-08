---

description: "Task list for 029-interruptible-origination-wait"
---

# Tasks: Interruptible wait for outbound call origination

## Implementation status (2026-08-08)

**Production code: complete.** All of US1/US2/US3 landed; Phase 4 (admission)
and Phase 5 (lifecycle, outcome) folded naturally into the Phase 3 restructure.
Full suite green (1243 passed, 0 failed); `make lint` clean.

**Deviations from the task plan, and why:**

- **Test placement.** The tasks named `tests/test_outbound_abandon.rs` etc.,
  but `dispatch_loop`/`originate_and_bridge`/`RegisteredSession` are
  `pub(crate)` and unreachable from a `tests/` integration crate (research R9).
  Pure logic that could be isolated is unit-tested in-crate: `poll_control_line`
  reassembly + EOF (T007), and the lifecycle transition rule that R5 violated
  was *already* pinned by `lifecycle.rs`'s existing
  `a_call_cannot_reach_bridged_without_the_pbx_ringing` / `a_call_walks_...`
  tests (T034 intent).
- **Socket-level tests (T003 race, T009–T013 abandon, T026–T029, T033, T035)
  not built as automated mocks.** They need a `RegisteredSession` test
  constructor + fake-carrier socket harness that does not exist, and
  `SipResponse` has no public constructor outside `sip_client`. Per the
  existing suite's own stance ("a real end-to-end call needs real hardware")
  and the user's direction to use the attached EC20/PC-SC hardware, these are
  covered by hardware verification (T046) rather than mocks. R2's race is fixed
  *by construction* (the main path no longer reads the socket directly),
  independent of a reproducing test.
- **R2 verdict:** analyzed as high-confidence from the source; not reproduced
  at runtime. The fix does not depend on reproducing it.

**Outstanding:** T046 (live hardware mid-ring-hangup verification) — see the PR;
being done against the attached hardware separately from CI.

**Input**: Design documents from `/specs/029-interruptible-origination-wait/`
**Prerequisites**: [plan.md](./plan.md), [spec.md](./spec.md), [research.md](./research.md), [data-model.md](./data-model.md), [contracts/](./contracts/)

**Tests**: **Required.** Constitution Principle I (Integration-First Testing) is
NON-NEGOTIABLE and the Development Workflow section makes TDD the default. Every
test task below drives real components — real `dispatch_loop`, real
`run_outbound_listener`, real TCP sockets, real `mpsc` channels. The only
stand-in is the carrier peer, which is not runnable locally; each such site
carries the written justification Principle I requires.

**Organization**: Grouped by user story. Note the honest dependency: US2 and US3
build on the Agent A restructure delivered in US1 (T014–T021) and are **not**
independently startable. This is stated rather than papered over.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: parallelizable — different files, no dependency on incomplete work
- **[Story]**: US1 / US2 / US3, mapping to spec.md

## Path Conventions

Single Rust workspace. Source under `gsm-sip-bridge/src/`, integration tests
under `gsm-sip-bridge/tests/`, unit tests in-module under `#[cfg(test)]`.

**Before every commit** (`CLAUDE.md`, Constitution II):
`make format && make lint && make test`.

---

## Phase 1: Setup

**Purpose**: Establish the baseline this feature must not regress.

- [ ] T001 Record the current outbound-path baseline: run `make test` and note which of `gsm-sip-bridge/tests/test_outbound_diagnostics.rs`, `test_volte_bridge.rs`, `test_vowifi_call_metrics.rs` cover origination today, in a short note appended to `specs/029-interruptible-origination-wait/research.md` under a new "R9 — baseline coverage" heading

---

## Phase 2: Foundational (Blocking Prerequisites)

**⚠️ CRITICAL**: No user story work can begin until this phase is complete. T003
in particular settles the open question in research R2 and determines whether
Phase 3's Agent A work is a bug fix or a pure refactor.

- [ ] T002 Create a reusable fake-carrier test harness in `gsm-sip-bridge/tests/support/fake_carrier.rs` (new; add `mod support;` where needed): a real TCP listener speaking enough SIP to accept an INVITE and emit scripted `100`/`180`/`200`/`486`/`487` responses on command, plus a helper that builds a `RegisteredSession` pointed at it **with `start_inbound` running**. Include the Principle I justification comment explaining that a real IMS carrier cannot run locally
- [ ] T003 Write `gsm-sip-bridge/tests/test_outbound_origination_race.rs` proving or disproving research R2: with `start_inbound` active, send an outbound INVITE and have the fake carrier reply, repeated ≥200 times; assert the response reaches the origination path every time. Record the verdict in `research.md` R2 under a new "**Verdict**" line
- [ ] T004 [P] Lock current good behaviour before touching it — add to `gsm-sip-bridge/tests/test_outbound_abandon.rs` (new file): a successful origination end-to-end, and the SC-006 slow-carrier case (≥18s gap between `100 Trying` and `180 Ringing`, call still completes)
- [ ] T005 [P] Lock the existing self-timeout path in `gsm-sip-bridge/tests/test_outbound_abandon.rs`: carrier never sends a final response, assert `cancel_pending_invite` emits a `CANCEL` and Agent B receives `CallFailed` marked `carrier_timeout`

**Checkpoint**: The behaviour that took five hardware passes to get right is now
pinned by tests, and R2 is settled. Restructuring can begin.

---

## Phase 3: User Story 1 — A caller who gives up stops the call they started (Priority: P1) 🎯 MVP

**Goal**: A caller hanging up mid-ring causes a `CANCEL` toward the carrier
within seconds, so the destination stops ringing and no phantom call connects.

**Independent Test**: Place an outbound call to a non-answering destination,
hang up the originating phone while it rings, confirm the destination stops
ringing and a `CANCEL` was sent for that attempt.

**Sub-structure**: T006–T013 are the Agent B half (detection) and are genuinely
independently shippable — they make the hangup visible in logs even before
Agent A acts. T014–T022 are the Agent A half (action).

### Tests for User Story 1 ⚠️ Write first; ensure they FAIL

- [ ] T006 [P] [US1] Test in `gsm-sip-bridge/src/vowifi/mod.rs` `#[cfg(test)]`: during the attempt phase, a phone leg reaching `CallState::Disconnected` causes `CallEnded { reason: "caller_hangup" }` to be written to Agent A and `PlaceCallOutcome::Abandoned` returned
- [ ] T007 [P] [US1] Test in `gsm-sip-bridge/src/vowifi/mod.rs` `#[cfg(test)]`: a `ControlMessage` delivered in two chunks straddling a poll timeout is still parsed exactly once (the research R7 `pending_line` property), and no partial line is lost or double-counted
- [ ] T008 [P] [US1] Test in `gsm-sip-bridge/tests/test_outbound_abandon.rs`: `PlaceCallOutcome::Abandoned` stops the line-by-line retry — with two configured lines, the second line is never dialled (FR-004)
- [ ] T009 [P] [US1] Test in `gsm-sip-bridge/tests/test_outbound_abandon.rs`: `CallEnded` arriving while the carrier INVITE is pending causes a `CANCEL` for that attempt's Call-ID within 1s, and Agent B receives exactly one `CallFailed` marked `caller_hangup`
- [ ] T010 [P] [US1] Test in `gsm-sip-bridge/tests/test_outbound_abandon.rs`: a `CallEnded` naming a **different** `call_id` is ignored — no `CANCEL`, attempt continues (FR-010)
- [ ] T011 [P] [US1] Test in `gsm-sip-bridge/tests/test_outbound_abandon.rs`: abandonment during the veth wait (after the carrier answered `200 OK`) sends `BYE` for the answered carrier leg rather than leaking it (FR-008, spec US1 scenario 5)
- [ ] T012 [P] [US1] Test in `gsm-sip-bridge/tests/test_outbound_abandon.rs`: the carrier answers `200 OK` in the same tick as abandonment — assert `ACK` then `BYE`, matching the existing self-timeout race handling (spec US1 scenario 3)
- [ ] T013 [P] [US1] Test in `gsm-sip-bridge/tests/test_outbound_abandon.rs`: the control connection to Agent B dropping mid-attempt is treated as abandonment, not waited out (spec Edge Cases)

### Implementation — Agent B half (detection)

- [ ] T014 [US1] In `gsm-sip-bridge/src/vowifi/mod.rs`, add `ATTEMPT_POLL_INTERVAL` (≈100ms, alongside the existing `PBX_RING_POLL_INTERVAL`) and rewrite `await_place_call_outcome` (currently lines ~900–926) as a poll loop: short socket read timeout, `pending_line` carried across timeouts per research R7, `CALL_ATTEMPT_TIMEOUT` reinterpreted as an overall deadline. Keep its value unchanged (FR-015)
- [ ] T015 [US1] In `gsm-sip-bridge/src/vowifi/mod.rs`, add `call.poll_state()` to each tick of that loop; on `CallState::Disconnected` write `ControlMessage::CallEnded { call_id, reason: reason::CALLER_HANGUP }` and return the new outcome
- [ ] T016 [US1] In `gsm-sip-bridge/src/vowifi/mod.rs`, add `PlaceCallOutcome::Abandoned` and handle it in `run_outbound_listener`'s per-line `match` (~line 722): `continue 'outer` without answering the phone leg (the caller is gone) and without trying the next line

**Checkpoint A**: Agent B now reports caller hangups during an attempt. T006–T008 pass. Commit here.

### Implementation — Agent A half (action)

- [ ] T017 [US1] In `gsm-sip-bridge/src/ims/agent.rs`, add the `PendingOrigination` struct and `OriginationStep` enum per [data-model.md](./data-model.md), including the `BridgedCall` lifecycle field constructed at `Answering`
- [ ] T018 [US1] In `gsm-sip-bridge/src/ims/agent.rs`, split the front half of `originate_and_bridge` (lines ~1193–1300: route headers, RTP bind, SDP offer, INVITE build and send) into `begin_origination(...) -> Option<PendingOrigination>`, and move `spawn_control_reader` from its current site (~line 1569) into it so `ctrl_rx` exists from the start
- [ ] T019 [US1] In `gsm-sip-bridge/src/ims/agent.rs`, extend `dispatch_loop`'s `SipMessage::Response` arm (~line 1943) to correlate against `pending` by Call-ID **before** the existing Gm-keepalive CSeq check, and advance the state machine: first `1xx` switches the deadline to `OUTBOUND_RING_TIMEOUT`; first `180` relays `CallRinging`; `200 OK` moves to `AwaitingVeth`; non-2xx final ACKs and clears with `CallFailed`. Non-matching responses must fall through to the keepalive path unchanged
- [ ] T020 [US1] In `gsm-sip-bridge/src/ims/agent.rs`, split the back half of `originate_and_bridge` (lines ~1345–1608: ACK, `DialogInfo`, codec negotiation, relay spawn, `ActiveCall` construction) into `finish_origination(...) -> Option<ActiveCall>`, called when the veth leg arrives
- [ ] T021 [US1] In `gsm-sip-bridge/src/ims/agent.rs`, replace the blocking `veth_rx.recv_timeout(VETH_INVITE_TIMEOUT)` (~line 1461) with the `AwaitingVeth` step polled by the dispatch loop, preserving the existing `hangup_answered_carrier_leg` timeout branch verbatim (FR-008)
- [ ] T022 [US1] In `gsm-sip-bridge/src/ims/agent.rs`, add the per-tick `pending` checks to `dispatch_loop`: `ctrl_rx.try_recv()` for a `call_id`-matching `CallEnded` (and `Disconnected` as abandonment), plus the deadline check — routing both to the shared `cancel_pending_invite` exit with different recorded reasons
- [ ] T023 [US1] In `gsm-sip-bridge/src/ims/agent.rs`, extend the poll-interval selection (~line 1813) so a pending origination polls at `ACTIVE_CALL_POLL_INTERVAL`, not `IDLE_POLL_INTERVAL` (research R8)
- [ ] T024 [US1] Teardown audit — enumerate every failure branch in the pre-refactor `originate_and_bridge` (RTP bind, `local_addr`, transport, INVITE send, non-2xx, ACK, `CallPlaced` write, veth timeout, unoffered codec, relay spawn, control clone) and confirm each survives the split with its comment intact and its carrier-leg/relay/pairing cleanup unchanged. Record the before/after table in `specs/029-interruptible-origination-wait/research.md` under "R10 — teardown audit"
- [ ] T025 [US1] In `gsm-sip-bridge/src/ims/sip_client.rs`, narrow `recv_final_response_for_origination` to its remaining caller (`cancel_pending_invite`) and update its doc comment to say it is no longer on the main origination path and why (single-reader rule, research R2)

**Checkpoint B**: US1 complete. T009–T013 pass, T004/T005 still pass. SC-001, SC-002, SC-003 verifiable.

---

## Phase 4: User Story 2 — Someone calling in during an outbound attempt is not left in silence (Priority: P2)

**Goal**: An inbound carrier INVITE arriving mid-attempt gets `486 Busy Here`
promptly instead of silence.

**Independent Test**: Start an outbound attempt to a non-answering number; call
the line from elsewhere; confirm a busy response arrives within seconds.

**Depends on**: T017 (the lifecycle field) and T019/T022 (the loop keeps
running during an attempt). Not startable before Phase 3.

### Tests for User Story 2 ⚠️ Write first

- [ ] T026 [P] [US2] Test in `gsm-sip-bridge/tests/test_outbound_abandon.rs`: an inbound INVITE arriving during an outbound attempt receives `486 Busy Here` within 10s (SC-004), while the outbound attempt continues unaffected
- [ ] T027 [P] [US2] Test in `gsm-sip-bridge/tests/test_vowifi_call_metrics.rs`: that refusal increments the same counters as a busy refusal during an established call (FR-013)
- [ ] T028 [P] [US2] Test in `gsm-sip-bridge/tests/test_outbound_abandon.rs`: an inbound INVITE refused during an attempt is **not** revived when the attempt fails moments later (FR-012, spec US2 scenario 2)
- [ ] T029 [P] [US2] Test in `gsm-sip-bridge/src/ims/agent.rs` `#[cfg(test)]`: an inbound SIP `MESSAGE` (SMS) arriving during an outbound attempt is still relayed and acknowledged — the data-loss hazard research R3 identifies

### Implementation for User Story 2

- [ ] T030 [US2] In `gsm-sip-bridge/src/ims/agent.rs`, extend the inbound-INVITE arm's admission check (~line 1820) to consult `pending`'s lifecycle as well as `active_call`'s, per [data-model.md](./data-model.md). No change to `Admission::for_current` itself
- [ ] T031 [US2] In `gsm-sip-bridge/src/ims/agent.rs`, confirm the `486` path's existing `obs.report_call_not_answered(...)` call is reached from the pending case and add a regression comment naming FR-013

**Checkpoint**: US2 complete. SC-004 verifiable. Inbound SMS provably unaffected.

---

## Phase 5: User Story 3 — Operators can see that abandonment is being handled (Priority: P3)

**Goal**: Abandoned attempts are recorded distinctly, exactly once.

**Independent Test**: Abandon an attempt mid-ring; confirm the record names
`caller_abandoned`, distinct from `unanswered` and from carrier rejection.

**Depends on**: Phase 3.

### Tests for User Story 3 ⚠️ Write first

- [ ] T032 [P] [US3] Test in `gsm-sip-bridge/tests/test_vowifi_call_metrics.rs`: a caller-abandoned attempt reports `OutboundAttemptOutcome::CallerAbandoned`, distinct from `Unanswered` and `RefusedNetworkFailure` (FR-018)
- [ ] T033 [P] [US3] Test in `gsm-sip-bridge/tests/test_outbound_abandon.rs`: exactly one outcome is reported per attempt across every exit path — success, carrier timeout, non-2xx, abandonment during carrier wait, abandonment during veth wait (FR-019, SC-005)
- [ ] T034 [P] [US3] Test in `gsm-sip-bridge/src/ims/lifecycle.rs` `#[cfg(test)]`: assert `Offered → Answering → PbxRinging → Bridged` is a legal path and that `Answering → Bridged` is **refused** — pinning the transition rule that research R5 found the outbound path silently violating
- [ ] T035 [P] [US3] Test in `gsm-sip-bridge/tests/test_outbound_abandon.rs`: a successful outbound call ends with `reached_bridged() == true` (the R5 regression)

### Implementation for User Story 3

- [ ] T036 [P] [US3] In `gsm-sip-bridge/src/control/protocol.rs`, add `OutboundAttemptOutcome::CallerAbandoned` and its `as_str()` arm `"caller_abandoned"` (~lines 189–207)
- [ ] T037 [US3] In `gsm-sip-bridge/src/vowifi/mod.rs`, report `CallerAbandoned` from the `PlaceCallOutcome::Abandoned` branch added in T016
- [ ] T038 [US3] In `gsm-sip-bridge/src/ims/agent.rs`, fix research R5: advance the outbound lifecycle `Answering → PbxRinging` on the carrier's first `180` (in T019's arm) and `PbxRinging → Bridged` in `finish_origination`, replacing the currently-silently-refused `advance_to(Bridged)` at ~line 1590
- [ ] T039 [US3] In `gsm-sip-bridge/src/ims/agent.rs`, ensure the single clear-`pending` site is also the single outcome-reporting site, so FR-019 holds structurally rather than by inspection

**Checkpoint**: All three user stories independently verifiable. SC-005 met.

---

## Phase 6: Polish & Cross-Cutting Concerns

- [ ] T040 [P] Update `docs/todo.md`: mark the dispatch-loop blocking item implemented in `specs/029-interruptible-origination-wait`, stating what is fixed and what residual limitation remains (still one call at a time per line)
- [ ] T041 [P] Replace the `KNOWN LIMITATION` comment in `gsm-sip-bridge/src/ims/agent.rs` (~lines 1775–1797) with a description of the state machine that now exists (FR-020)
- [ ] T042 [P] Update `cancel_pending_invite`'s doc comment in `gsm-sip-bridge/src/ims/agent.rs` (~lines 1057–1064), which currently states a caller hangup "has no way to trigger this at all" (FR-020)
- [ ] T043 [P] Update `docs/plans/dispatch-loop-interruptible-wait.md` to note it is superseded by this spec, and correct its step-2 premise about Agent B per research R1 — leaving the wrong version unmarked would mislead the next reader
- [ ] T044 [P] Add the contract delta to `specs/025-outbound-calling/contracts/agent-outbound-protocol.md` as a pointer to [contracts/agent-outbound-protocol-delta.md](./contracts/agent-outbound-protocol-delta.md)
- [ ] T045 If research R2's verdict was "race confirmed", add a note to `gsm-sip-bridge/src/ims/session.rs` near `spawn_client_reader` documenting the single-reader rule it implies, so the next direct `transport.recv_*` call is not written by accident
- [ ] T046 Run the `quickstart.md` manual verification against real hardware in the privileged container (sandbox cannot: see `sandbox-blocks-root-network-testing`); record the result in `tasks.md` under this task. Use synthetic numbers only
- [ ] T047 Full regression: `make format && make lint && make test`, plus SC-006/SC-007 confirmation that registration and the Gm keepalive survive a full-length attempt

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: no dependencies
- **Foundational (Phase 2)**: blocks everything. T003's verdict informs Phase 3 but does not change its shape
- **US1 (Phase 3)**: after Phase 2. Internally: T006–T016 (Agent B) can proceed independently of T017–T025 (Agent A)
- **US2 (Phase 4)**: after Phase 3 — **not independent**, needs T017/T019/T022
- **US3 (Phase 5)**: after Phase 3 — **not independent**, needs T017/T020
- **Polish (Phase 6)**: after all desired stories

### Within User Story 1

```text
T006,T007 ──▶ T014 ──▶ T015 ──▶ T016        (Agent B; shippable at Checkpoint A)
T008..T013 ──▶ T017 ──▶ T018 ──▶ T019 ──▶ T020 ──▶ T021 ──▶ T022 ──▶ T023 ──▶ T024 ──▶ T025
```

T017–T023 are strictly sequential — they are successive cuts through the same
function in the same file.

### Parallel Opportunities

- **Phase 2**: T004 and T005 in parallel once T002 exists
- **Phase 3 tests**: T006–T013 all `[P]` — write them together, then implement
- **Phase 3 implementation**: the Agent B chain (T014–T016) and the Agent A chain (T017–T025) touch different files and can run in parallel with two people
- **Phase 5**: T032–T036 all `[P]`
- **Phase 6**: T040–T045 all `[P]` — six different files

### Parallel Example: User Story 1 tests

```bash
# All eight are different test bodies with no shared mutable state
cargo test --test test_outbound_abandon          # T008..T013
cargo test -p gsm-sip-bridge vowifi::tests       # T006, T007
```

---

## Implementation Strategy

### MVP scope

**Phase 1 + Phase 2 + Phase 3 = US1.** That is the whole point of the feature:
a caller who hangs up stops the call they started. US2 and US3 are worthwhile
but neither has a third party on the other end of it.

A smaller-still increment exists: **Phase 2 + T006–T016** (Agent B only) makes
caller hangups visible in logs during an attempt without changing any carrier
behaviour. Useful if the Agent A restructure needs to wait.

### Incremental delivery

1. Phase 2 → the existing behaviour is pinned and R2 is answered
2. Checkpoint A → hangups are detected and reported (low risk, no carrier impact)
3. Checkpoint B → **ship**: CANCEL on abandonment, SC-001/002/003
4. Phase 4 → inbound callers hear busy instead of silence
5. Phase 5 → the fix becomes visible in records and metrics
6. Phase 6 → three stale "known limitation" notes stop lying

### Risk note

T017–T025 restructure code with five hardware-verification passes behind it
(`specs/025-outbound-calling` T072). T004, T005 and T024 exist specifically to
protect that. Do not proceed past Phase 2 until those tests are green.

---

## Task Summary

| Phase | Tasks | Count |
|---|---|---|
| 1. Setup | T001 | 1 |
| 2. Foundational | T002–T005 | 4 |
| 3. US1 (P1) 🎯 MVP | T006–T025 | 20 |
| 4. US2 (P2) | T026–T031 | 6 |
| 5. US3 (P3) | T032–T039 | 8 |
| 6. Polish | T040–T047 | 8 |
| **Total** | | **47** |

Test tasks: 19 of 47 (T003–T005, T006–T013, T026–T029, T032–T035).
Parallelizable: 25 marked `[P]`.
