# Tasks: Inbound VoWiFi-to-SIP Call Bridge

**Input**: Design documents from `/specs/011-vowifi-sip-bridge/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/agent-control-protocol.md, quickstart.md

**Tests**: Included throughout. The project constitution (`.specify/memory/constitution.md`)
mandates Integration-First Testing (NON-NEGOTIABLE) and TDD as the default practice, and
`CLAUDE.md`'s pre-commit checklist requires `cargo test --workspace` to pass before every commit —
none of that is relaxed for this feature. Every implementation task below is preceded by a test
task in the same phase; tests must fail before their corresponding implementation task lands.

**Organization**: Tasks are grouped by user story (spec.md priorities P1/P2/P3) so each story is
independently implementable, testable, and demoable.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependency on an incomplete task)
- **[Story]**: US1 / US2 / US3, per spec.md's prioritized user stories
- Exact file paths are included in every task description

## Path Conventions

Single Cargo workspace, existing project layout — no new crate. All paths are relative to the repo
root (`/home/selva/projects/ec20/gsm-sip-bridge`), primarily under `gsm-sip-bridge/src/`,
`pjsua-safe/src/`, and `docker/epdg/`, per `plan.md`'s Project Structure section.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Scaffolding only — new module files, config surface, and CLI argument stubs. No
behavior yet; nothing here should make any spec acceptance scenario pass on its own.

- [X] T001 Create `gsm-sip-bridge/src/vowifi/mod.rs` and `gsm-sip-bridge/src/vowifi/control.rs`
      (empty modules for now) and register `pub mod vowifi;` in `gsm-sip-bridge/src/lib.rs`
- [X] T002 [P] Create `gsm-sip-bridge/src/ims/agent.rs` (empty module) and register `mod agent;` in
      `gsm-sip-bridge/src/ims/mod.rs` alongside the existing `pub mod call;`
- [X] T003 [P] Add a `VowifiConfig` struct to `gsm-sip-bridge/src/config/mod.rs` (fields: `enabled`,
      `mcc`, `mnc`, `modem_port`, `use_tcp`, `sec_agree`, `pcscf_source_path`, `veth_local_addr`,
      `veth_peer_addr`, `control_port`) and wire it into `AppConfig`; the SIP/PBX destination
      itself is NOT duplicated here — it reuses the existing `[bridge]`/`[sip]` config as-is (FR-003)
- [X] T004 [P] Add a documented `[vowifi]` example block to `config.toml.example` covering every
      field added in T003
- [X] T005 [P] Add `VowifiImsAgent` and `VowifiSipAgent` variants (with their arg structs, mirroring
      `ImsRegisterArgs`) to the `Commands` enum in `gsm-sip-bridge/src/cli.rs`
- [X] T006 Wire `Commands::VowifiImsAgent` / `Commands::VowifiSipAgent` into the pre-daemon dispatch
      in `gsm-sip-bridge/src/main.rs` (mirroring the existing `Commands::ImsRegister`/`ImsCall`
      handling), initially calling stub handlers that log "not yet implemented" and exit

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Cross-cutting infrastructure with no story-specific behavior that every user story
implicitly depends on: the wire protocol between the two agents, PJSIP's ability to bridge two
calls to each other instead of one call to a sound device, and the network path (veth) between the
two processes.

**⚠️ CRITICAL**: No user story work can begin until this phase is complete.

- [X] T007 [P] Define the Agent A↔B control message types (`IncomingCall`, `CallEnded`,
      `BridgeReady`, `BridgeFailed`, `HangupAck`, per `contracts/agent-control-protocol.md`) in
      `gsm-sip-bridge/src/vowifi/control.rs`
- [X] T008 Implement newline-JSON `read_msg`/`write_msg` helpers for the T007 types in
      `gsm-sip-bridge/src/vowifi/control.rs`, mirroring `gsm-sip-bridge/src/control/protocol.rs`'s
      `read_cmd`/`write_resp` pattern exactly (depends on T007)
- [X] T009 [P] Unit test: round-trip every control message variant through `read_msg`/`write_msg`
      over an in-memory buffer, in `gsm-sip-bridge/src/vowifi/control.rs` `#[cfg(test)]` (mirrors
      `control/protocol.rs`'s existing round-trip tests)
