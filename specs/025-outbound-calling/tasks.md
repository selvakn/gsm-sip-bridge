---

description: "Task list for 025-outbound-calling"
---

# Tasks: Outbound Calling

**Input**: Design documents from `/specs/025-outbound-calling/`
**Prerequisites**: plan.md (revised 2026-08-03), spec.md, research.md
(revised — R-003/R-007/R-008), data-model.md, contracts/

**Revision note**: this list replaces the original tasks.md after three
`Explore` agents corrected two assumed blockers. Phases 1–2 and part of
Phase 4 were already implemented, tested, and committed in the first pass
and are carried forward as done. New tasks (T051+) cover the pjsua-safe UAS
addition, which nothing in US1/US3 can proceed without.

**Tests**: INCLUDED (constitution Principle I, NON-NEGOTIABLE). The pjsua
UAS additions are verified against the real, already-`pjsip-linked`-built
container and the physically attached EC200 modems
(`/dev/ttyUSB0`–`ttyUSB6`) — not mocked. Everything reachable from the stub
build (`make test`, no `pjsip-linked`) is still exercised over real
in-process channels, matching the rest of this codebase.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to (US1–US5)
- Exact file paths are given in every task

## Path Conventions

Rust workspace. Crate sources at `gsm-sip-bridge/src/` and `pjsua-safe/src/`;
integration tests at `gsm-sip-bridge/tests/`. Unit tests live inline.

## Pre-commit gate (applies to EVERY commit)

`make format && make lint && make test` — all three, no exceptions
(`CLAUDE.md`). `make lint` includes `tools/count-unsafe.sh`
(`gsm-sip-bridge/src` = 0 unsafe; `pjsua-safe/src` currently 1.68%/5%).

---

## Phase 1: Setup — DONE

- [X] T001 [P] Create `gsm-sip-bridge/src/sip/outbound.rs` module and register it in `gsm-sip-bridge/src/sip/mod.rs`
- [X] T002 [P] Create `gsm-sip-bridge/src/control/line_server.rs` and `gsm-sip-bridge/src/control/line_client.rs` module stubs, registered in `gsm-sip-bridge/src/control/mod.rs` — remain stubs; cross-process work is now Phase 8, not Phase 3

---

## Phase 2: Foundational — DONE

- [X] T003 Add `RawOutbound` in `gsm-sip-bridge/src/config/raw.rs`
- [X] T004 Add `OutboundConfig` runtime struct in `gsm-sip-bridge/src/config/mod.rs`
- [X] T005 Implement `build_outbound` in `gsm-sip-bridge/src/config/build.rs`
- [X] T006 [P] Inline tests in `gsm-sip-bridge/src/config/mod.rs` for `[outbound]` defaults
- [X] T007 [P] Document `[outbound]` in `docs/configuration.md`
- [X] T008 [P] Commented-out `[outbound]` block in `config.toml.example`
- [X] T009 Add `PlaceCall`/`PlaceCallOutcome` wire types in `gsm-sip-bridge/src/control/protocol.rs` (kept for the cross-process case, Phase 8)
- [X] T010 Add `OutboundCallRequest`/`Origin`/`CandidateLine`/`OutboundOutcome`/`CarrierPath`/`validate_destination`/`select_idle_line` in `gsm-sip-bridge/src/sip/outbound.rs`, unit-tested
- [X] T011 [P] Add `gsm_sip_bridge_outbound_attempts_total` metric in `gsm-sip-bridge/src/metrics/mod.rs`
- [X] T012 [P] Document the metric in `docs/observability.md`

**Checkpoint**: unchanged from the first pass — still the correct foundation.

---

## Phase 3: pjsua-safe UAS support (NEW — prerequisite for US1 and US3)

**Purpose**: BLOCKING for T018 and (later) T035. research.md R-007: small,
well-scoped addition — `Call::from_id` needs no FFI at all; `Call::answer`
and the `on_incoming_call` callback follow the exact
`#[cfg(feature = "pjsip-linked")]`/stub-split template `hangup`/
`on_call_state_cb` already use. No `pjsua-sys` regeneration needed
(`pjsua_call_answer`, `on_incoming_call`, `pjsua_call_get_info` are already
in the unfiltered bindgen output).

