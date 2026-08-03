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

**T017a/T017d superseded, 2026-08-03 (fifth code review)**: `ControlCmd::Dial`
and its `handle_control_cmd` arm have since been deleted — see
`contracts/control-cmd-dial.md`'s superseded banner. `ModuleCmd::Dial`
(T017b/c) is unaffected and still does the actual dialing, now reached
directly from `CardPool::handle_outbound_request` for the same-process case
(this task list's own T018 entry below described it going through
`ControlCmd::Dial`, which turned out not to match what was actually built);
`apply_dial_cmd`'s unit tests (T017e) remain valid and unaffected.

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

- [X] T071 [US4] Fixed: `ObservedEvent::OutboundAttempt { outcome: OutboundAttemptOutcome }` added (`control/protocol.rs`), applied in `metrics/ingest.rs::apply_event`. `run_telephony_side` spawns a dedicated `Reporter` (mirroring the existing `Sip`/`VolteSip` transport-label mapping already used for the SMS reporter) and passes it into `run_outbound_listener`, which now reports every outcome via `report_outbound()` instead of incrementing `OUTBOUND_ATTEMPTS_TOTAL` directly — that direct form stays correct and unchanged in `modules::mod` (same process as `/metrics`)

### Verification

- [X] T072 [US4] **Hardware verification, seven passes, 2026-08-03 — PASSED on pass 7**:
  - **Pass 1, BLOCKED — real bug found and fixed**: dialed +919789063708 via the registrar-redirect path with both VoWiFi lines up. Line 1 (PC/SC): Agent A sent the INVITE, carrier returned 100/183/180/**200 OK** — the call genuinely connected on the carrier side (confirmed by the user: "i got a call, no audio") — but `try_place_on_line`'s `PLACE_CALL_TIMEOUT` (`vowifi/mod.rs`, 3s) was a read-timeout on Agent A's `CallPlaced` reply, and Agent A didn't send `CallPlaced` until *after* the carrier INVITE transaction resolved (up to `OUTBOUND_INVITE_TIMEOUT`=15s for a real ringing call). Agent B gave up at 3s, moved on, and the answered carrier call was left unbridged — Agent A's own `veth_rx` wait then timed out and it sent BYE, hanging up the real, answered call. **Fixed**: added `ControlMessage::CallAttempting` (Agent A → Agent B, sent immediately once not-busy, before touching the carrier at all); `try_place_on_line` now does a two-phase read — short `PLACE_CALL_TIMEOUT` for the busy/reachability check, then a much longer `CALL_ATTEMPT_TIMEOUT` (25s, cross-checked by test against `OUTBOUND_INVITE_TIMEOUT`) once committed. Separately, line 0's Gm TCP connection had silently reset ~6 min after registration (`Connection reset by peer`) with no reconnect logic — a second, independent, still-open finding (dead-connection detection/reconnect gap, not yet fixed).
  - **Pass 2, BLOCKED — second real bug found and fixed**: re-tested after the `CallAttempting` fix; still got `503`/"no idle line available" on a fresh container. Root cause: `dispatch_loop` only re-checks `place_call_rx` once per loop iteration, and its sole blocking wait (`inbound.rx.recv_timeout(poll)`) used `REGISTRATION_POLL_INTERVAL`=30s when idle — so a `PlaceCall` could sit unnoticed for up to 30s, long past `PLACE_CALL_TIMEOUT`'s 3s busy-check window, before Agent A even sent `CallAttempting`. **Fixed**: renamed/repurposed to `IDLE_POLL_INTERVAL`=1s (renewal checks stay plenty timely against the 300s `RENEWAL_HEADROOM`); added a second cross-check test (`place_call_timeout_exceeds_agent_as_idle_poll`) asserting `PLACE_CALL_TIMEOUT` clears it with margin.
  - **Pass 3, BLOCKED — third real bug found and fixed**: re-tested after both fixes above. This time Agent B correctly committed to line 1 (no premature "no idle line") and the carrier INVITE went out, rang the real phone (100/183/183/180 Ringing), and the callee (a person, this project's own test number) answered — but only ~24s after the INVITE was sent. `OUTBOUND_INVITE_TIMEOUT` (`ims/agent.rs`, 15s, "generous for a real carrier round trip") was sized for network-layer round trip, not for how long a human actually takes to notice and answer a ringing phone — Agent A's `recv_final_response` gave up at ~15s, and the real 200 OK arrived after the transaction had already been abandoned ("received response outside an active transaction") — including an observed 18s gap between `100 Trying` and the next provisional, apparently carrier-side routing rather than the callee's own ring time. **Fixed**: added `SipTransport::recv_final_response_for_origination` (`ims/sip_client.rs`) — two-phase like the earlier fixes: `OUTBOUND_INVITE_TIMEOUT` (15s) for *any* response at all, then a single, longer `OUTBOUND_RING_TIMEOUT` (60s) once the call is confirmed in flight, not reset per provisional (so a retransmission flood can't wedge it open forever). `CALL_ATTEMPT_TIMEOUT` raised to 90s and its cross-check test updated to sum both constants. The original shared `recv_final_response` (used by `ims::call::run_call`, the CLI diagnostic tool) was left untouched rather than modified in place, since that call site already uses its own generous, configurable `ring_timeout` and has different needs (a human operator watching it run, not a second process racing a timeout).
  - **Pass 4, BLOCKED — fourth real bug found and fixed**: re-tested with an improved test script that plays a real 440Hz tone over RTP after the call connects (`test_outbound_call.py`, scratchpad). Call connected cleanly (200 OK) — but the user, holding the real phone, heard no tone at all and reported the call ending right after answering. Root cause, found in `pjsua-safe/src/endpoint.rs`'s `on_call_media_state_cb`: `bridge_outbound_leg` (`vowifi/mod.rs`) called `call.answer(200)` *before* `endpoint.pair_calls(...)`, but `pair_calls` is explicitly documented as safe to call before either call's media is active ("idempotent... happen lazily"). Answering can complete the INVITE transaction and fire the phone leg's media-active callback on a PJSIP worker thread within microseconds — observed live: `call.answer` and the resulting "audio connected to sound device" log landed *before* "placed and paired both legs" even ran, by about 100 microseconds. With no pairing registered yet, that callback fell through to the "connect to the sound device" branch (meant only for the circuit-switched bridge) instead of "connect to peer" — the phone leg's audio silently went nowhere. **Fixed**: swapped the two lines — `pair_calls` now runs before `answer(200)`.
  - **Pass 5, BLOCKED — fifth real bug found and fixed (regression from the pass 4 fix)**: re-tested twice after the reorder fix; both times the call connected for ~2s with no tone, then dropped — worse than pass 4. Container logs showed `Assertion failed: source >= 0 && sink >= 0 (pjsua_aud.c: pjsua_conf_connect2: 976)` immediately after the phone leg's media went active, followed by `[supervise] vowifi-sip-agent exited; restarting` — **the reorder didn't just fail to bridge, it crashed the whole process**, twice, reproducibly. Root cause: reordering made it far more likely that the phone leg's media-active callback fires while checking the veth leg as a peer — `pjsua_call_get_info(peer_id, &mut peer_info)` can return `media_status == ACTIVE` for the peer while its `conf_slot` is still `-1` (a narrow PJSIP-internal window between the media-active flag flipping and the conference slot actually being assigned), and the existing code passed that unvalidated `conf_slot` straight into `pjsua_conf_connect`, which asserts both slots are `>= 0` and aborts the process otherwise. **Fixed**: `on_call_media_state_cb` now also requires `peer_slot >= 0` before calling `pjsua_conf_connect`; when it isn't, this call's attempt is skipped exactly like the existing "peer not active yet" case — the peer's own later callback (once its slot is genuinely valid) completes the connection symmetrically, same retry path already relied on for that case.
  - **Pass 6, no crash, still no audio**: re-tested after the slot-validity fix — no crash, `conference-connected to each other` logged cleanly, call ran its full ~6s duration with a clean BYE teardown. Still no audible tone. Agent A's own low-level per-socket RTP counters (`ims::agent`'s `MediaMeter`, independent of the conference bridge's own state) told the real story: `media="receive-only" carrier_rx=2 pbx_rx=0` — Agent A received **zero** RTP packets over the veth link from Agent B's side, for the whole call, despite the bridge wiring being correct. Added a temporary diagnostic (`pjsua_call_dump` logged on `PJSIP_INV_STATE_DISCONNECTED`, `pjsua-safe/src/endpoint.rs`, marked for removal) to get PJSIP's own internal stream stats for the next pass, since packet capture isn't available in this sandbox (no `CAP_NET_RAW`).
  - **Pass 7, BLOCKED — sixth real bug found and fixed, root cause of "no audio" across passes 4/6**: re-tested with the diagnostic in place. Found the real cause by reading Agent A's own logs precisely: `"outbound: call placed and bridged"` and `"Agent B's control connection dropped mid-call"` landed **260 microseconds apart** — the bridge was torn down essentially the instant it was established, regardless of my test script's real ~6s tone (which was still running when this happened). Root cause: `try_place_on_line` (`vowifi/mod.rs`) connected to Agent A, got `CallPlaced`, and **returned**, dropping its `TcpStream` — Agent A's next read on its own end of that same TCP connection saw an immediate EOF, which `dispatch_loop` correctly (per its own contract) treats as "the PBX side is gone," so it hung up the just-answered carrier call within microseconds. The conference bridge (pass 6) and the transcoding relay were both actually wired up correctly the whole time; there was just nothing downstream by the time either had anything to carry — matches `pbx_rx=0` exactly. The established *inbound* direction (`handle_connection`, same file) already gets this right: it holds its accepted connection open for the whole call in one long-lived per-connection thread, exchanging `CallEnded` in either direction as the call ends. The outbound direction never grew the same lifecycle handling — it was written as a one-shot request/reply helper. **Fixed**: `try_place_on_line` now returns the still-open `BufReader<TcpStream>` instead of dropping it (with a short `OUTBOUND_POLL_INTERVAL` read timeout from that point on); `bridge_outbound_leg` now also returns the veth `Call`; `run_outbound_listener` holds both plus the connection in a new `ActiveOutboundCall`, checked once per poll tick by `service_active_outbound_call` — mirrors `handle_connection`'s inbound teardown logic (relay `CallEnded` in whichever direction hangs up first) but single-threaded/polling rather than a dedicated thread, since `pjsua_safe`'s `Endpoint`/`Account` don't implement `Clone` (thin handles onto a process-global PJSUA singleton with their own `Drop`) and `std::thread::scope`'s spawn/join semantics don't fit a call that must outlive one poll iteration.   - **Pass 7 result — SUCCESS**: `test_outbound_call.py` registered as `outboundtest`, dialed +919789063708, sent a 440Hz tone after answer, and hung up cleanly. Agent A's own log confirmed a real, sustained, bidirectional call: `call media verdict media="both-ways" carrier_rx=765 pbx_rx=326 ended_by="caller_hangup"`, running the full ~28s (carrier ring/setup + 6s tone + signaling overhead) rather than dropping in microseconds. **The user confirmed hearing the tone live** ("call landed, heard the tone (long beep)") — genuine, audible, end-to-end outbound audio over VoWiFi, line 0 (modem-sourced). The temporary `pjsua_call_dump` diagnostic from pass 6 was removed afterward (`pjsua-safe/src/endpoint.rs`). `config.toml` reverted to its pre-test baseline and the container restarted clean; the `outboundtest` account and `[outbound]` block were never left enabled.
  - **Also found, unrelated to the outbound-calling feature itself**: this session's sandboxed environment has no working access to real desktop audio hardware — `parecord`/PulseAudio capture of the "default source" returns bytes that decode as pure digital silence (verified two ways: raw `ulaw` and `s16le` capture, both all-zero with zero variance, which no real microphone produces even at rest) despite reporting success and a valid default source. A "bridge the local system microphone into the call" test therefore wasn't buildable from within this session; the RTP-tone approach (pass 4 onward) was the substitute verification path, and it worked.
  - **Still open, out of scope for this pass**: line 0's Gm TCP connection silently resetting minutes after registration with no reconnect logic (found pass 1) — a separate, pre-existing resilience gap, not part of outbound calling's own correctness.
- [X] T073 [P] [US4] Confirmed by the same pass 7 run — no separate code path exists for PC/SC vs. modem-sourced lines (T070's construction-based argument), and passes 1/3 already independently exercised the PC/SC line's carrier leg up through a real carrier answer before the earlier bugs (since fixed) cut those calls short.

### Second code review, 2026-08-03 (post T072/T073, before any live re-test)

An independent review of the live-verified code found five more real
issues, all triaged and fixed except where noted:

- **No CANCEL sent for an abandoned INVITE (RFC 3261 §9.1)**: `originate_and_bridge` gave up on a pending INVITE (timeout, or after this review's own CANCEL work) without ever telling the carrier — it just stopped reading, leaving the carrier free to keep ringing the destination for as long as *it* was willing to wait. **Fixed**: added `ims::call::build_cancel`/`CancelParts` (reuses the original INVITE's own branch/CSeq number per §9.1) and `ims::agent::cancel_pending_invite`, called when `recv_final_response_for_origination` times out; gives a short bounded window afterward for the `487`/`200` race (a `200` despite the CANCEL is ACKed then immediately BYE'd). **Scoped down**: a caller hanging up mid-ring while Agent A is still blocked waiting on the carrier still isn't observable — `dispatch_loop` has no way to watch the phone/PBX control connection while inside this blocking wait. Fixing that needs an interruptible wait on both agents, a materially bigger change; left as a documented, known limitation.
- **FR-012's progress table (`contracts/sip-dialout.md`) was entirely unimplemented for VoWiFi/VoLTE**: the phone leg was answered `200` only after the carrier's own `200` (silence for up to 75s, then a sudden answer — no `180 Ringing` relay existed at all), and every carrier rejection collapsed to a blanket `503` (the real status arrived inside `CallFailed.reason` but nothing read it). **Fixed**: `ControlMessage::CallRinging` (Agent A → Agent B, sent once per call on the carrier's `180`, relayed as `call.answer(180)`) — `SipTransport::recv_final_response_for_origination` gained an `on_provisional` callback to make this possible without restructuring its own timeout logic. `vowifi::mod::carrier_status_from_reason` reads the real carrier status back out of `CallFailed.reason` (`ims::agent::fail`'s non-2xx branch already formats it as `"{status} {reason}"` — the one place with a real code to report) instead of always answering `503`. The CS/AT-dial path's own equivalent gap (T019) stays separately, honestly flagged — this fix is VoWiFi/VoLTE-only.
- **FR-009a violated — a carrier rejection retried on every other line**: `try_place_on_line` returned the same `Err(String)` for "busy, cheap, try next line" and "committed, then rejected by the carrier" — a destination that rejected the call got redialed once per remaining VoWiFi/VoLTE line before finally being refused. **Fixed**: replaced the `Result` return with `PlaceCallOutcome::{Placed, Unavailable, Committed}` — only `Unavailable` (pre-`CallAttempting`) tries the next line; `Committed` (post-`CallAttempting`) stops immediately and refuses the whole request with the carrier's real status code (`outbound_outcome_for_committed_failure`, unit-tested).
- **`OutboundAttemptOutcome::Unanswered` declared, never emitted (SC-005)**: existed on the wire (`control/protocol.rs`) and in `sip::outbound::OutboundOutcome` with zero increment sites anywhere — a genuine no-answer was indistinguishable from any other rejection in logs/metrics. **Fixed on the VoWiFi/VoLTE path**: `ims::agent`'s carrier-timeout `fail()` call now marks its reason with the (previously-declared-but-unused) `reason::CARRIER_TIMEOUT` constant; `outbound_outcome_for_committed_failure` recognizes it (and an explicit carrier `480`) and reports `Unanswered` instead of `RefusedNetworkFailure`. **Still open**: the CS/AT-dial path's own `Unanswered` emission — matches AT-command call-progress tracking's general immaturity here (T019), deferred with it rather than fixed in isolation.
- **US3's destination extraction rested on an unstated, phone-dependent assumption**: the registrar's redirect `Contact` carried the phone's own AOR (`sip:{aor}@host:port`), not the dialed destination — relying entirely on the phone preserving the *original* To header into its retry INVITE's To, which RFC 3261 §8.1.3.4 does not require (a UAC only *MAY* copy the 3xx's Contact into its retry's Request-URI, and nothing governs To at all). A handset that just follows the redirect as given would send its own extension as the destination. **Fixed**: the Contact now carries the real destination directly (`sip:{destination}@host:port`, extracted from the *first* INVITE's own Request-URI via the new `uri_user` helper) — correct regardless of what the retry's To header turns out to be. Updated `contracts/sip-dialout.md`'s "SIP server mode path" and the existing redirect test (which had been asserting the *old*, now-fixed-as-wrong behavior).
- **Multi-card CS: an outbound call could clobber an in-progress inbound one — already fixed as a side effect**: `SipBridge` tracks a single `active_call` field shared by *both* directions (`make_call` for inbound GSM→PBX, `accept_outbound` for outbound PBX→GSM), so accepting an outbound call on an idle modem slot while a different slot was mid-inbound-call would silently overwrite `active_call` and orphan the live call. Investigated and found already covered: the busy check `poll_outbound_request` gained during the FR-009a-shaped CS fix earlier this session (`self.active_call.is_some()` → refuse `503`) checks the exact same shared field, so it already catches this case too, not just the "two outbound requests" case it was written for. Documented the connection explicitly in the code comment; no new code needed.

`make format`/`lint`/`test` (910 tests, +4 new: `build_cancel` reuses the
INVITE's branch/CSeq, `call_ringing_roundtrips`,
`carrier_status_from_reason_reads_the_leading_sip_code`,
`committed_failure_outcome_distinguishes_unanswered_from_refused`,
`a_uri_user_is_extracted_as_the_dial_out_destination`) and the
`pjsip-linked` build are all clean. Not yet re-verified live — none of
this pass's fixes have been exercised against real hardware yet.

### Third code review, 2026-08-03 (three more findings)

- **Real bug, fixed**: `service_active_outbound_call` (`vowifi/mod.rs`) called `read_line` into a fresh, local `String` every tick. `read_line` documents that any bytes it already appended stay in the buffer even when it returns an error — but that guarantee is worthless when the buffer itself gets discarded (dropped, local to the call) the instant the 200ms `OUTBOUND_POLL_INTERVAL` timeout fires mid-message. A message split across that boundary silently lost its first half, and whatever arrived afterward became an orphaned, unparseable fragment on the next tick — no crash, no log beyond a generic "malformed message" warning, just a dropped `CallEnded`. **Fixed**: `pending_line: String` moved from a per-call local into a field of `ActiveOutboundCall`, only cleared once a complete line is actually consumed; a timeout leaves it as-is so the next `read_line` call naturally appends the continuation instead of starting over. New regression test forces exactly this split (writes a real message across two `TcpStream::write_all` calls with a gap longer than the read timeout in between) and asserts the reassembled message parses correctly.
- **Known limitation, documented (not fixed) — VoWiFi/VoLTE side**: `originate_and_bridge` blocks `ims::agent::dispatch_loop` entirely for up to `OUTBOUND_INVITE_TIMEOUT + OUTBOUND_RING_TIMEOUT + VETH_INVITE_TIMEOUT` (~80s). Nothing else the loop is responsible for runs during that window — an inbound carrier INVITE arriving then is only *technically* not lost (its bytes sit in `inbound.rx`, fed by independent reader threads) but is effectively dropped anyway, since the caller's own SIP Timer B (32s) will very likely have already given up long before this loop gets back around to it. This is the same underlying gap as the "caller hangs up mid-ring" limitation this session's CANCEL work (finding #1, second code review) already scoped out — properly bounding either needs an interruptible wait on this loop, a materially bigger change than either pass makes. Documented prominently at the `originate_and_bridge` call site and in `cancel_pending_invite`'s own doc comment, rather than left silent.
- **Known, bounded trade-off — CS side, already reasonably documented, strengthened**: `handle_control_cmd`'s `ControlCmd::Dial` arm awaits its dial round-trip inline, blocking `CardPool`'s whole `select!` loop (not just the one line) — hotplug rescan, retry scheduling, and every other slot's `BridgeEvent` handling all stall too. The existing doc comment already justified the *duration* (bounded to 5s, shorter than `SetMode`'s existing 30s precedent) but didn't name what else the block affects; expanded it to say so explicitly. Meaningfully smaller blast radius than the VoWiFi/VoLTE side (5s vs. ~80s), so left as a documented trade-off rather than restructured.

`make format`/`lint`/`test` (911 tests, +1 new:
`a_message_split_across_a_read_timeout_is_not_lost`) and the
`pjsip-linked` build are all clean.

### Fourth code review, 2026-08-03 (dead code and a missing call-history record)

- **`sip/outbound.rs` was largely dead code documenting behavior the code doesn't have**: `Origin`, `OutboundCallRequest`, `CarrierPath`, and `CandidateLine` (plus `select_idle_line`) had zero callers — both real paths (`modules::mod::handle_outbound_request` for CS, `vowifi::mod::run_outbound_listener` for VoWiFi/VoLTE) had already grown their own line-selection logic against their own state (`SlotState`, `RuntimeLine`) well before this module was ever wired up. Worse, `CandidateLine::idle`'s own doc comment claimed to be "the sole definition of 'idle' (FR-005)" — false, since nothing called it. **Fixed**: deleted the dead types/function and their tests; kept `OutboundOutcome`/`validate_destination` (genuinely used by both paths); added a module doc comment explaining the deletion and pointing to `data-model.md` for where the conceptual model still lives.
- **`control/line_client.rs`/`control/line_server.rs` were doc-comment-only stubs, and `control/protocol.rs` carried an entirely unused `PlaceCall`/`PlaceCallOutcome` wire protocol** — confusable with the real, live `vowifi::control::ControlMessage::PlaceCall` it shares a name with, but a completely separate, never-implemented cross-process design (`contracts/line-command.md`). **Fixed**: deleted both stub files, their `pub mod` declarations in `control/mod.rs`, and the dead protocol section in `control/protocol.rs` (confirmed zero callers before deleting). Added a "SUPERSEDED" banner to `contracts/line-command.md` and rewrote `data-model.md`'s "Line selection"/"PlaceCall command" sections to describe the two real dispatch paths instead of the deleted abstraction. Marked Phase 8 (T042–T046) below as superseded rather than leaving it looking like outstanding work.
- **`try_place_on_line`'s doc comment was misattached to `report_outbound`**: a stale paragraph describing `try_place_on_line`'s old `Result<(), String>`-shaped API (superseded by the `PlaceCallOutcome` three-way split from the second review) sat directly above the unrelated `report_outbound` function, while `try_place_on_line`'s own doc comment further down (already correct/up to date) duplicated the real description. **Fixed**: deleted the misplaced stale paragraph.
- **Real bug, fixed — outbound CS calls produced no call-store record and never set `ACTIVE_CALLS`**: inbound calls get their call-history bookkeeping from `handle_ring` (`modules/mod.rs`), but outbound calls never called anything equivalent — `call_ctx` stayed `None` for the whole call, so `record_call_end` had nothing to record and the call was invisible in call history, even though `ACTIVE_CALLS.set(0.0)` still fired on teardown (misleadingly implying a call had been tracked). **Fixed**: added `record_call_start_outbound(module_id, destination, call_ctx)`, mirroring `handle_ring`'s bookkeeping (`CallContext` with `caller_id` set to the dialed destination, `ACTIVE_CALLS` set to 1.0, `CALLS_TOTAL` incremented with the `"outgoing"` label), wired into `ModuleCmd::Dial`'s handling right after a successful `apply_dial_cmd`. New test `an_outbound_call_produces_a_real_call_history_record` verifies the full start→end lifecycle produces a real `StoreCommand::InsertCall` with the right `caller_id`/`status`/`module_id`.
  - **Related, investigated and ruled out**: checked whether `handle_hangup`'s NO-CARRIER path could misclassify an answered outbound call as "missed," since outbound calls never reach `CardState::Bridged` (only `handle_ring`, inbound-only, sets it — outbound calls stay in `CardState::Answering` for their whole duration). Traced the actual condition and confirmed it already checks `Bridged || Answering`, so this is not a bug; no additional fix made.

`make format`/`lint`/`test` (909 tests: 911 − 3 removed `sip::outbound` line-selection
tests + 1 new `an_outbound_call_produces_a_real_call_history_record`) and the
`pjsip-linked` build are all clean. Not yet re-verified live — none of this
pass's changes have been exercised against real hardware yet.

### Fifth code review, 2026-08-03 (a silent hangup, a stale wire code, and more dead code)

- **Real bug, fixed — `bridge_outbound_leg` failure left the caller with no final response**: the `Err(e)` branch in `run_outbound_listener` (after `try_place_on_line` succeeded but bridging then failed) logged, reported `RefusedNetworkFailure`, and moved on — never answering the phone/PBX leg. Every other terminal path in this loop answers with a status code; this one left the caller ringing until its own timeout. **Fixed**: added `let _ = call.answer(503);` to that branch. Two related leaks on the same path, both inside `bridge_outbound_leg` itself: if `call.answer(200)` failed *after* `pair_calls` and the veth `Call::make` had already succeeded, the veth call was dropped without `hangup()` (`pjsua_safe::Call` has no `Drop`) and the just-made `BRIDGE_PAIRS` entry was never removed via `unpair_call`. **Fixed**: `bridge_outbound_leg` now unpairs and hangs up the veth call internally before returning `Err` in that case.
- **Real bug, fixed — `ModuleCmd::Hangup` reopened the call-history gap the fourth review closed**: `record_call_start_outbound` populates `call_ctx` once `ATD` is accepted, but `ModuleCmd::Hangup`'s handler (sent when the SIP side rejects a call right after a successful dial) did `at.hangup()`/`card.state = Idle`/`ACTIVE_CALLS.set(0)` without ever calling `record_call_end` — the attempt vanished from history again, and `call_ctx` stayed `Some` with a stale context until the next `handle_ring` silently overwrote it. **Fixed**: added `record_call_end(&module.id, &event_tx, &store_tx, &mut call_ctx, "failed")`, matching what `handle_hangup` already does for the inbound NO-CARRIER case. New test `a_hung_up_outbound_attempt_produces_a_failed_call_history_record`.
- **Real bug, fixed — `carrier_status_from_reason` could pass any 3-digit code straight to `Call::answer`**: a carrier `302` or `202` reaching `fail()`'s `"{status} {reason}"` format would become `call.answer(302)` toward the phone (a redirect with no `Contact` to give it) or `call.answer(202)` (a 2xx that isn't really an answer) — both nonsensical on a *failure* path. **Fixed**: `outbound_outcome_for_committed_failure` now only passes a code through when it's in `400..700`; anything else (including the `480`/general cases already handled) falls back to `503`, keeping the `486`/`480` passthrough that motivated the original code intact. New test `committed_failure_outcome_clamps_non_failure_codes_to_503`.
- **Dead code, deleted — `ControlCmd::Dial` had zero callers**: its only real caller was `vowifi::mod`'s cross-process CS fallback, itself removed in the second code review (no cross-process audio bridge exists for CS, so it used to answer `200 OK` over dead air). The genuinely same-process case (`CardPool::handle_outbound_request`) never went through it either — it dispatches `ModuleCmd::Dial` directly. Left as ~110 lines of unreachable code (the variant, and its full `handle_control_cmd` arm with its claim discipline and 5s blocking await) with no CLI subcommand to reach it. **Fixed**: deleted the variant and its handler (matching the standard set by the fourth review's `sip/outbound.rs`/`control::line_client`/`line_server` deletions); added a superseded banner to `contracts/control-cmd-dial.md`; corrected `data-model.md` and `contracts/agent-outbound-protocol.md`'s stale references (the latter also still referenced the already-deleted `select_idle_line`/`CandidateLine.path` from the fourth review — missed at the time, caught and fixed here).
- **Stale docs, fixed — `hangup_answered_carrier_leg` had inherited `originate_and_bridge`'s doc comment**: the new function was inserted mid-doc-block in an earlier pass, so it read as if it originated and bridged calls, while `originate_and_bridge` was left with only an unrelated "Never re-registers" paragraph — the same misattachment shape as `report_outbound`'s in the fourth review. **Fixed**: split the doc block back onto its two functions.
- **Stale docs, fixed — `run_outbound_listener`'s doc comment still described the removed CS fallback**: "Falls back to the circuit-switched daemon via `ControlCmd::Dial`... lines are tried in the order `lines` lists them, then CS" — the code has refused outright since the second code review. **Fixed**: rewrote the paragraph to describe the actual no-CS-fallback behavior and why (no cross-process audio bridge), and dropped "then CS" from the FR-007 simplification note below it.

`make format`/`lint`/`test` (911 tests: +2 new,
`committed_failure_outcome_clamps_non_failure_codes_to_503` and
`a_hung_up_outbound_attempt_produces_a_failed_call_history_record`) and the
`pjsip-linked` build are all clean. Not yet re-verified live.

---

## Phase 8: Cross-process line-command channel (was Phase 2 of US2 in the original plan) — SUPERSEDED

**Purpose**: needed only once a deployment mixes CS with VoWiFi/VoLTE, or the
SIP side is hosted by an agent process that needs to reach a *different*
agent's line. Deferred until Phases 4–7 are solid, per plan.md Step 4.

**Superseded, 2026-08-03 (fourth code review)**: this whole phase never
started, and the cross-process case it was scoped for is already met by a
different, already-implemented mechanism —
`vowifi::control::ControlMessage::PlaceCall` over Agent A/B's existing TCP
control channel (`contracts/agent-outbound-protocol.md`), live-verified on
real hardware. The `control::line_server`/`line_client` module stubs and the
`control::protocol::PlaceCall`/`PlaceCallOutcome` wire types T042–T045 would
have built on are deleted (dead code with zero callers — see the fourth
review batch below and `contracts/line-command.md`'s superseded banner).
T042–T046 are left unchecked below as a record of the original design, not
as outstanding work; do not implement them.

- [ ] ~~T042 [US2] Implement `control::line_server`...~~ — superseded, see above
- [ ] ~~T043 [US2] Implement `control::line_client::place_call`...~~ — superseded, see above
- [ ] ~~T044 [US2] Extend `sip/outbound.rs`'s line selection to include cross-process `CandidateLine`s...~~ — superseded, `sip/outbound.rs` no longer has a `CandidateLine` type
- [ ] ~~T045 [US2] Implement the provisional-claim-then-command sequence for the cross-process case...~~ — superseded, see above
- [ ] ~~T046 [P] [US2] Create `test_outbound_line_command.rs`...~~ — superseded, no protocol left to test

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