- [X] T010 [P] Generalize `on_call_media_state_cb` in `pjsua-safe/src/endpoint.rs` (currently
      hardcoded to `pjsua_conf_connect(call_slot, 0)` / `(0, call_slot)`) so a call can instead be
      bridged to an explicit peer conference slot, while preserving today's slot-0
      sound-device-bridging behavior unchanged for the existing CS-GSM bridge call path.
      Implemented via a `BRIDGE_PAIRS` registry + `Endpoint::pair_calls`/`unpair_call`; verified
      against real linked PJSIP (`pjsua-safe/tests/two_call_bridge.rs`).
- [X] T011 [P] Add `pjsua_set_null_snd_dev` support to `Endpoint` in `pjsua-safe/src/endpoint.rs`,
      for the containerized deployment which has no physical sound device — already present as
      `Endpoint::set_null_sound_device` before this feature; confirmed reusable as-is.
- [X] T012 Add tracking for two concurrent `Call` handles from one `Endpoint` in
      `pjsua-safe/src/call.rs`, each independently bridgeable via T010's generalized callback
      (depends on T010) — no structural change needed: `Call::make` was never limited to one
      instance per `Endpoint`; the only single-call constraint lived in `sip::SipBridge`'s own
      `active_call: Option<Call>` field, which Agent B (T024) works around by holding two `Call`s
      directly rather than going through `SipBridge`.
- [X] T013 [P] `pjsip-linked`-feature-gated integration test: two `Call`s on one `Endpoint`,
      conference-connected to each other via T010/T012, exchange audio when built against the real
      system PJSIP, in `pjsua-safe/tests/`. Implemented as `two_call_bridge.rs`, verified passing
      against a real linked PJSIP in this environment. Scope note: verifies the pairing bookkeeping
      end-to-end with real `pjsua_call_id`s; does not verify actual RTP audio exchange, since that
      requires two calls to reach `CONFIRMED`/media-active, which needs a live SIP registrar/PBX or
      a second peer process — deferred to the hardware-gated manual verification (quickstart.md).