- [X] T051 [P] Add `Call::from_id(call_id: i32, state: CallState) -> Self` in `pjsua-safe/src/call.rs` — safe, no FFI (mirrors the struct literal already used internally by `Call::make`)
- [X] T052 [US1] Add `Call::answer(&mut self, code: u32) -> Result<(), PjsipError>` in `pjsua-safe/src/call.rs`, wrapping `pjsua_sys::pjsua_call_answer`, `#[cfg(feature = "pjsip-linked")]`/stub split identical to `hangup` (`call.rs:129-153`)
- [X] T053 [US1] Add the `on_incoming_call_cb` free `unsafe extern "C" fn(acc_id, call_id, rdata)` in `pjsua-safe/src/endpoint.rs` — DONE, adjusted from the original sketch: instead of `BRIDGE_PAIRS`, pushes `(acc_id, call_id)` onto a new `INCOMING_CALLS: Mutex<VecDeque<(i32,i32)>>` (that map is peer-pairing state for an already-answered call, not the right shape for "calls nobody has claimed yet"); exposed via `Endpoint::poll_incoming_call()`, following the same polling idiom `Call::poll_state` already uses rather than adding this crate's first callback trait for one event
- [X] T054 [US1] Register `cfg.cb.on_incoming_call = Some(on_incoming_call_cb);` in `pjsua-safe/src/endpoint.rs`'s `Endpoint::create` (`endpoint.rs:145-146` area)
- [X] T055 [P] Add inline tests to `pjsua-safe/src/call.rs` for `Call::from_id` and `Call::answer`'s stub-path state transitions (3 tests, all passing)
- [~] T056 [US1] **Hardware/container verification** — PARTIAL: the full daemon binary, including this task's code, builds and passes `cargo check`/`cargo test` against the **real** `pjsip-linked` feature (PJSIP 2.16 found via `pkg-config` directly on this host — confirms the FFI signatures/types are correct against the real headers, not just the stub). Did **not** run it against the attached EC200s: `docker-gsm-sip-bridge-1` is privileged with `/dev:/dev:rw` and is almost certainly the live production bridge already holding those serial ports — running a second instance risked disrupting real calls/registration. Confirming `on_incoming_call_cb` actually *fires* needs either a maintenance window on that container or hardware it isn't holding; deferred pending the user's go-ahead, and superseded anyway by T023's full end-to-end verification once T018 is wired
- [X] T057 Run `tools/count-unsafe.sh` (via `make lint`) and confirm `pjsua-safe/src` stays under the 5% ceiling — DONE: 31 blocks / 1.67% (was 29 / 1.68%)

**Checkpoint**: pjsua-safe can accept and answer an inbound call. Nothing
yet triggers `OutboundCallRequest` from it — that's T018.

---

## Phase 4: User Story 1 — PBX-originated CS outbound call (Priority: P1) 🎯 MVP

**Goal**: A PBX-sent INVITE dials a number out over an idle CS modem, in the
single main-daemon process — no cross-process IPC needed (research.md R-003,
revised).

**Independent Test**: Configure `[outbound].enabled = true` with a real EC20
attached, send an INVITE from a PBX (or a test UAC) naming a real number,
confirm `ATD` reaches the modem and two-way audio flows once answered.

- [X] T013 [P] [US1] Inline tests for `AtCommander::dial` in `gsm-sip-bridge/src/modules/at_commander.rs` — DONE
- [X] T014 [P] [US1] Destination validation tests in `gsm-sip-bridge/src/sip/outbound.rs` — DONE (part of T010)
- [X] T016 [US1] `AtCommander::dial` in `gsm-sip-bridge/src/modules/at_commander.rs` — DONE

### `ControlCmd::Dial` (same-process line command — contracts/control-cmd-dial.md)

