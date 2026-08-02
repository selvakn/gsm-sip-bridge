---

description: "Task list for 025-outbound-calling"
---

# Tasks: Outbound Calling

**Input**: Design documents from `/specs/025-outbound-calling/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/

**Tests**: INCLUDED. Constitution Principle I (Integration-First Testing) is
NON-NEGOTIABLE and the Development Workflow section makes TDD the default.
Every test in this feature runs against real components over real sockets —
the new daemon↔agent channel is tested with two real processes/real Unix
sockets, never a mock, exactly as `test_sip_server_registrar.rs` (spec 024)
tests the registrar over a real `UdpSocket`.

**Organization**: Grouped by user story, in spec.md priority order (US1/US2
are both P1; US3/US4 are P2; US5 is P3).

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to (US1–US5)
- Exact file paths are given in every task

## Path Conventions

Rust workspace. Crate sources at `gsm-sip-bridge/src/`; integration tests at
`gsm-sip-bridge/tests/`. Unit tests live inline in a `#[cfg(test)] mod tests`
at the bottom of the file they cover.

## Pre-commit gate (applies to EVERY commit)

`make format && make lint && make test` — all three, no exceptions
(`CLAUDE.md`). `make lint` includes `tools/count-unsafe.sh`, which fails on
any `unsafe` in `gsm-sip-bridge/src` — the new `control::line_server`/
`line_client` and `sip::outbound` modules must be safe Rust, as the existing
registrar is.

---

## Phase 1: Setup

**Purpose**: No new project/crate is created (plan.md Structure Decision) —
this feature extends existing modules. Setup is limited to the new file
skeletons the rest of the plan writes into.

- [X] T001 [P] Create empty `gsm-sip-bridge/src/sip/outbound.rs` with a module doc comment summarizing its role (data-model.md `OutboundCallRequest`/`CandidateLine`) and add `pub mod outbound;` to `gsm-sip-bridge/src/sip/mod.rs`
- [X] T002 [P] Create empty `gsm-sip-bridge/src/control/line_server.rs` and `gsm-sip-bridge/src/control/line_client.rs` with module doc comments referencing `contracts/line-command.md`, and register both in `gsm-sip-bridge/src/control/mod.rs`

**Checkpoint**: New modules exist and compile empty; nothing wired up yet.

---

## Phase 2: Foundational — config, protocol types, metrics

**Purpose**: BLOCKING. Every user story reads `[outbound].enabled` and needs
the `PlaceCall`/`PlaceCallOutcome` types and outcome metrics to exist first.