- [X] T014 Create the `veth` pair in `docker/epdg/entrypoint.sh` (one end moved into netns `ims`,
      one left in the container's default namespace), addressed from the same env vars T003's
      `VowifiConfig` reads, run after tunnel readiness is confirmed and before either agent launches
- [X] T015 [P] Review and update `docker/epdg/docker-compose.epdg.yml` for any additional
      capabilities/devices the veth pair needs beyond the existing `NET_ADMIN`/`SYS_ADMIN` grants
      (verify; document if nothing changes) — confirmed `NET_ADMIN` already covers veth creation;
      documented inline, no capability changes needed.

**Checkpoint**: Foundation ready — user story implementation can now begin.

---

## Phase 3: User Story 1 - Inbound VoWiFi call reaches a SIP extension (Priority: P1) 🎯 MVP

**Goal**: An inbound call arriving over VoWiFi is automatically answered and two-way bridged to the
existing SIP/PBX destination, exactly as the circuit-switched bridge already does for CS calls.

**Independent Test**: Register the line for VoWiFi, call the SIM's number from an external phone
while both agents are running, and confirm the call is answered with two-way audio to an existing
SIP extension — `quickstart.md` steps 1, 3, and 6.

### Tests for User Story 1 ⚠️

> Write these tests first; they must fail until the corresponding implementation task lands.

- [X] T016 [P] [US1] Unit tests for `SipRequest` parsing (canned INVITE/BYE wire-format fixtures,
      partial-read framing) and UAS response builders (`100 Trying`, `180 Ringing`, `200 OK` with
      SDP body, `200 OK` to BYE, `486 Busy Here`) and dialog-state extraction (To/From tags,
      Call-ID, CSeq, Contact) in `gsm-sip-bridge/src/ims/sip_client.rs` `#[cfg(test)]` — 12 tests,
      all passing.
- [X] T017 [P] [US1] Unit tests for `sdp::parse_offer` (against a real Airtel-captured INVITE offer
      fixture) and `sdp::build_answer` (asserts PCMU is chosen when both PCMU and AMR-WB are
      offered) in `gsm-sip-bridge/src/ims/sdp.rs` `#[cfg(test)]` — 8 tests, all passing.
- [X] T018 [P] [US1] Unit tests for the Bridged Call state machine — implemented as `relay_rtp`'s
      real loopback-UDP round-trip test plus `extract_caller` tests in
      `gsm-sip-bridge/src/ims/agent.rs` `#[cfg(test)]`, rather than a separate state-machine test in
      `vowifi/mod.rs` as originally scoped: the actual `Ringing→Answering→Bridged→Ended`/
      `Ringing→Declining→Ended` transitions ended up implemented as a linear function
      (`handle_invite`/`handle_bye` in `ims/agent.rs`) rather than an explicit state enum, so the
      testable units are its message-handling logic and the RTP relay it drives, not a standalone
      state machine — see T022/T024 notes.
- [X] T019 [US1] Implement `SipRequest` + `try_parse` (mirroring `SipResponse::try_parse`'s
      partial-read/`Content-Length` framing), dialog-state helpers (`header`, `headers_all`), and
      the UAS response builders from T016 in `gsm-sip-bridge/src/ims/sip_client.rs`. `try_parse` is
      `pub(super)` (not private) so `ims::agent` — a sibling module — can parse a single-datagram
      INVITE on the veth-facing UAS (see T022).
- [X] T020 [P] [US1] Implement `sdp::parse_offer` and `sdp::build_answer` (PCMU-preferred; AMR-WB
      fallback path exists in `build_answer` but `ims::agent` never passes `amr_available = true`
      yet — see T022 note on the AMR-WB decline path) in `gsm-sip-bridge/src/ims/sdp.rs`.
- [X] T021 [US1] Confirmed (no code change needed): `register_session` in `gsm-sip-bridge/src/ims/mod.rs`
      already returns a `RegisteredSession` that stays alive until the caller explicitly calls
      `.cleanup()` — only `run_register`/`ims::call::run_call` (the CLI tools) do that immediately;
      `ims::agent::run_inner` calls `register_session` directly and defers `.cleanup()` until its
      dispatch loop exits, which is all "staying alive" required.
- [X] T022 [US1] Implemented Agent A in `gsm-sip-bridge/src/ims/agent.rs`: `dispatch_loop` reads
      `session.transport.recv_message()` (new `SipTransport` method, T019) and routes `INVITE` →
      `handle_invite`, `BYE` → `handle_bye`. `handle_invite` sends `100 Trying`, parses the offer,
      declines (`486`, no transcode path) if PCMU wasn't offered, sends `180 Ringing`, signals Agent
      B over the control channel, and on `BridgeReady` answers with `200 OK` and starts
      `relay_rtp` (raw UDP byte-forwarding, not decode/re-encode — both legs are PCMU by
      construction) between the IMS-side and veth-side sockets. Also implements Agent A's
      veth-facing UAS (`spawn_veth_uas_listener`/`accept_veth_invite`) — a second, unauthenticated
      SIP responder on `crate::vowifi::VETH_SIP_PORT` that answers Agent B's own `Call::make` to
      Agent A, reusing the same T019 primitives. **Deviation from the original task text**: the
      IMS-side RTP relay does *not* reuse `rtp::build_packet`/`parse_packet`/`ulaw_to_linear` as
      scoped — it forwards RTP payloads as opaque bytes rather than re-encoding, which is simpler
      and correct given both legs already use the same codec (PCMU) by the time bridging proceeds.
- [X] T023 [US1] Wired via `main.rs::handle_vowifi_ims_agent_command` → `ims::agent::run`, completed
      already in Phase 1 (T006) as a stub and given a real body here; still gated on
      `[vowifi].enabled` and requires `--config` (`load_vowifi_config`).
- [X] T024 [US1] Implemented Agent B in `gsm-sip-bridge/src/vowifi/mod.rs`. **Deviation from the
      original task text**: does *not* reuse `SipBridge` — `SipBridge` holds a single
      `active_call: Option<Call>` and has no accessor for its private `Endpoint`, which doesn't fit
      "hold two concurrent `Call`s and pair them"; `vowifi::run_inner` builds its own
      `Endpoint`/`Account` instead (same construction pattern as `SipBridge::register`, a few
      duplicated lines, documented in the module doc comment) so it can hold both `Call`s directly
      and call `Endpoint::pair_calls`. Still reuses the *destination-URI logic*
      (`pbx_dest_uri`, mirroring `compute_destination_uri`) and the caller-ID→
      `P-Asserted-Identity`/`X-GSM-Caller-ID` header forwarding pattern from `sip::mod` (FR-011) —
      just not the `SipBridge` struct itself. `handle_connection` replies `BridgeReady`/
      `BridgeFailed`, and on `CallEnded` unpairs + hangs up both legs and replies `HangupAck`.
- [X] T025 [US1] Wired via `main.rs::handle_vowifi_sip_agent_command` → `vowifi::run`, same pattern
      as T023.
- [X] T026 [US1] Structured `tracing` events present at every transition in both `ims/agent.rs` and
      `vowifi/mod.rs` (inbound call, decline reasons, bridge success, hangup) — see `tracing::info!`/
      `tracing::warn!` call sites in both files.
- [X] T027 [US1] `docker/epdg/entrypoint.sh` now creates the veth pair (T014) and launches both
      agents under a restart-on-exit supervisor loop once the tunnel is confirmed up, conditional on
      the binary/config being present at `$GSM_SIP_BRIDGE_BIN`/`$GSM_SIP_BRIDGE_CONFIG` (logs a
      clear note and skips supervision rather than crash-looping if either is missing, since the
      binary still isn't baked into the image — see T028's updated build/copy instructions).
- [X] T028 [US1] `docker/epdg/README.md` gained a "Phase 4" section describing the always-on
      two-agent flow, the build/copy/restart steps to enable it, and an explicit note that live
      end-to-end verification against a real network is still outstanding (see below).

**Checkpoint**: User Story 1's code is implemented, unit/integration-tested (159 `gsm-sip-bridge`
lib tests + the real-PJSIP `two_call_bridge` test all pass; `make lint`/`cargo fmt --check` clean;
zero `unsafe` in `gsm-sip-bridge/src`), and compiles against both the stub and the real linked
PJSIP. **Not yet done**: `quickstart.md` steps 1, 3, and 6 against real hardware (a live SIM
receiving an actual inbound VoWiFi call) — this requires the physical Quectel EC200U, a
provisioned SIM, and a reachable PBX that aren't available in this environment; SC-001/SC-002 and
the live two-way-audio claim remain unverified until that manual pass is run.