- [X] T017a [US1] Add `ControlCmd::Dial { slot: u32, destination: String }` in `gsm-sip-bridge/src/control/protocol.rs`
- [X] T017b [US1] Add `ModuleCmd::Dial(String, oneshot::Sender<Result<(), String>>)` in `gsm-sip-bridge/src/modules/mod.rs` alongside `SetMode`/`Reboot`
- [X] T017c [US1] Handle `ModuleCmd::Dial` in `run_module_loop`'s `cmd_rx` match — DONE, refactored into a standalone `apply_dial_cmd(at, card, number)` free function (unit-testable against a mocked `AtCommander`, matching `at_commander.rs`'s own `MockStream` idiom) rather than inline in the match arm
- [X] T017d [US1] Add the `ControlCmd::Dial` arm in `CardPool::handle_control_cmd`, mirroring `SetMode`'s round-trip with a 5s timeout (shorter than `SetMode`'s 30s) and a `has_active_call` busy check
- [X] T017e [P] [US1] Unit tests for the `ControlCmd::Dial`/`ModuleCmd::Dial` logic — SCOPE ADJUSTED: rather than a full `CardPool` integration harness (which doesn't exist for `SetMode` either — `CardPool` needs heavy real-modem-shaped construction `handle_control_cmd` alone can't easily be given in a test), added `apply_dial_cmd` unit tests in `gsm-sip-bridge/src/modules/mod.rs` (idle→dials, busy→refused-and-state-untouched, AT failure→error) via a `MockAtStream` mirroring `at_commander.rs`'s own test mock — exercises the real `AtCommander::dial`/parsing path, not a reimplementation
- [~] T017f [US1] **Hardware verification** — PARTIAL: with the user's go-ahead, briefly stopped the live `docker-gsm-sip-bridge-1` container (the sole physical EC200 was otherwise held by it) and ran this build directly against the real modem. Discovery found and opened the real serial port (`/dev/ttyUSB0`) fine, but `CardPool` excluded it (`has_audio_capability=false`) before reaching a state where `ControlCmd::Dial` could be exercised — audio-device capability detection appears to depend on setup the privileged container provides (udev/ALSA) that a bare host process run outside it doesn't have. Restored the container immediately (~90s total disruption, confirmed healthy again). `ATD` reaching a real modem was NOT directly observed this way; the `apply_dial_cmd` unit tests (T017e) remain the verification for the dial logic itself. A true end-to-end run needs to happen *inside* the container image (`make docker-build` + run it there), not a bare host process — noted for T023's full-flow verification too

### PBX INVITE → outbound call (uses Phase 3's UAS support)

- [ ] T018 [US1] Implement the PBX-trunk UAS INVITE handler in `gsm-sip-bridge/src/sip/mod.rs`: on `on_incoming_call` (T053/T054) with `[outbound].enabled = true`, build `OutboundCallRequest { origin: Origin::Pbx, .. }` (`gsm-sip-bridge/src/sip/outbound.rs`), validate (`validate_destination`), select a line (`select_idle_line` over CS `SlotState`s), dispatch via `ControlCmd::Dial` (T017a–d) through the daemon's own `control_tx` — no socket, direct in-process send since `CardPool` always lives in the same process as the trunk `Account`
- [ ] T019 [US1] Implement call-progress relay in `gsm-sip-bridge/src/sip/mod.rs` per `contracts/sip-dialout.md`'s table (`180 Ringing`, `486 Busy Here`, `503 Service Unavailable`, `200 OK`), driven off the CS leg's AT-reported call state and `Call::answer` (T052)
- [ ] T020 [US1] Wire teardown in `gsm-sip-bridge/src/sip/mod.rs`: either leg hanging up ends the other, reusing the existing bridged-call teardown path
- [ ] T021 [US1] Increment `OUTBOUND_ATTEMPTS_TOTAL` (`gsm-sip-bridge/src/metrics/mod.rs`) with the right outcome label at every terminal point reached by T018–T020, called from `gsm-sip-bridge/src/sip/mod.rs`
- [ ] T022 [US1] Add an inline test in `gsm-sip-bridge/src/sip/mod.rs` (or extend T015-equivalent) confirming `[outbound].enabled = false` (default) leaves the INVITE handling byte-for-byte as before (FR-017); confirm `make test` stays green
- [ ] T023 [US1] **Hardware verification, end to end** (no source file): with `[outbound].enabled = true`, a real EC20 attached and registered, and a test SIP UAC standing in for the PBX, place a real outbound call and confirm: `180`/`200` progression, `ATD` reaches the modem, the call connects on the real mobile network, audio flows, and either side hanging up tears down the other

**Checkpoint**: PBX-originated outbound calling works end-to-end on real
circuit-switched hardware. This is the MVP.

---

## Phase 5: User Story 2 — Dial out on whichever SIM is free, same-process (Priority: P1)

**Goal**: unchanged in principle from the original tasks.md, but now split
by what actually needs the cross-process channel.