- [X] T003 Add `RawOutbound` via the `section!` macro in `gsm-sip-bridge/src/config/raw.rs` with the single `enabled: bool` field per `contracts/config-schema.md`, and register `("outbound", RawOutbound::KEYS)` in `section_key_lists()`
- [X] T004 Add `OutboundConfig` runtime struct and the `outbound` field on `AppConfig` in `gsm-sip-bridge/src/config/mod.rs`
- [X] T005 Implement `build_outbound` in `gsm-sip-bridge/src/config/build.rs` — structural pass-through only; `contracts/config-schema.md` was revised during implementation to drop the originally-planned "at least one carrier path configured" rule, since CS modem presence is runtime-discovered, not config-declared, and so isn't checkable at build time
- [X] T006 [P] Add inline tests to `gsm-sip-bridge/src/config/mod.rs` covering the default (`enabled = false`, byte-for-byte unaffected) and rule 1's rejection, driven through the real `load_config` pipeline
- [X] T007 [P] Document `### \`[outbound]\`` in `docs/configuration.md` with a table row for the one key, so `tests/test_config_docs.rs` passes
- [X] T008 [P] Add a **commented-out** `[outbound]` block to `config.toml.example`, matching the `test_the_shipped_example_config_still_loads` convention already used for `[sip_server]`
- [X] T009 Add `PlaceCall { destination: String }` and `PlaceCallOutcome { Placed, Busy, Failed { reason: String } }` to `gsm-sip-bridge/src/control/protocol.rs` per `contracts/line-command.md`, with `Serialize`/`Deserialize` matching the existing `ControlCmd` framing style
- [X] T010 Define `OutboundCallRequest`, `Origin`, `CandidateLine`, and the outcome-category enum from `data-model.md` in `gsm-sip-bridge/src/sip/outbound.rs`
- [X] T011 [P] Add `gsm_sip_bridge_outbound_attempts_total` (labelled by `outcome`, per data-model.md's category table) to `gsm-sip-bridge/src/metrics/mod.rs`, following the existing `SIP_SERVER_*` counter registration pattern
- [X] T012 [P] Document the new metric in `docs/observability.md`

**Checkpoint**: config parses/validates/documents; the wire types and metric
exist but nothing produces or consumes them yet.

---

## Phase 3: User Story 1 — Place an outbound call from the PBX (Priority: P1) 🎯 MVP

**Goal**: A PBX-sent INVITE dials a number out over an idle circuit-switched
SIM (same-process case — the daemon's own `CardPool` already tracks every
modem's busy/idle state, so this story needs no cross-process channel).

**Independent Test**: Configure `[outbound].enabled = true` with at least one
CS modem, send an INVITE from a PBX naming a real number, confirm the modem
dials it (`ATD`) and two-way audio flows once answered.

### Tests for User Story 1

- [X] T013 [P] [US1] Inline test in `gsm-sip-bridge/src/modules/at_commander.rs` — DONE, adjusted scope: `AtResponse`/`read_response` only ever recognizes `OK`/`ERROR`/`+CME ERROR` as terminal (verified by reading the parser); `NO CARRIER`/`BUSY` arrive later as unsolicited result codes, the same way `RING` does, not as `ATD`'s own response. Tests cover `OK`→success, `ERROR`/`CME ERROR`→failure only; final call disposition is out of scope for this task and belongs to T019's progress relay, driven off the existing URC loop in `modules::mod`, not `AtCommander::dial` itself
- [X] T014 [P] [US1] Inline test in `gsm-sip-bridge/src/sip/outbound.rs` for destination validation (FR-014) — DONE as part of T010's entity module
- [ ] T015 [US1] Create `gsm-sip-bridge/tests/test_outbound_pbx_call.rs`: a real loopback SIP INVITE from a fake "PBX" `UdpSocket`/pjsua peer to a daemon configured with `[outbound].enabled = true` and one (simulated) idle CS line, asserting the destination reaches `AtCommander::dial` unmodified and a `180`/`200` progression is returned

### Implementation for User Story 1

- [X] T016 [US1] Add `AtCommander::dial(&mut self, number: &str) -> BridgeResult<()>` in `gsm-sip-bridge/src/modules/at_commander.rs`, sending `ATD{number};` alongside the existing `answer_call`/`hangup`
> **BLOCKED, discovered during implementation**: T018 (and T035 in US3) assumed
> pjsua-safe can accept an incoming INVITE (UAS). It cannot — `pjsua-safe/src`
> registers `on_call_state` only; there is no `on_incoming_call` callback and
> no `Call::answer`/accept API anywhere in `pjsua-safe`/`pjsua-sys`. Every
> existing call in this codebase is UAC-only (`Call::make`); the bridge has
> never received an INVITE via pjsua. Adding this means new `unsafe` FFI
> bindings (a callback registered in `AccountConfig`/`EndpointConfig`, a
> `pjsua_call_answer` wrapper) — real, but non-trivial and non-negotiably
> `unsafe`-audited work (`count-unsafe.sh`, currently 1.68% of `pjsua-safe`)
> that also cannot be exercised by `make test`/CI at all (the `pjsip-linked`
> feature they'd need is only built by `docker/Dockerfile`, per plan.md's own
> Constraints). This needs its own focused pass — including real testing in
> the privileged container — rather than being written blind here. T017,
> T019–T022 below are written to be ready to wire up once that lands.

- [ ] T017 [US1] **Also blocked, discovered during implementation**: each CS modem runs its own dedicated OS thread owning its `CardInstance`/`AtCommander` (`modules::mod`'s per-module loop); `CardPool` (the tokio-side orchestrator) only sees derived `SlotState` via the one-directional `BridgeEvent` channel *from* that thread. There is no existing command channel *into* a running modem thread — the same class of gap as US2's cross-process one, just intra-process. `sip::outbound::{CandidateLine, select_idle_line}` (T010) are ready to consume whatever read model this produces, but building the modem-thread command channel itself is unstarted. Implement same-process line selection over `CardPool` in `gsm-sip-bridge/src/sip/outbound.rs`: iterate configured CS modems, claim the first `idle()` one per data-model.md's provisional-claim rule (FR-004/005/007)
- [ ] T018 [US1] **BLOCKED on the pjsua-safe UAS gap above.** Implement the PBX-trunk UAS INVITE handler in `gsm-sip-bridge/src/sip/mod.rs`: on an incoming INVITE to the existing trunk `Account` with `[outbound].enabled = true`, construct an `OutboundCallRequest { origin: Origin::Pbx, .. }`, validate (T014), select a line (T017), and dial (T016)
- [ ] T019 [US1] Implement call-progress relay in `gsm-sip-bridge/src/sip/mod.rs` per `contracts/sip-dialout.md`'s table (`180 Ringing`, `486 Busy Here`, `503 Service Unavailable`, `200 OK`) driven off the CS leg's AT-reported call state
- [ ] T020 [US1] Wire teardown: either leg hanging up ends the other, reusing the existing bridged-call teardown path (`sip::mod`'s current hangup handling) rather than a new implementation (FR-013)
- [ ] T021 [US1] Increment `gsm_sip_bridge_outbound_attempts_total` with the right outcome label at every terminal point reached by T018–T020
- [ ] T022 [US1] Confirm `make test` green with `[outbound].enabled = false` (default) leaving `sip::mod`'s INVITE handling byte-for-byte as before (FR-017) — add the negative-path inline test if not already covered by T015

**Checkpoint**: A PBX can dial out over a single-process CS deployment. This
is the MVP — independently demoable without US2–US5.

---

## Phase 4: User Story 2 — Dial out on whichever SIM is free, across processes (Priority: P1)

**Goal**: Line selection generalizes beyond the SIP-owning process's own
modems to VoWiFi/VoLTE lines running as separate agent processes, using the
new synchronous command channel (research.md R-003).

**Independent Test**: With a CS modem busy and a VoWiFi line idle (or vice
versa, in separate processes), an outbound request lands on the idle one;
two concurrent requests with only one idle line total result in exactly one
success.

### Tests for User Story 2

- [ ] T023 [P] [US2] Create `gsm-sip-bridge/tests/test_outbound_line_command.rs`: two real processes (or two real Unix-socket endpoints in-process, mirroring `test_sip_server_registrar.rs`'s realism) exchanging `PlaceCall`/`PlaceCallOutcome` per `contracts/line-command.md`, covering `Placed`, `Busy`, `Failed`, and the request timeout
- [ ] T024 [P] [US2] Inline test in `gsm-sip-bridge/src/sip/outbound.rs` for the contention rule (FR-008): two selections racing for the same last-idle `CandidateLine` — the second sees it already claimed and is refused identically to "no line idle" (FR-009)
- [ ] T025 [US2] Extend `test_outbound_pbx_call.rs` (T015) with a case where the CS modem is busy and a second (simulated cross-process) line is idle, asserting the call is placed on the idle one, not refused

### Implementation for User Story 2

- [ ] T026 [US2] Implement `control::line_server`: a per-line Unix socket listener (path derived from line identity, per `contracts/line-command.md`) that accepts `PlaceCall`, dispatches to the local dial function (`AtCommander::dial` for CS, or the VoWiFi/VoLTE origination point added in US4), and replies with `PlaceCallOutcome`
- [ ] T027 [US2] Implement `control::line_client::place_call(socket_path, destination, timeout) -> PlaceCallOutcome`, applying the sub-Timer-B timeout from `contracts/line-command.md` and mapping a timed-out/unreachable socket to `Failed`
- [ ] T028 [US2] Start a `line_server` listener per line at daemon/agent startup, gated on `[outbound].enabled`, in whichever module already owns that line's lifecycle (the daemon's `CardPool` init for CS, `ims::agent`/`vowifi::mod` startup for VoWiFi/VoLTE)
- [ ] T029 [US2] Extend `sip::outbound`'s line selection (T017) to build its `CandidateLine` list from every configured line regardless of hosting process, using the existing `AgentReport` liveness/state stream (data-model.md) for the cross-process ones, and route the actual dial through `line_client::place_call` (T027) instead of a direct in-process call when the target line is remote
- [ ] T030 [US2] Implement the local-claim-then-command sequence in `gsm-sip-bridge/src/sip/outbound.rs` (data-model.md's race handling): mark a `CandidateLine` provisionally non-idle before issuing `PlaceCall`, and unconditionally let the next `AgentReport` correct it regardless of outcome
- [ ] T031 [US2] No automatic retry on `Busy`/`Failed` (FR-008/FR-009a) — confirm `sip::outbound` returns the whole request as refused rather than trying a second `CandidateLine`; extend T024 if this isn't already pinned there

**Checkpoint**: Outbound calling works across every configured line,
regardless of which process hosts it, with correct contention handling.

---

## Phase 5: User Story 3 — Dial out from a phone in SIP server mode (Priority: P2)

**Goal**: A phone registered directly to the bridge's own SIP server mode
(spec 024) can also originate an outbound call, reversing that spec's FR-020.

**Independent Test**: With `[sip_server].enabled` and `[outbound].enabled`
both true and a phone registered (not necessarily `ring_aor`), dialing a
number from the phone places it on an idle SIM.

### Tests for User Story 3

- [ ] T032 [P] [US3] Extend `gsm-sip-bridge/tests/test_sip_server_registrar.rs`: a registered phone's INVITE now receives `302 Moved Temporarily` with the `Contact` from `contracts/sip-dialout.md` when `[outbound].enabled = true`, and still `403 Forbidden` when it is `false` (FR-017) or the phone has no live binding
- [ ] T033 [US3] Create `gsm-sip-bridge/tests/test_outbound_sip_server_phone.rs`: a real phone-side `UdpSocket` registers, dials, follows the 302 to `Account::local`'s port, and the resulting call reaches `sip::outbound` with `Origin::SipServerPhone`

### Implementation for User Story 3

- [ ] T034 [US3] Change the `"INVITE"` branch in `gsm-sip-bridge/src/sip/server/mod.rs::handle_datagram` from unconditional `403 Forbidden` to: `403` if the peer has no live binding, else (when `[outbound].enabled`) `302 Moved Temporarily` with `Contact: sip:{aor}@{listen_addr}:{sip.local_port}` per `contracts/sip-dialout.md` — else the existing `403` (FR-017)
- [ ] T035 [US3] Implement the UAS INVITE handler on `Account::local` in `gsm-sip-bridge/src/sip/mod.rs`, constructing `OutboundCallRequest { origin: Origin::SipServerPhone { aor }, .. }` and reusing the same validate → select → dial pipeline as US1/US2 (T014/T017/T029) — no new pipeline
- [ ] T036 [US3] Confirm eligibility is any currently-registered account, not only `ring_aor` (spec.md FR-003) — add the case explicitly if T032/T033 don't already cover a non-`ring_aor` account

**Checkpoint**: Both SIP-side topologies (PBX, SIP server mode) can
originate outbound calls through the same underlying pipeline.

---

## Phase 6: User Story 4 — Same behavior on every carrier path (Priority: P2)

**Goal**: VoWiFi and VoLTE SIMs (including PC/SC-sourced VoWiFi lines) can
be selected and dialed exactly like a CS modem.

**Independent Test**: With outbound calling enabled on a VoWiFi- or
VoLTE-only deployment, an outbound request is placed and carries audio with
no visible difference from the CS case.

### Tests for User Story 4

- [ ] T037 [P] [US4] Inline test in `gsm-sip-bridge/src/ims/agent.rs` asserting the new outbound-origination path builds an INVITE whose Request-URI user part is the given destination verbatim (FR-010), reusing the existing SDP-offer construction used for the inbound direction
- [ ] T038 [US4] Extend `test_outbound_line_command.rs` (T023) or add `test_outbound_vowifi_call.rs`: a `PlaceCall` routed to a simulated VoWiFi/VoLTE agent results in an originated IMS INVITE, not an `ATD`
- [ ] T039 [P] [US4] Confirm (test or explicit assertion in T038) that a PC/SC-sourced VoWiFi line is selected and dialed identically to a modem-sourced one — per research.md, this requires no new code path since PC/SC only changes SIM sourcing, not call origination, but the parity must be demonstrated, not assumed

### Implementation for User Story 4

- [ ] T040 [US4] Add the outbound-origination function to `gsm-sip-bridge/src/ims/agent.rs`: send an INVITE toward the P-CSCF with the destination as Request-URI user part, reusing the existing inbound call's SDP/media bridging code for everything after the initial INVITE (research.md R-005)
- [ ] T041 [US4] Wire this function as the VoWiFi/VoLTE line's dial target for `control::line_server` (T026), so `PlaceCall` on a VoWiFi/VoLTE line reaches T040 instead of `AtCommander::dial`
- [ ] T042 [US4] Verify `sip::outbound`'s line selection (T029) treats `CarrierPath::VoWifi`/`Volte` candidates with no preference relative to `CircuitSwitched` (FR-007) — extend T024 if not already covered

**Checkpoint**: Outbound calling is fully path-agnostic.

---

## Phase 7: User Story 5 — Diagnose a failed or refused outbound call (Priority: P3)

**Goal**: An operator can distinguish "no SIM was idle," "network refused,"
and "unanswered" from logs and metrics alone.

**Independent Test**: Trigger each of the three outcomes and confirm they
are distinguishable without a packet capture.

- [ ] T043 [P] [US5] Add a log line at each refusal/failure point (T018, T029, T035, T040) naming the reason and, for `refused_no_idle_line`, that no `CandidateLine` was idle at the time (FR-016)
- [ ] T044 [P] [US5] Confirm `gsm_sip_bridge_outbound_attempts_total{outcome=...}` (T011/T021) is incremented at every terminal point across US1–US4 with the correct label from data-model.md's outcome table — add any missing increments
- [ ] T045 [US5] Add an inline or integration test asserting the three outcomes in the Independent Test above produce distinct log fields and distinct metric label values

**Checkpoint**: All five user stories are independently functional and
diagnosable.

---

## Phase 8: Polish & Cross-Cutting Concerns

- [ ] T046 [P] Update `README.md`'s Highlights with an outbound-calling bullet, mirroring the existing SIP-server-mode bullet's style
- [ ] T047 [P] Add an "Outbound calling" section to `docs/architecture.md` describing the three new mechanisms (dial-out leg, inbound-leg acceptance, cross-process line command) and linking `contracts/line-command.md` and `contracts/sip-dialout.md`
- [ ] T048 [P] Add an outbound-calling entry to `RELEASE_NOTES.md` following the project's dense-narrative style (see the `v8.3.0` SIP-server-mode entry for the expected shape)
- [ ] T049 Run `specs/025-outbound-calling/quickstart.md` end to end against a real container (per the project's `run` skill / docker compose), confirming every step and the "If nothing happens" diagnostics actually match observed behavior
- [ ] T050 Mark `docs/todo.md`'s outbound-calling item (if present) complete, referencing this feature directory

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: no dependencies
- **Foundational (Phase 2)**: depends on Setup — BLOCKS all user stories
- **US1 (Phase 3)**: depends on Foundational only — the MVP
- **US2 (Phase 4)**: depends on Foundational; builds directly on US1's
  `sip::outbound` pipeline (T017) but is independently testable once US1
  exists (a single-line deployment stays on US1's fast path)
- **US3 (Phase 5)**: depends on Foundational and reuses US1/US2's pipeline
  (T014/T017/T029) — cannot be tested meaningfully before US1 exists, but
  adds no new pipeline of its own
- **US4 (Phase 6)**: depends on Foundational; reuses `control::line_server`
  from US2 (T026) — a VoWiFi/VoLTE-only deployment needs US2's cross-process
  channel even to exercise US1's CS-only fast path is absent, so US4
  effectively depends on US2 being present, not just US1
- **US5 (Phase 7)**: depends on US1–US4 existing to have outcomes to
  diagnose; purely additive (logging/metrics), touches no new call-placement
  logic
- **Polish (Phase 8)**: depends on all desired user stories being complete

### Within Each User Story

- Tests before implementation (constitution TDD default)
- `sip::outbound` entity/validation before line selection before dial
  before SIP-side wiring before progress/teardown
- Story complete before moving to the next priority

### Parallel Opportunities

- T001/T002 (Setup) in parallel
- T006/T007/T008 and T011/T012 (Foundational) in parallel — different files
- T013/T014 (US1 tests) in parallel
- T023/T024 (US2 tests) in parallel
- T037/T039 (US4 tests) in parallel
- T046/T047/T048 (Polish) in parallel

---

## Parallel Example: User Story 1

```bash
# Tests first, in parallel (different files):
Task: "Inline test for AtCommander::dial in gsm-sip-bridge/src/modules/at_commander.rs"
Task: "Inline test for destination validation in gsm-sip-bridge/src/sip/outbound.rs"

# Then sequentially, since each depends on the previous:
Task: "Implement AtCommander::dial"
Task: "Implement same-process line selection in sip::outbound"
Task: "Implement the PBX-trunk UAS INVITE handler in sip::mod"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Setup + Foundational
2. User Story 1 — PBX-originated outbound calling on a single-process CS
   deployment
3. **STOP and VALIDATE**: run `quickstart.md`'s steps 1–3 against a
   single-modem deployment
4. Deploy/demo if ready — this alone delivers the feature's stated core
   value for the most common (single-SIM, PBX-backed) deployment shape

### Incremental Delivery

1. Setup + Foundational → foundation ready
2. US1 → validate → MVP demoable
3. US2 → validate → multi-SIM and cross-process deployments covered
4. US3 → validate → PBX-free (SIP server mode) deployments covered
5. US4 → validate → VoWiFi/VoLTE/PC-SC deployments covered
6. US5 → validate → day-two diagnosability covered
7. Polish

### Notes

- [P] tasks = different files, no dependencies
- Verify tests fail before implementing (TDD, constitution Development
  Workflow)
- Commit after each task or logical group (constitution Principle III)
- `make format && make lint && make test` before every commit, no
  exceptions (`CLAUDE.md`)