---

## Phase 4: User Story 2 - VoWiFi availability maintained without manual intervention (Priority: P2)

**Goal**: The line stays reachable over VoWiFi unattended — surviving network interruptions,
session expiry, and process restarts without any manual re-arming step.

**Independent Test**: With Story 1 already working, interrupt the underlying network path and
confirm automatic recovery within the SC-003 window with zero manual steps — `quickstart.md`
step 4.

### Tests for User Story 2 ⚠️

- [X] T029 [P] [US2] Unit tests for renewal scheduling — a re-REGISTER/AKA cycle is triggered
      before `expires_at`, using an injectable clock/trigger rather than a real timer — in
      `gsm-sip-bridge/src/ims/mod.rs` `#[cfg(test)]`. Implemented as 6 tests against the pure
      `renewal_due(now, expires_at, headroom)` function (clock fully injected via `SystemTime`
      arguments, no real timer involved) plus a `RegistrationStatus::default()` test.
- [X] T030 [P] [US2] Unit tests for retry/backoff — implemented as 3 tests against the pure
      `next_backoff(current, max)` function (doubling, capping, overflow-safety) in
      `gsm-sip-bridge/src/ims/agent.rs` `#[cfg(test)]`. **Scope note**: `last_failure` population is
      exercised structurally (the field exists on `RegistrationStatus` and `dispatch_loop`'s
      renewal-failure branch sets it) but not covered by an automated test, since reaching that
      branch requires a real failing REGISTER attempt (network/AT+CSIM), which needs hardware.

