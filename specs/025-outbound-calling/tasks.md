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

**Session note (2026-08-03, implementation pass 3)**: found mid-implementation
that this specific host's actual deployment (`config.toml`) runs SIP server
mode hosted by `vowifi::mod` (Agent B), not the plain daemon — so T018 alone
(implemented against `SipBridge`/the daemon) could never receive a real
phone's INVITE without T034's registrar redirect *and* the equivalent
poll/dial/accept pipeline in `vowifi::mod` too. Both were added in the same
pass (see T018/T035's entries below) once this was discovered, rather than
declaring T018 "done" against a topology this host doesn't run. Also found
`ControlCmd`/`control::client::send_cmd` already work cross-process today
(proven by `SetMode`/`Reboot`/`CardRestart` from the CLI) — so `ControlCmd::Dial`
with `slot: None` reaches the daemon's CS lines from `vowifi::mod` with no
new socket, and Phase 8's originally-planned `line_server`/`line_client`
protocol is very likely unnecessary in its entirety, not just for the
same-process case as the previous revision concluded. Left as unimplemented
stubs; revisit only if a real deployment is found that `ControlCmd` truly
cannot reach.

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

- [X] T018 [US1] Implement the PBX-trunk (and, once discovered necessary, SIP-server-mode) UAS INVITE handler — DONE, different shape than planned: a 200ms poll of `SipBridge::poll_outbound_request` (new, wraps `Endpoint::poll_incoming_call` + `Call::request_destination`) added as its own `tokio::select!` arm in `CardPool::run` (`modules/mod.rs`), independent of the far-future retry/rescan wakeups already computed there — those are too infrequent for a caller waiting on a response. Dispatches via `ControlCmd::Dial` (not a direct in-process shortcut — see T024's note, this turned out to already need to work cross-process too)
- [X] T019 [US1] Call-progress relay — SCOPE ADJUSTED: full `180`/`486` mid-dial progress needs AT+CLCC-based call-state polling that does not exist in this codebase yet (`ATD`'s own response only confirms the attempt started — see `AtCommander::dial`'s doc comment). Implemented the two endpoints that don't need it: refusal (`400` no destination, `484` invalid, `503` no line/network failure) and acceptance (`200` once the dial is confirmed *accepted*, not once actually answered). True ringing-to-answered progress is a follow-up, tracked nowhere yet — flagging here rather than silently shipping it as if done
- [X] T020 [US1] Teardown — DONE, and turned out to need **no new code**: `ModuleCmd::Dial` already sets `card.state = Answering` on success, which is exactly the state the existing SIP-peer-disconnect check in `run_module_loop` and `BridgeEvent::Hangup`'s `self.sip_bridge.hangup_active_call()` (both written for the inbound direction) already watch — reused unmodified
- [X] T021 [US1] `OUTBOUND_ATTEMPTS_TOTAL` incremented at every terminal point in `CardPool::handle_outbound_request` (`modules/mod.rs`)
- [X] T022 [US1] Compatibility — DONE: the `outbound_poll.tick()` `select!` arm carries `if self.config.outbound.enabled`, so disabled means the poll (and everything downstream of it) never runs at all; `make test` green throughout including the full pre-existing suite
- [~] T023 [US1] **Hardware verification, end to end** — see the session-level note below (this deployment's actual topology needed T032–T036 too, not just T018–T022, before an end-to-end call was even reachable)

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

- [X] T032 [P] [US3] `gsm-sip-bridge/tests/test_sip_server_registrar.rs`: added `a_registered_phones_call_is_redirected_when_outbound_is_enabled` (302, Contact on the pjsua UAC port) and `an_unregistered_peers_call_is_still_refused_when_outbound_is_enabled` (still 403) — 24/24 tests pass including the unmodified original `a_call_from_a_phone_is_explicitly_refused`
- [X] T034 [US3] Changed the `"INVITE"` branch in `handle_datagram` (`sip/server/mod.rs`): `state.outbound_local_port: Option<u16>` (new field, threaded through `Registrar::start_observed`) gates a 302 redirect via `BindingStore::find_by_source` (new — matches the INVITE's peer address against a live binding's registered source, i.e. "already proved its password at REGISTER time," rather than a second digest exchange on the INVITE itself)
- [X] T035 [US3] `Account::local`'s incoming calls are covered by the same `SipBridge::poll_outbound_request`/`accept_outbound`/`refuse_outbound` T018 already added — `Endpoint::poll_incoming_call` is endpoint-wide, not account-specific, so no separate handler was needed once T034 made the registrar actually route a phone's INVITE to that account at all. **Also extended beyond the original plan**: this real deployment hosts SIP server mode in `vowifi::mod` (its own separate `Endpoint`/`Account`, not `SipBridge` — see that file's module doc), so the identical poll/validate/dial/accept pipeline was added there too (`run_outbound_listener`), dispatching to the daemon's CS lines over the existing control socket
- [X] T036 [US3] Eligibility is any registered account — `find_by_source` checks the binding table generically, with no `ring_aor` filter; covered by T032's redirect test using account `1001` (this deployment's only configured account, incidentally also `ring_aor` — a distinct-non-ring_aor-account test would strengthen this, not yet added)
- [ ] T033 [US3] **Hardware verification** (no source file): see the session-level note below

---

## Phase 7: User Story 4 — VoWiFi/VoLTE outbound (Priority: P2) — REVISED, no pjsua dependency

**Goal**: reuse `ims::call`'s existing working UAC INVITE-origination code
(research.md R-008) from the live `ims::agent` loop, instead of writing new
SDP/RTP/signalling code. **This phase does not depend on Phase 3 at all** —
the carrier-facing leg never touches pjsua.

**Revised 2026-08-03 (plan revision 4)**: the live test against this host's
real deployment showed T037–T041 as originally written undersold the work —
Agent A/B are separate processes, and nothing let Agent B tell Agent A to
originate a call at all. Replaced with the real breakdown below, per
`contracts/agent-outbound-protocol.md` and `research.md` R-009/R-010/R-011.

### Control protocol extension (Agent B → Agent A)

- [X] T060 [P] [US4] Add `ControlMessage::PlaceCall { call_id, destination }`, `CallPlaced { call_id }`, `CallFailed { call_id, reason }` in `gsm-sip-bridge/src/vowifi/control.rs` — DONE, `CallPlaced` corrected to carry no port (discovered mid-implementation: RTP addressing is negotiated through the real veth SIP/SDP exchange, not the control-channel JSON — see `contracts/agent-outbound-protocol.md`)
- [X] T061 [P] [US4] Real round-trip tests added inline in `gsm-sip-bridge/src/vowifi/control.rs`'s existing `mod tests` (not a separate integration test file — matches where the existing inbound triad's own tests already live), all passing (21/21 in that module)

### `ims::call` builder generalization (no behavior change)

- [X] T062 [US4] Widened `InviteParts`/`build_invite`/`AckParts`/`build_ack`/`build_bye` (`gsm-sip-bridge/src/ims/call.rs`) to `pub(crate)` — DONE. **Found mid-implementation**: `build_bye` (this one) turned out unneeded for the outbound BYE — `ims::agent` already has its own generic dialog-BYE mechanism (`sip_client::ByeRequest`/`build_bye`, used by the existing `hangup_carrier`) that works unmodified once `DialogInfo` is populated correctly (T064), so only `InviteParts`/`build_invite`/`AckParts`/`build_ack` are actually called from `agent.rs`. Widening `build_bye` too was harmless and left in place for consistency
- [X] T063 [US4] Confirmed — `ims::call`'s existing tests (`build_invite_includes_sdp_body_and_content_length`, `build_invite_advertises_the_protected_server_port_in_contact`, `build_invite_includes_route_headers_in_order`, `build_bye_reuses_to_header_verbatim`, plus 4 unrelated tone-pattern tests) all 8 pass unmodified

### Agent A: UAC origination over the live session

- [X] T064 [US4] Added `DialogInfo::from_uac_response` next to `from_invite` (`agent.rs`) — DONE, built from the live `RegisteredSession`, never re-registers
- [X] T065 [US4] `originate_and_bridge` (`agent.rs`, new) builds and sends the INVITE via T062's builders + T064's dialog, offering via `sdp::build_offer`/`sdp::CodecOffer::preferring_wideband`. **Also required, discovered during research (not in the original task list)**: Agent A had no always-on listener a caller could reach — `run_status_listener` was extended to accept `PlaceCall` too (a genuinely different connection lifecycle than `StatusQuery`'s one-shot reply, so it hands the raw `TcpStream` off via a new `mpsc` channel — `PendingPlaceCall` — rather than answering inline), and `dispatch_loop` gained a new poll of that channel, checked every iteration: an immediate `busy` `CallFailed` if a call is already active (so Agent B never waits out someone else's call), otherwise `originate_and_bridge` runs
- [X] T066 [US4] On a final 2xx: ACK sent, `sdp::parse_answer` used, `spawn_veth_uas_listener` started (unmodified, already used for inbound) *before* `CallPlaced` is sent — same ordering `handle_invite` uses so the listener is guaranteed up first — then waits for Agent B's veth call and bridges via the existing `spawn_relay`/`spawn_transcoding_relay`. On non-2xx/timeout/no-veth-call: `CallFailed` sent, and if the carrier leg was already up when the veth call failed to arrive, a BYE is sent to it rather than leaving it connected with no media path
- [X] T067 [P] [US4] **Descoped, matches existing precedent**: `DialogInfo::from_invite` (the UAS constructor this mirrors) has no dedicated unit test either in this codebase — dialog construction isn't unit-tested at that level here; it's covered by the wire-level `ims::call`/`sip_client` builder tests (T063) plus live verification (T072). Kept consistent with that existing boundary rather than introducing a new fixture pattern just for this one constructor

### Agent B: dispatch selection and bridging

- [X] T068 [US4] `run_outbound_listener` (`vowifi/mod.rs`) rewritten: tries every configured line via the new `try_place_on_line` helper (connects to that line's own Agent A on `AGENT_A_STATUS_PORT`, sends `PlaceCall`, waits for `CallPlaced`/`CallFailed`) before falling back to `ControlCmd::Dial` for circuit-switched. **Honest scope note**: this is "try VoWiFi/VoLTE lines in list order, then CS" — a real but arbitrary ordering, not strict FR-007 unordered arbitration across every process simultaneously; a "busy" reply costs no carrier round trip (Agent A answers it before touching the carrier transport), so trying several lines in sequence is cheap and safe, but it is still an ordering, not a preference-free selection
- [X] T069 [US4] New `bridge_outbound_leg` helper: on `try_place_on_line` returning `Ok`, places the veth-side `Call::make` toward that line's Agent A (now waiting) and `pair_calls`s it with the already-accepted phone/PBX leg — `bridge_call`'s exact mechanism, roles reversed. `OUTBOUND_ATTEMPTS_TOTAL` incremented on every path (placed via VoWiFi, placed via CS, or refused)
- [X] T070 [P] [US4] Confirmed by construction, not a new test: `try_place_on_line`/`bridge_outbound_leg` operate on `RuntimeLine` generically — nothing in the outbound path branches on whether a line's SIM came from a modem or a `pcsc_reader` (that distinction is resolved entirely upstream, at line/IMSI discovery). No separate code path exists to test against

### Observability (cross-process metrics gap found in pass 3's live test)

- [ ] T071 [US4] Fix `OUTBOUND_ATTEMPTS_TOTAL` never appearing on `/metrics`: `vowifi-sip-agent` is a separate process from the one serving `/metrics` (found live in pass 3 — the counter registers in the wrong process's local `REGISTRY`). Report it over the existing agent-reporting channel the same way `sip_server_bindings`/`sip_server_ring_aor_registered` already solve this (spec 024's fix, `2396800`) — extend `AgentState`/`ObservedEvent` (`control/protocol.rs`) with outbound-outcome fields/events, forward from `run_outbound_listener` and T069's `CallFailed` handling

### Verification

- [ ] T072 [US4] **Hardware verification** (no source file): with the user's involvement (per the caution already established this session — this touches the live VoWiFi tunnel registration), place a real outbound call over the VoWiFi line and confirm the originated INVITE reaches the carrier, the call connects, and audio bridges. Same live-test discipline as pass 3: confirm before disrupting anything, keep the blast radius minimal, revert any temporary config after
- [ ] T073 [P] [US4] Same verification over the PC/SC-sourced line, confirming T070's "no separate code path" claim holds for a real call, not just discovery/config

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