- [ ] T024 [US2] Extend `gsm-sip-bridge/src/sip/outbound.rs`'s line-selection call site in `gsm-sip-bridge/src/sip/mod.rs` (T018) to iterate **all** CS modems' `SlotState`s (not just the first), confirming `select_idle_line`'s no-path-preference behavior holds across multiple real EC20s if more than one is attached
- [ ] T025 [US2] Add a contention test in `gsm-sip-bridge/tests/test_outbound_control_cmd_dial.rs` (T017e): two near-simultaneous `ControlCmd::Dial` requests for the same last-idle CS slot — confirm exactly one succeeds via the existing single-threaded-per-modem serialization (research.md R-003 revised: no separate provisional-claim step needed for the same-process case, since the modem thread only processes one `ModuleCmd` at a time)
- [ ] T026 [US2] Document in `specs/025-outbound-calling/tasks.md` (this file, Phase 8) that multi-path selection (CS **and** a cross-process VoWiFi/VoLTE line) is deferred, since it depends on the cross-process channel — no code task here

**Checkpoint**: multi-SIM selection works for every CS modem attached to
this host. Cross-process multi-path selection is Phase 8.

---

## Phase 6: User Story 3 — Dial out from a phone in SIP server mode (Priority: P2)

**Goal**: unchanged from the original plan; now unblocked by Phase 3.

- [ ] T032 [P] [US3] Extend `gsm-sip-bridge/tests/test_sip_server_registrar.rs`: a registered phone's INVITE receives `302 Moved Temporarily` (per `contracts/sip-dialout.md`) when `[outbound].enabled = true`, still `403` otherwise
- [ ] T034 [US3] Change the `"INVITE"` branch in `gsm-sip-bridge/src/sip/server/mod.rs::handle_datagram` per T032
- [ ] T035 [US3] Implement the UAS INVITE handler on `Account::local` in `gsm-sip-bridge/src/sip/mod.rs` using Phase 3's `on_incoming_call`/`Call::answer`, constructing `OutboundCallRequest { origin: Origin::SipServerPhone { aor }, .. }`, reusing T018's validate → select → dial pipeline
- [ ] T036 [US3] Add an inline test in `gsm-sip-bridge/src/sip/mod.rs` confirming eligibility is any currently-registered account, not only `ring_aor` (FR-003)
- [ ] T033 [US3] **Hardware verification** (no source file): a real SIP phone (or softphone) registers to SIP server mode, dials a number, and the call is placed on a real EC20 exactly as T023 verified for the PBX path

---

## Phase 7: User Story 4 — VoWiFi/VoLTE outbound (Priority: P2) — REVISED, no pjsua dependency

**Goal**: reuse `ims::call`'s existing working UAC INVITE-origination code
(research.md R-008) from the live `ims::agent` loop, instead of writing new
SDP/RTP/signalling code. **This phase does not depend on Phase 3 at all** —
the carrier-facing leg never touches pjsua.

- [ ] T037 [P] [US4] Generalize/export `ims::call`'s UAC builders (`build_invite`/`build_ack`/`build_bye`/`InviteParts`, `gsm-sip-bridge/src/ims/call.rs:653-745`) for reuse outside the CLI diagnostic tool — e.g. lift beside `gsm-sip-bridge/src/ims/sip_client.rs`'s existing builders, or make the relevant `ims::call` items `pub(crate)`
- [ ] T038 [US4] Add an origination trigger to `gsm-sip-bridge/src/ims/agent.rs`'s session state (its `ActiveCall`, `agent.rs:610-628`, today only populated `from_invite`) so a `PlaceCall`-equivalent request can start a UAC dialog using T037's builders and the existing `sdp::build_offer`/`parse_answer` (`gsm-sip-bridge/src/ims/sdp.rs`) and RTP-relay code the inbound path already uses
- [ ] T039 [US4] Wire the dial-target dispatch in `gsm-sip-bridge/src/ims/agent.rs`: reached by whichever cross-process mechanism (Phase 8) or, if the agent process itself owns the SIP side, a direct in-process call
- [ ] T040 [P] [US4] Add a test in `gsm-sip-bridge/tests/` confirming PC/SC-sourced VoWiFi lines (`gsm-sip-bridge/src/modules/pcsc_card.rs`) are dialed identically to modem-sourced ones (no separate code path — per research.md, this needs no new code, just a test/assertion)
- [ ] T041 [US4] **Hardware verification, if a VoWiFi/VoLTE-capable SIM and P-CSCF reachability are available on this host** (no source file): place a real outbound call over VoWiFi or VoLTE and confirm the originated INVITE reaches the carrier and the call connects. If no such SIM is available in this environment, this task falls back to the container-based verification spec 015–017 already established for inbound IMS calls.

---

## Phase 8: Cross-process line-command channel (was Phase 2 of US2 in the original plan)

**Purpose**: needed only once a deployment mixes CS with VoWiFi/VoLTE, or the
SIP side is hosted by an agent process that needs to reach a *different*
agent's line. Deferred until Phases 4–7 are solid, per plan.md Step 4.