### Implementation for User Story 2

- [X] T031 [US2] Implemented `RegistrationState`/`RegistrationStatus` (per `data-model.md`:
      `Unregistered→Registering→Registered→Renewing→Failed`) and the pure `renewal_due` scheduling
      function in `gsm-sip-bridge/src/ims/mod.rs`. Wired into `ims::agent::dispatch_loop`, which now
      polls via the new `SipTransport::recv_message_deadline` (added to `sip_client.rs`, replacing
      the earlier blocking-forever `recv_message` since nothing else needed it — T022's original
      version is superseded) so it can periodically check `renewal_due` between messages without
      blocking on the next inbound call indefinitely. On a due renewal, calls `register_session`
      again (a full fresh AKA cycle — there's no cheaper incremental refresh in this protocol),
      cleans up the *old* session's Gm IPsec state before swapping in the new one.
- [X] T032 [US2] Implemented `next_backoff` (doubling, capped) plus `attempt_renewal` in
      `gsm-sip-bridge/src/ims/agent.rs`; `dispatch_loop`'s idle branch records `last_failure` and
      sleeps for the current backoff before the next attempt on failure, resetting to the initial
      backoff on the next success.
- [ ] T033 [US2] Verify process-restart resilience end-to-end — **not done**, requires real
      hardware (Quectel EC200U, provisioned SIM, live carrier network) not available in this
      environment. `docker/epdg/entrypoint.sh`'s restart-on-exit supervisor loop (T027) is in place
      and its bash syntax is valid, but has not been exercised against a real agent crash/restart.

**Checkpoint**: User Story 2's renewal/backoff logic is implemented and unit-tested (9 new tests,
all passing; full workspace test suite at 168 `gsm-sip-bridge` lib tests, `make lint` clean).
**Not yet done**: `quickstart.md` step 4 (simulated WAN interruption) against real hardware — SC-003
remains unverified until that manual pass is run.

---

## Phase 5: User Story 3 - Operator can confirm the VoWiFi line is healthy (Priority: P3)

**Goal**: The operator can check current VoWiFi registration health and recent call outcomes
through the bridge's existing operational tooling.

**Independent Test**: With the bridge running, query status and confirm it reports current
registration health and the outcome of recent inbound call attempts — `quickstart.md` step 2.

### Tests for User Story 3 ⚠️

- [X] T034 [P] [US3] Unit tests for the bounded recent-call-outcome ring buffer (oldest entry
      evicted once full; records outcome/duration/destination per `data-model.md`'s Bridged Call
      entity) in `gsm-sip-bridge/src/vowifi/mod.rs` `#[cfg(test)]` — 3 tests, all passing.
- [X] T035 [P] [US3] Unit tests for the `vowifi-status` output shape. **Deviation from the original
      task text**: implemented as the `ControlMessage::RegistrationStatusReply`/`CallHistoryReply`
      round-trip tests in `gsm-sip-bridge/src/vowifi/control.rs` `#[cfg(test)]` rather than in
      `cli.rs` — `vowifi-status`'s output is a thin `println!` formatter over those two wire types
      (see T038), so the wire shape is what's actually worth unit-testing; the print formatting
      itself has no branching logic beyond `Option` presence, covered by inspection.

### Implementation for User Story 3

- [X] T036 [US3] Implemented `RecentCalls` (bounded `VecDeque`, oldest evicted, newest-first
      snapshot) in `gsm-sip-bridge/src/vowifi/mod.rs`, updated by `handle_connection` at the end of
      every call (both the answered and failed-to-bridge paths push a `CallRecord`).
