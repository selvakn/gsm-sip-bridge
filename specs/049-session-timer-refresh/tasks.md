---
description: "Task list: RFC 4028 session-timer refresh (outbound/UAC leg)"
---

# Tasks: Honour RFC 4028 session-timer refresh on outbound calls

**Input**: Design documents from `/specs/049-session-timer-refresh/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, quickstart.md

**Tests**: Included — every new function is pure and colocated with
existing `#[cfg(test)]` coverage in the same files, matching this
codebase's established Integration-First Testing convention. The one
exception (documented, not skipped) is the trigger condition itself (a
`200 OK` carrying `Session-Expires`) — no carrier reachable here has ever
sent one on a connecting call, so end-to-end confirmation is fixture-driven
unit tests, same posture `specs/048` already established for this exact
feature area (quickstart.md).

## Phase 1: Foundational (blocking prerequisite for all three user stories)

- [x] T001 In `gsm-sip-bridge/src/ims/agent/session_refresh.rs` (new file,
      registered as `mod session_refresh;` in `agent/mod.rs`): add
      `Refresher` (`Uac`/`Uas`), `parse_session_expires(value: &str) ->
      Option<(u32, Option<Refresher>)>`, `SessionRefreshState`
      (`interval`, `refresher`, `phase`), `RefreshPhase`
      (`WaitingToSend`/`AwaitingResponse`/`Failed`/`WaitingForPeer`),
      `RefreshVerdict` (`Idle`/`SendNow`/`Overdue`), and
      `SESSION_REFRESH_RESPONSE_TIMEOUT` (10s, matching
      `ping.rs::PING_RESPONSE_TIMEOUT`'s precedent/rationale), per
      `data-model.md`. Include `SessionRefreshState::from_2xx(session_expires:
      Option<&str>, now: Instant) -> Option<Self>` (research.md Decision
      2's defensive default — no `refresher` param present defaults to
      `Uac`) and mutators `on_sent`/`on_response`/`on_send_failed`/
      `on_peer_refresh`/`refresher()` per `data-model.md`. Pure — no I/O,
      mirrors `agent/ping.rs`'s `PingState`/`PingVerdict` shape exactly.
- [x] T002 In `gsm-sip-bridge/src/ims/sip_client.rs`: add `UpdateRequest`
      and `build_update(req: &UpdateRequest) -> String`, mirroring
      `ByeRequest`/`build_bye` exactly (same fields) plus a
      `session_expires: &str` field, emitting `Supported: timer\r\n` and
      `Session-Expires: {session_expires}\r\n` headers and no body
      (`Content-Length: 0` — RFC 4028 §7.4: "RECOMMENDED that the UPDATE
      request not contain an offer").
- [x] T003 In `gsm-sip-bridge/src/ims/agent/call.rs`: add
      `DialogInfo::build_update_for(&mut self, call_id: &str,
      session_expires: &str) -> String` — increments `self.cseq`, calls
      `sip_client::build_update`. Add `ActiveCall.session_refresh:
      Option<session_refresh::SessionRefreshState>`; update
      `test_active_call` to set it `None`.
- [x] T004 In `gsm-sip-bridge/src/ims/agent/call.rs`: generalize
      `end_call_attachment_lost(session, call)` to
      `end_call_best_effort(session, call, reason: &str)` (same body,
      `reason::ATTACHMENT_LOST.to_string()` → the new `reason` parameter).
      Update its one call site in `agent/mod.rs`'s `handle_attachment_loss`
      to pass `reason::ATTACHMENT_LOST` explicitly.
- [x] T005 [P] In `gsm-sip-bridge/src/ims/lifecycle.rs`: add
      `EndedBy::SessionTimerExpired` (`as_str()` = `"session_timer_expired"`,
      `control_reason()` = `reason::SESSION_TIMER_EXPIRED`). In
      `gsm-sip-bridge/src/vowifi/control.rs`'s `reason` module: add
      `pub const SESSION_TIMER_EXPIRED: &str = "session_timer_expired";`.
- [x] T006 [P] Tests for T001-T005: `session_refresh.rs` —
      `parse_session_expires` (with/without `refresher=`, unknown extra
      params ignored, malformed value → `None`); `SessionRefreshState::
      verdict` for every phase, before/after its deadline, driven by an
      explicit `now` (never a real sleep, mirrors `PingState`'s existing
      tests); `on_response` ignores a mismatched `cseq` (mirrors
      `PingState::on_response`'s existing test). `sip_client.rs` —
      `build_update` produces `Content-Length: 0`, `Supported: timer`, the
      given `Session-Expires` value, and the given `CSeq`. `lifecycle.rs`
      — a confirming test that `EndedBy::SessionTimerExpired::as_str()`
      and `control_reason()` agree with `reason::SESSION_TIMER_EXPIRED`,
      matching the existing coincidence tests for the other variants.

**Checkpoint**: The refresh state machine, its wire request, and its
storage/observability slots all exist and are unit-tested; nothing yet
calls any of it from the dispatch loop.

---

## Phase 2: User Story 1 - This bridge is the session refresher (Priority: P1)

**Goal**: An outbound call whose `200 OK` assigns this bridge the
refresher role gets its own periodic `UPDATE` sent before the interval
elapses; a refresh that fails ends the call cleanly instead of leaving it
silently dead.

**Independent Test**: A `200 OK` carrying `Session-Expires:
300;refresher=uac` → at/before 150s, this bridge sends an `UPDATE`; a
`200 OK`/timeout answering it that isn't 2xx ends the call with
`EndedBy::SessionTimerExpired`.

- [x] T007 [US1] In `gsm-sip-bridge/src/ims/agent/origination.rs`'s
      `on_carrier_response`'s `resp.status == 200` arm: call
      `session_refresh::SessionRefreshState::from_2xx(resp.header("Session-Expires"),
      Instant::now())`. Add a `session_refresh:
      Option<session_refresh::SessionRefreshState>` field to
      `PendingOrigination` (initialized `None` in `begin_origination`, set
      here), destructured in `finish_origination` and passed into the
      `ActiveCall { ..., session_refresh, }` literal.
- [x] T008 [US1] In `gsm-sip-bridge/src/ims/agent/mod.rs`: add
      `LoopState::handle_session_refresh(&mut self, session, inbound, p)
      -> bool`, inserted into `dispatch_loop` right after
      `handle_attachment_loss` (before `advance_origination`), following
      that method's own shape. `call.session_refresh` is `None` → `false`
      (nothing to do). Otherwise, per `refresh.verdict(Instant::now())`:
      `Idle` → `false`; `SendNow` (refresher == `Uac`) → build the
      `Session-Expires` value (`"{interval};refresher=uac"`), send via
      `call.dialog.build_update_for(...)` over `session.transport_mut()`;
      `Ok` → `refresh.on_sent(call.dialog.cseq, now)`, `false`; `Err` →
      `refresh.on_send_failed()`, `false` (resolved `Overdue` the very
      next tick — no duplicated teardown logic here). `Overdue` → take
      `self.active_call`, `call.lifecycle.end(EndedBy::SessionTimerExpired)`,
      `call.stop.store(true, ..)`, `report_answered_call_ended`,
      `hangup_carrier(session, inbound, call, reason::SESSION_TIMER_EXPIRED)`,
      `self.maintenance.release()`, `true`.
- [x] T009 [US1] In `gsm-sip-bridge/src/ims/agent/mod.rs`'s
      `handle_carrier_response`: add a branch, checked before the existing
      Gm-keepalive-response match, for a response to this bridge's own
      sent refresh — `self.active_call`'s `session_refresh` is
      `AwaitingResponse { cseq, .. }`, `resp.header("Call-ID")` matches
      `call.call_id`, and `resp.header("CSeq")`'s number matches `cseq` →
      `refresh.on_response(cseq, resp.status, Instant::now())`, return
      (matched, regardless of 2xx/non-2xx — `on_response` itself decides
      success vs. `Failed`).
- [x] T010 [P] [US1] Tests: `origination.rs` — a `200 OK` fixture carrying
      `Session-Expires: 300;refresher=uac` leaves
      `ActiveCall.session_refresh` in `WaitingToSend`; one with no
      `refresher` param defaults to `Uac` too (research.md Decision 2); one
      with no `Session-Expires` header leaves it `None` (today's
      behavior, unchanged). `mod.rs` — `handle_session_refresh` sends on
      `SendNow` and transitions to `AwaitingResponse`; a `send()` failure
      resolves `Overdue` on the following tick; an `Overdue` verdict ends
      the call via `EndedBy::SessionTimerExpired` and releases
      maintenance (mirrors `handle_attachment_loss`'s existing test
      shape). `handle_carrier_response` — a matching `200 OK` moves
      `AwaitingResponse` to a fresh `WaitingToSend`; a matching non-2xx
      moves it to `Failed`; a response with a different `CSeq` is ignored
      (mirrors `PingState`'s own mismatched-response test).

**Checkpoint**: A call where this bridge holds the refresher role is fully
handled — sent, waited on, and torn down cleanly on failure. Nothing yet
handles the carrier holding the refresher role.

---

## Phase 3: User Story 2 - The carrier is the session refresher (Priority: P2)

**Goal**: An outbound call whose `200 OK` assigns the carrier the
refresher role has its own in-dialog `UPDATE` refresh accepted, instead of
rejected as it is today; a carrier that never refreshes still ends the
call (reusing Phase 2's generic tick handler — `RefreshVerdict::Overdue`
already covers `WaitingForPeer` past its deadline, no new teardown code
needed here).

**Independent Test**: A `200 OK` carrying `Session-Expires:
300;refresher=uas`, followed by the carrier's own body-less `UPDATE`
before the deadline → `200 OK` accepted, call continues. The same setup
with no `UPDATE` ever arriving → the call ends via Phase 2's
`handle_session_refresh`, unchanged code.

- [x] T011 [US2] In `gsm-sip-bridge/src/ims/agent/mod.rs`'s
      `dispatch_loop`: add a match arm for `req.method == "UPDATE"`
      (before the generic catch-all), calling a new
      `LoopState::handle_carrier_update(&mut self, session, req, sink)`.
      Accept (build a `200 OK` via `build_uas_response_with_headers` with
      `[("Supported", "timer")]`, call `refresh.on_peer_refresh(now)`)
      only when `self.active_call` is `Some`, `req.header("Call-ID")`
      matches `call.call_id`, `req.body` is empty, and
      `call.session_refresh`'s `refresher()` is `Uas`. Every other case
      (wrong dialog, carries a body, no call active, refresher is `Uac`,
      or no `session_refresh` at all) falls through to the exact existing
      `unserved_method_response(req, &random_hex(4))` — unchanged
      (FR-007: this must not touch or weaken the pre-existing re-INVITE
      decline in `handle_inbound_invite`, which this task does not
      modify).
- [x] T012 [P] [US2] Tests: `mod.rs` — `handle_carrier_update` accepts a
      matching body-less `UPDATE` and rejects (same response as today) one
      carrying a body, one naming a different Call-ID, one arriving with
      no active call, and one arriving while `refresher == Uac` (the
      carrier isn't on refresh duty in that case). A `WaitingForPeer`
      verdict past its deadline with no `UPDATE` ever received ends the
      call through `handle_session_refresh` (Phase 2's code, exercised
      here to prove FR-008 end to end for the carrier-refresher case).

**Checkpoint**: Both refresher roles are fully handled. Only the
minimal-advertisement regression pin (User Story 3) and polish remain.

---

## Phase 4: User Story 3 - The minimal-advertisement default keeps its promise (Priority: P3)

**Goal**: Confirm `[vowifi] originating_headers` at its default still
produces today's byte-identical outbound INVITE — this feature adds no
new code here by design (FR-009); it only needs a regression pin.

**Independent Test**: `originating_headers = []` (default) → the built
INVITE is unchanged from before this feature.

- [x] T013 [US3] Confirm the existing
      `the_originating_header_set_is_pinned` test (`ims/call.rs`) still
      passes unmodified with this feature's code compiled in — no
      `Session-Expires`/`Supported: timer` leaks into the outbound INVITE
      builder itself (only into the later, in-dialog `UPDATE` this
      feature's refresh logic sends, which that test does not and should
      not cover). No production code change; add nothing if the existing
      test already covers this — this task is the confirmation step.

**Checkpoint**: All three user stories land. FR-001 through FR-012 are
implementable end to end.

---

## Phase 5: Polish & Cross-Cutting

- [x] T014 Update `docs/todo.md`: mark the "RFC 4028 session refresh
      (`Supported: timer`) is not implemented" item `[x]`, with a "Landed"
      description citing the `UPDATE`-only transport decision
      (research.md Decision 1), the defensive refresher default (Decision
      2), and the regression-only hardware-verification note
      (quickstart.md).
- [x] T015 Add one entry to `RELEASE_NOTES.md` under `## Unreleased`
      describing the user-facing change: an outbound call whose carrier
      requires RFC 4028 session-timer refresh on a connecting call is now
      kept alive (or ended cleanly and diagnosably on failure) instead of
      silently dropping at the session interval.
- [x] T016 `make format && make lint && make test` (whole workspace,
      clippy `-D warnings`) — must be clean before any commit, per
      `CLAUDE.md`.
- [x] T017 Hardware round on the `test/` docker rig per `quickstart.md`
      (**regression-only**, per spec Clarifications and `docs/todo.md`'s
      own triage: no carrier reachable here has ever sent
      `Session-Expires` on a connecting call's `200 OK`): rebuild/redeploy,
      drive one ordinary outbound call, confirm no regression — INVITE
      unchanged, call connects, audio both ways, clean hangup. Record that
      the refresh logic itself remains unit-tested only, since no carrier
      reachable here can trigger it live.

---

## Dependencies & Execution Order

- Phase 1 (Foundational) blocks all of Phase 2-4 — every user story reads
  or writes `SessionRefreshState`/`ActiveCall.session_refresh`.
- Phase 2 (US1) must land before Phase 3 (US2): `handle_session_refresh`
  (T008) is where `RefreshVerdict::Overdue` is turned into a call-ending
  action, and Phase 3's own independent test relies on that existing,
  unmodified code path to prove FR-008 for the carrier-refresher case —
  Phase 3 adds only the accept-inbound-`UPDATE` path (T011) on top of it.
  Phase 4 (US3) depends only on Phase 1-3 being complete (it is a
  regression check, not new logic). Implement in priority order (US1 →
  US2 → US3) to match the spec's priorities and land as sequential commits
  per Constitution Principle III.
- Phase 5 depends on Phases 1-4 being complete.

## Implementation Strategy

Sequential, one user story per commit (matching prior batches' own commit
pattern): Foundational (state machine + wire request + storage +
observability) → US1 (this bridge as refresher: send, await, teardown on
failure) → US2 (carrier as refresher: accept its `UPDATE`) → US3
(regression pin) → docs → release notes → gate → regression-only hardware
round → PR.