- [ ] T042 [US2] Implement `control::line_server` in `gsm-sip-bridge/src/control/line_server.rs` (per `contracts/line-command.md`, rescoped) started from the VoWiFi/VoLTE agent processes (`gsm-sip-bridge/src/ims/agent.rs`/`gsm-sip-bridge/src/vowifi/mod.rs` startup) only
- [ ] T043 [US2] Implement `control::line_client::place_call` in `gsm-sip-bridge/src/control/line_client.rs`
- [ ] T044 [US2] Extend `gsm-sip-bridge/src/sip/outbound.rs`'s line selection to include cross-process `CandidateLine`s, routing through T043 when the target line is remote
- [ ] T045 [US2] Implement the provisional-claim-then-command sequence in `gsm-sip-bridge/src/sip/outbound.rs` for the cross-process case (data-model.md's race handling — still needed here, unlike the same-process case)
- [ ] T046 [P] [US2] Create `gsm-sip-bridge/tests/test_outbound_line_command.rs`: real cross-process socket round-trip, no mocks

---

## Phase 9: User Story 5 — Diagnostics (Priority: P3) — unchanged

- [ ] T047 [P] [US5] Add log lines at every refusal/failure point in `gsm-sip-bridge/src/sip/mod.rs` and `gsm-sip-bridge/src/ims/agent.rs`
- [ ] T048 [P] [US5] Audit `gsm-sip-bridge/src/sip/mod.rs`, `gsm-sip-bridge/src/modules/mod.rs`, and `gsm-sip-bridge/src/ims/agent.rs` to confirm `OUTBOUND_ATTEMPTS_TOTAL` is incremented correctly at every terminal point across all phases
- [ ] T049 [US5] Create `gsm-sip-bridge/tests/test_outbound_diagnostics.rs` testing the three distinguishable outcomes (no idle line, network refused, unanswered) end to end

---

## Phase 10: Polish

- [ ] T050a [P] Add outbound-calling bullet to `README.md` Highlights
- [ ] T050b [P] Add "Outbound calling" section to `docs/architecture.md`
- [ ] T050c [P] Add an entry to `RELEASE_NOTES.md`
- [ ] T050d Run `specs/025-outbound-calling/quickstart.md` end to end against real hardware
- [ ] T050e Mark the outbound-calling item complete in `docs/todo.md`

---

## Dependencies & Execution Order

- **Phase 3 (pjsua UAS) blocks Phase 4 (T018+) and Phase 6 (T035)** — nothing
  else in the original plan blocks on it. Phase 7 (US4, VoWiFi/VoLTE)
  explicitly does **not** depend on Phase 3.
- **Phase 4 (US1)** is the MVP: Foundational (done) → Phase 3 → `ControlCmd::Dial`
  (T017a–f, independent of Phase 3) → T018–T023 (needs both).
- **Phase 5 (US2, same-process)** only needs Phase 4. **Phase 8 (cross-process)**
  is deferred and separate.
- **Phase 6 (US3)** needs Phase 3 and reuses Phase 4's pipeline.
- **Phase 7 (US4)** needs only Foundational — it can, in principle, be built
  in parallel with Phases 3/4/6, since it shares no code with the pjsua UAS
  work. Reaching it from a cross-process SIP-owning process still needs
  Phase 8.
- **Phase 9 (US5)** needs Phases 4/6/7 to have outcomes to diagnose.
- **Phase 10 (Polish)** last.

### Parallel opportunities

- T051/T055 (Phase 3) in parallel with T017a–e (Phase 4's `ControlCmd::Dial`,
  independent of pjsua)
- Phase 7 (US4) can run entirely in parallel with Phases 3/4/6, since it
  shares no files or dependencies with the pjsua UAS work
- T037/T040 (Phase 7 tests) in parallel

## Implementation Strategy

### MVP (unchanged goal, corrected path)

1. Foundational (done)
2. Phase 3 (pjsua UAS) — small, verify against real container + hardware early since everything else in US1/US3 depends on it
3. `ControlCmd::Dial` (T017a–f) — can start immediately, doesn't wait on Phase 3
4. T018–T023 — the actual MVP, verified against a real attached EC20
5. **STOP and VALIDATE** on real hardware before continuing
6. From there, US2 (same-process)/US3/US4/US5/Polish as before — US4 is now
   independently reachable at any point since it doesn't depend on Phase 3