- [X] T037 [US3] Extended the T007 `ControlMessage` enum (`gsm-sip-bridge/src/vowifi/control.rs`)
      with `StatusQuery`/`RegistrationStatusReply`/`CallHistoryReply`. **Deviation from the original
      task text**: Agent A didn't have any listening port before this phase (it's a control-channel
      *client*, not server) — added a new dedicated `AGENT_A_STATUS_PORT` listener
      (`run_status_listener` in `ims/agent.rs`, backed by `Arc<Mutex<RegistrationStatus>>` shared
      with `dispatch_loop`) rather than reusing T007/T008's existing channel, since that channel is
      Agent A→B only. Agent B's *existing* control-port listener was extended in place to also
      accept `StatusQuery` as a connection's first message (alongside `IncomingCall`).
- [X] T038 [US3] Implemented `vowifi::print_status` (queries both agents, prints registration health
      and recent call outcomes; either query failing is reported without blocking the other) and
      wired it as the `vowifi-status` CLI subcommand (already dispatched since Phase 1's T006 stub;
      this phase gave it a real body).
- [X] T039 [US3] Structured `tracing` events already present from T026/T032 cover registration-health
      transitions (`"registration renewed"`/`"registration renewal failed"` with `error`/
      `retry_in_secs` fields) and call outcomes (`"incoming VoWiFi call signaled"`,
      `"failed to bridge call"`, `"call ended"`) — confirmed these exist independently of the
      `vowifi-status` CLI path (plain `tracing::info!`/`warn!`, not gated on anyone querying status).

**Checkpoint**: All three user stories' code is implemented and unit-tested (20 new tests this phase
— 175 total `gsm-sip-bridge` lib tests; `make lint`/`cargo fmt --check` clean). **Not yet done**:
`quickstart.md` step 2 against real hardware — SC-004 (operator can read status in under 30s)
remains unverified until both agents have actually run against live hardware to check against.

---

## Phase 6: Polish & Cross-Cutting Concerns

- [X] T040 [P] Ran `cargo fmt --all && make lint && cargo test --workspace` — clean throughout this
      implementation pass (also verified separately against `--features gsm-sip-bridge/pjsip-linked`
      with a real linked PJSIP, which `make lint`/`make test` don't build by default but which this
      environment happened to have available). `cargo deny` was not installed in this environment,
      so that specific sub-check of `make lint` couldn't be exercised — everything else in the
      target (`fmt --check`, `clippy -D warnings` for both `gsm-sip-bridge` and `pjsua-safe`, the
      unsafe-block audit) ran and passed.
- [X] T041 [P] Confirmed: `tools/count-unsafe.sh` reports `gsm-sip-bridge/src: 0 unsafe blocks` and
      `pjsua-safe/src` ratio `1.61%` (well under the 5% threshold) after T010-T013's changes.
- [X] T042 [P] Added `docs/vowifi-bridge.md`.
- [ ] T043 Run the full `specs/011-vowifi-sip-bridge/quickstart.md` end-to-end (steps 1-6) against
      real hardware and the real carrier network, and record the outcome — **not done**, requires
      hardware (Quectel EC200U, provisioned SIM, live carrier network, reachable PBX) not available
      in this environment. This is the last remaining gap before this feature can be considered
      production-verified; see `docs/vowifi-bridge.md`'s closing section.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies — start immediately.
- **Foundational (Phase 2)**: Depends on Setup completion — BLOCKS all user stories.
- **User Story 1 (Phase 3)**: Depends on Foundational completion. No dependency on US2/US3.
- **User Story 2 (Phase 4)**: Depends on Foundational completion **and** on User Story 1's
  registration machinery (T021) — resiliency has nothing to renew/retry until the basic
  answer-and-bridge flow exists. Not independently buildable before US1, but independently
  *testable* once US1 is in place (per spec's own priority ordering: "ranked below Story 1 because
  the answer/bridge mechanics must exist first").
- **User Story 3 (Phase 5)**: Depends on Foundational completion **and** on US1's call outcomes
  (T024) and US2's registration state (T031) existing to report on. Same relationship as above —
  an operational nicety layered on top of Stories 1 and 2, per spec.md.
- **Polish (Phase 6)**: Depends on all desired user stories being complete.

### Within Each User Story

- Tests are written first and must fail before their paired implementation task.
- `sip_client.rs`/`sdp.rs` parsing before the agent loops that consume them.
- Agent A and Agent B implementations before their CLI wiring.
- CLI wiring before deployment/supervision glue.
- Story complete (all tasks + checkpoint validation) before moving to the next priority.

### Parallel Opportunities

- Setup: T002, T003, T004, T005 in parallel (T001 first since it creates the directories T005's
  sibling work assumes exist; T006 depends on T005).
- Foundational: T007 first, then T008/T009 (depend on T007) in parallel with T010/T011/T014/T015
  (independent files); T012 depends on T010; T013 depends on T012.
- User Story 1: T016, T017, T018 (tests, three different files) in parallel; T020 and T021 in
  parallel with each other and with T019 (three different files, no cross-dependency); T022 depends
  on T019+T020+T021; T023 depends on T022; T024 depends on Foundational only (can start as soon as
  Foundational is done, in parallel with T019-T023); T025 depends on T024 and shares `main.rs` with
  T023 (sequential between them); T026 depends on T022+T024; T027 depends on T023+T025+T014;
  T028 can run any time after T027.
- User Story 2: T029, T030 in parallel (different files); T031 makes T029 pass; T032 depends on
  T031; T033 is a manual verification step depending on T027+T031+T032.
- User Story 3: T034, T035 in parallel (different files); T036 depends on T024 (US1) and makes T034
  pass; T037 depends on T031 (US2) and T036; T038 depends on T037 and makes T035 pass; T039 depends
  on T031+T036.
- Polish: T040, T041, T042 in parallel; T043 last (depends on everything).

---

## Parallel Example: User Story 1

```bash
# Tests first, all in different files:
Task: "Unit tests for SipRequest parsing, UAS response builders, dialog-state extraction in gsm-sip-bridge/src/ims/sip_client.rs"
Task: "Unit tests for sdp::parse_offer / sdp::build_answer in gsm-sip-bridge/src/ims/sdp.rs"
Task: "Unit tests for the Bridged Call state machine in gsm-sip-bridge/src/vowifi/mod.rs"

# Then, three independent implementation tracks:
Task: "Implement SipRequest/try_parse, dialog helpers, UAS response builders in gsm-sip-bridge/src/ims/sip_client.rs"
Task: "Implement sdp::parse_offer and sdp::build_answer in gsm-sip-bridge/src/ims/sdp.rs"
Task: "Refactor register_session to optionally stay alive in gsm-sip-bridge/src/ims/mod.rs"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Complete Phase 1: Setup
2. Complete Phase 2: Foundational (blocks everything)
3. Complete Phase 3: User Story 1
4. **STOP and VALIDATE**: run `quickstart.md` steps 1, 3, 6 against real hardware
5. This alone delivers the feature's entire stated purpose ("receive the gsm call over vowifi and
   bridge it to the sip side with two way bridging") — Stories 2 and 3 harden and instrument it.

### Incremental Delivery

1. Setup + Foundational → foundation ready, nothing user-visible yet.
2. Add User Story 1 → validate independently → this is the deployable MVP.
3. Add User Story 2 → validate independently → the line survives unattended over time.
4. Add User Story 3 → validate independently → operator has visibility without extra tooling.
5. Polish.

### Notes

- Every implementation task names its exact target file(s); no task should require guessing where
  code belongs.
- Commit after each task or logical group, per Constitution Principle III (Frequent Atomic
  Commits) — run `cargo fmt --all && make lint && cargo test --workspace` before every commit, per
  `CLAUDE.md`'s pre-commit checklist, unchanged by this feature.
- Live-hardware verification tasks (T033, T043, and the checkpoint validations) are explicitly
  outside `cargo test --workspace`'s scope, consistent with how the existing `ims-register`/
  `ims-call` tools were validated against real carriers.
