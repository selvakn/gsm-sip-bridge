---

description: "Task list for 024-sip-server-mode"
---

# Tasks: SIP Server Mode

**Input**: Design documents from `/specs/024-sip-server-mode/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/

**Tests**: INCLUDED. Constitution Principle I (Integration-First Testing) is
NON-NEGOTIABLE and the Development Workflow section makes TDD the default, so
test tasks are first-class here, not optional. Every test in this feature runs
against real components over real sockets — **no mocks are introduced anywhere**.

**Organization**: Grouped by user story. Each phase maps to one or more commits
from plan.md's Commit Sequence; the mapping is stated per phase.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to (US1–US4)
- Exact file paths are given in every task

## Path Conventions

Rust workspace. Crate sources at `gsm-sip-bridge/src/` and `pjsua-safe/src/`;
integration tests at `gsm-sip-bridge/tests/`. Unit tests live inline in a
`#[cfg(test)] mod tests` at the bottom of the file they cover.

## Pre-commit gate (applies to EVERY commit)

`make format && make lint && make test` — all three, no exceptions
(`CLAUDE.md`). `make lint` includes `tools/count-unsafe.sh`, which fails on any
`unsafe` in `gsm-sip-bridge/src`.

---

## Phase 1: Setup — make existing SIP machinery reachable

**Purpose**: The registrar reuses the hand-rolled SIP parser, response builder
and digest math already in `crate::ims`. Both modules are private today, so
nothing outside `ims` can call them. Without this, the alternative is a second
SIP parser and a second digest implementation in the same binary.

**Commits**: 2 (`refactor(ims): make sip_client and digest reachable crate-wide`),
3 (`feat(ims): let a UAS response carry extra headers`)

- [X] T001 Widen `mod digest;` to `pub(crate) mod digest;` in `gsm-sip-bridge/src/ims/mod.rs`
- [X] T002 Widen `mod sip_client;` to `pub(crate) mod sip_client;` in `gsm-sip-bridge/src/ims/mod.rs`
- [X] T003 Widen `SipRequest::try_parse` from `pub(super)` to `pub(crate)` in `gsm-sip-bridge/src/ims/sip_client.rs`
- [X] T004 Confirm `make test` is green with T001–T003 applied — these are mechanical visibility changes with zero behavioural effect; commit 2 here
- [X] T005 Add `build_uas_response_with_headers(status, reason, request, to_tag, contact, body, extra: &[(&str, &str)])` in `gsm-sip-bridge/src/ims/sip_client.rs`, emitting each `extra` header after the copied ones
- [X] T006 Reimplement the existing `build_uas_response` in `gsm-sip-bridge/src/ims/sip_client.rs` as a delegation to T005 with an empty slice, so the IMS path's behaviour is provably unchanged
- [X] T007 Add an inline test in `gsm-sip-bridge/src/ims/sip_client.rs` asserting that extra headers appear in the response and that a call with no extras is byte-identical to the previous output; commit 3 here

**Checkpoint**: `crate::sip` can now parse SIP requests, build UAS responses with
arbitrary headers, and compute digests.

---

## Phase 2: Foundational — the `[sip_server]` config section

**Purpose**: BLOCKING. Every later phase reads this config. Also the commit that
`tests/test_config_docs.rs` gates: the docs and example must land with it or
`make test` fails.

**Commit**: 4 (`feat(config): add the opt-in [sip_server] section`)

- [X] T008 Add `RawSipServer` and `RawSipServerAccount` via the `section!` macro in `gsm-sip-bridge/src/config/raw.rs`, with the fields and defaults in `contracts/config-schema.md`; name the vector field `account` so it *is* the TOML key
- [X] T009 Register the section in `gsm-sip-bridge/src/config/raw.rs`: add `sip_server` to `RawConfig`, and add both `("sip_server", RawSipServer::KEYS)` and `("sip_server.account", RawSipServerAccount::KEYS)` to `section_key_lists()`
- [X] T010 Add `SipServerConfig` and `SipServerAccount` runtime structs to `gsm-sip-bridge/src/config/mod.rs` and the `sip_server` field to `AppConfig`
- [X] T011 Add `"sip_server.account.password"` to `SECRET_KEY_PATHS` in `gsm-sip-bridge/src/config/env.rs`
- [X] T012 Implement `build_sip_server` in `gsm-sip-bridge/src/config/build.rs` with validation rules 1–12 from `contracts/config-schema.md`, reusing the existing `in_range`, `require_non_empty` helpers and the `account[{i}]:` error-prefix convention
- [X] T013 Restructure `build()` in `gsm-sip-bridge/src/config/build.rs` to build `sip_server` first and pass `&SipServerConfig` into `build_sip` and `build_bridge`, following the existing `build_alerts(raw.alerts, &sms)` precedent
- [X] T014 Implement cross-section rules 13–18 from `contracts/config-schema.md` in `gsm-sip-bridge/src/config/build.rs`, including relaxing the `[sip]` server/username/password requirement when the mode is enabled, and the port-collision message that names the remedy
- [X] T015 [P] Add inline tests to `gsm-sip-bridge/src/config/mod.rs` covering every rule in T012 and T014 — one test per rule, driven through the real `load_config` pipeline as the existing tests there do
- [X] T016 [P] Document `### \`[sip_server]\`` in `docs/configuration.md` with a table row for every key in both `KEYS` lists plus a backticked `` `[[sip_server.account]]` `` mention, so all four `tests/test_config_docs.rs` checks pass
- [X] T017 [P] Add a **commented-out** `[sip_server]` block and `# [[sip_server.account]]` example to `config.toml.example`; it must stay commented so `tests/test_config.rs::test_the_shipped_example_config_still_loads` keeps passing

**Checkpoint**: config parses, validates, and is documented. No runtime behaviour
has changed yet. Commit 4.

---

## Phase 3: User Story 1 — Ring a desk phone with no PBX (Priority: P1) 🎯 MVP

**Goal**: An operator with no PBX enables the mode, points one IP phone at the
bridge, and an incoming mobile call rings it with two-way audio.

**Independent test**: Configure the mode with one account, register a UA, place
a call to the SIM, confirm the phone rings and carries audio — with no PBX
present or configured.

**Commits**: 5, 6, 7, 8, 9, 10

### Binding store

- [X] T018 [US1] Create `gsm-sip-bridge/src/sip/server/bindings.rs` with `Binding` and `BindingStore` per `data-model.md` §2, taking `now: Instant` as a parameter on every expiry-sensitive method
- [X] T019 [US1] Write the justification comment at `BindingStore` explaining why one binding per AOR is stored rather than RFC 3261's contact set (Constitution Principle V; research.md R-005)
- [X] T020 [US1] Implement `upsert`, `remove`, `get_live`, `sweep`, `live_count` in `gsm-sip-bridge/src/sip/server/bindings.rs`, using the poison-tolerant `.lock().unwrap_or_else(|e| e.into_inner())` idiom
- [X] T021 [US1] Add inline tests to `gsm-sip-bridge/src/sip/server/bindings.rs` for lazy expiry, upsert-replaces-not-appends, remove, and sweep/live counts — all at simulated times, no `sleep`; commit 5 here

### Digest authentication

- [X] T022 [US1] Create `gsm-sip-bridge/src/sip/server/auth.rs` with `NonceEntry` and `NonceStore` per `data-model.md` §2, capped at 256 entries with oldest-first eviction
- [X] T023 [US1] Implement challenge generation in `gsm-sip-bridge/src/sip/server/auth.rs` using `ims::sip_client::random_hex(16)`, emitting the `WWW-Authenticate` form in `contracts/sip-registrar.md` §1.1
- [X] T024 [US1] Implement response verification in `gsm-sip-bridge/src/sip/server/auth.rs`: parse `Authorization` with `ims::sip_client::parse_digest_challenge`, compute with `ims::digest::ha1`/`ha2` and either `response_qop` or `response_simple` depending on whether `qop`/`nc`/`cnonce` are present
- [X] T025 [US1] Implement the algorithm policy in `gsm-sip-bridge/src/sip/server/auth.rs`: accept absent or `MD5`, reject `MD5-sess` and `SHA-256`; compute HA2 from the client's own `uri` parameter verbatim, logging a DEBUG on mismatch with the Request-URI rather than rejecting
- [X] T026 [US1] Add inline tests to `gsm-sip-bridge/src/sip/server/auth.rs` for challenge formatting and for both response forms, computing expected values with `ims::digest` directly so a change on either side breaks the test; commit 6 here

### Registrar serve loop

- [X] T027 [US1] Create `gsm-sip-bridge/src/sip/server/mod.rs` with a `Registrar` that binds a `UdpSocket` on `listen_addr:listen_port`, sets a 500 ms read timeout, and runs its loop on a `std::thread` — the shape used by `ims::agent::spawn_veth_uas_listener`
- [X] T028 [US1] Implement the shutdown path in `gsm-sip-bridge/src/sip/server/mod.rs`: an `Arc<AtomicBool>` checked each iteration, plus a handle whose join the host can await
- [X] T029 [US1] Implement the idle tick in `gsm-sip-bridge/src/sip/server/mod.rs` — on `WouldBlock`/`TimedOut`, sweep expired bindings and nonces
- [X] T030 [US1] Implement REGISTER handling in `gsm-sip-bridge/src/sip/server/mod.rs` for the accept path: challenge when unauthenticated (§1.1), verify, store the binding, and answer `200 OK` echoing `Contact` with the granted `;expires=` (§1.2)
- [X] T031 [US1] Implement the non-REGISTER method dispatch in `gsm-sip-bridge/src/sip/server/mod.rs` per `contracts/sip-registrar.md` §2 — `OPTIONS`, `INVITE`, `SUBSCRIBE` and the `405` default — building every response through `build_uas_response_with_headers`
- [X] T032 [US1] Log the `Contact`-versus-source-address mismatch WARN in `gsm-sip-bridge/src/sip/server/mod.rs`, naming both, without rewriting the stored `Contact`
- [X] T033 [US1] Create `gsm-sip-bridge/tests/test_sip_server_registrar.rs` with the real-socket harness: registrar on `127.0.0.1:0`, a second `UdpSocket` as the phone with a read timeout
- [X] T034 [US1] Add tests to `gsm-sip-bridge/tests/test_sip_server_registrar.rs` for §1.1 (challenge), §1.2 (both digest forms accepted), §2 (`OPTIONS` 200, `INVITE` 403, `SUBSCRIBE` 489, `405` default) and §3 (multi-`Via` echoed in order); commit 7 here

### Destination selection

- [X] T035 [US1] Create `gsm-sip-bridge/src/sip/target.rs` with the `CallTarget` enum and `uri_for(caller_did, now) -> Result<String, String>` per `data-model.md` §2, preserving today's DID-passthrough rule exactly in the `Pbx` variant
- [X] T036 [US1] Add inline tests to `gsm-sip-bridge/src/sip/target.rs` pinning the existing `Pbx` behaviour (empty vs fixed `sip_destination`, leading `+` stripped) as a regression guard, plus both `RegisteredPhone` outcomes
- [X] T037 [US1] Replace the body of `SipBridge::compute_destination_uri` in `gsm-sip-bridge/src/sip/mod.rs` with a delegation to `CallTarget::Pbx`, keeping the signature infallible for now
- [X] T038 [US1] Replace the body of `pbx_dest_uri` in `gsm-sip-bridge/src/vowifi/mod.rs` with a delegation to `CallTarget::Pbx`, and verify `make test` shows no behavioural change; commit 8 here

### Non-registering pjsua account

- [X] T039 [US1] Add `Account::local(endpoint, id_uri, display_name)` to `pjsua-safe/src/account.rs` as a branch inside the **existing** `unsafe` block — `reg_uri` zeroed, `cred_count = 0` — introducing no new `unsafe` block
- [X] T040 [US1] Add the `#[cfg(not(feature = "pjsip-linked"))]` stub for `Account::local` in `pjsua-safe/src/account.rs`, returning `registered: false`, and comment why `Drop` must not unregister an account that never registered; commit 9 here

### Wiring the circuit-switched path

- [X] T041 [US1] Add `server_mode: bool` to `SipBridgeConfig` in `gsm-sip-bridge/src/sip/mod.rs` and extend `register_trunk` to `!volte.bridge_inbound && !vowifi.enabled && !sip_server.enabled`, updating the existing doc comment to name the third claimant
- [X] T042 [US1] Add the server-mode branch to `SipBridge::register` in `gsm-sip-bridge/src/sip/mod.rs`, placed **before** the `!register_trunk` early return: create the `Endpoint`, build the `BindingStore`, spawn the `Registrar`, then `Account::local` with `sip:{ring_aor}@{listen_addr}:{listen_port}`
- [X] T043 [US1] Change `SipBridge::compute_destination_uri` in `gsm-sip-bridge/src/sip/mod.rs` to return `Result<String, String>`, selecting `CallTarget::RegisteredPhone` in server mode and `CallTarget::Pbx` otherwise
- [X] T044 [US1] Extend `SipBridge::unregister` in `gsm-sip-bridge/src/sip/mod.rs` to signal the registrar's stop flag and join its thread before dropping the endpoint
- [X] T045 [US1] Handle the new `Err` at the `BridgeEvent::Ring` call site in `gsm-sip-bridge/src/modules/mod.rs`, using the same early-return shape as the existing registration guard: WARN naming `ring_aor`, count it, and leave the GSM call to ring out (FR-018)
- [X] T046 [US1] Extend `gsm-sip-bridge/tests/test_sip_registration.rs` with server-mode cases through the real `load_config` fixture: `register()` reaches `Registered` with no PBX; `compute_destination_uri` is `Err` with no binding and returns the registered `Contact` once a phone has registered through the real registrar over a real socket; commit 10 here

**Checkpoint**: 🎯 **MVP complete.** A phone can register and be rung on the
circuit-switched path with no PBX anywhere in the deployment.

---

## Phase 4: User Story 2 — Provision and re-provision phones safely (Priority: P2)

**Goal**: Only authorised phones are accepted, and handsets that come, go, and
move keep working with no operator action.

**Independent test**: Register with correct credentials (accepted), with a wrong
password (refused), unplug and re-plug the phone (calls resume), move it to a
different address (calls follow).

**Commits**: folded into 7 and 10 if implemented alongside Phase 3; otherwise its
own commit `feat(sip): registration lifecycle and refusal paths`

- [X] T047 [US2] Implement the refusal paths in `gsm-sip-bridge/src/sip/server/auth.rs`: wrong password and unknown username must produce **byte-identical** `401`s (FR-009), distinguished only by the value returned to the caller for metric labelling
- [X] T048 [US2] Implement staleness in `gsm-sip-bridge/src/sip/server/auth.rs`: a well-formed response against an unknown, expired or consumed nonce gets `401 stale=true`; a wrong response against a live nonce gets `401` without `stale`
- [X] T049 [US2] Implement replay rejection in `gsm-sip-bridge/src/sip/server/auth.rs`: strictly increasing `nc` per nonce under `qop=auth`, single-use nonce without `qop`
- [X] T050 [US2] Implement expiry negotiation in `gsm-sip-bridge/src/sip/server/mod.rs`: `423 Interval Too Brief` with `Min-Expires` below the floor (§1.8), clamp above the ceiling and report the **granted** value (§1.9)
- [X] T051 [US2] Implement de-registration in `gsm-sip-bridge/src/sip/server/mod.rs`: `Expires: 0` or `Contact: *` removes the binding and answers `200 OK` with no `Contact` (§1.10), still requiring valid credentials
- [X] T052 [US2] Implement the retransmission guard in `gsm-sip-bridge/src/sip/server/mod.rs`: same `Call-ID` with `CSeq <=` stored answers `200 OK` with the existing binding and does **not** extend it; comment the deliberate deviation from RFC 3261 §10.3 (§1.11)
- [X] T053 [US2] Implement `400 Bad Request` for malformed requests in `gsm-sip-bridge/src/sip/server/mod.rs` (§1.12)
- [X] T054 [US2] Add tests to `gsm-sip-bridge/tests/test_sip_server_registrar.rs` for §1.3 and §1.4 asserting the two `401`s are byte-identical, plus §1.5 stale, §1.6 replay, §1.7 unsupported algorithm
- [X] T055 [US2] Add tests to `gsm-sip-bridge/tests/test_sip_server_registrar.rs` for §1.8 `423`+`Min-Expires`, §1.9 clamping, §1.10 de-registration, §1.11 retransmission, §1.12 malformed
- [X] T056 [US2] Add a test to `gsm-sip-bridge/tests/test_sip_server_registrar.rs` covering re-registration from a different source address replacing the stored binding — the "phone moved" case behind SC-003

**Checkpoint**: registration lifecycle is complete and the mode is safe to expose
on a real network.

---

## Phase 5: User Story 3 — VoWiFi and VoLTE deployments (Priority: P2)

**Goal**: The same PBX-free operation when calls arrive over the carrier's
packet-switched path rather than the circuit-switched one.

**Independent test**: Enable the mode on a VoWiFi-configured deployment, register
a phone, place a call to the SIM — the phone rings and carries audio.

**Commit**: 11 (`feat(vowifi): host the registrar on the telephony side too`)

- [X] T057 [US3] Add the server-mode branch to `run_telephony_side` in `gsm-sip-bridge/src/vowifi/mod.rs`: skip the `Account::register` plus confirm-with-backoff block, use `Account::local`, and host a `Registrar` — this is the process that owns the trunk when VoWiFi or VoLTE inbound bridging is active (research.md R-003)
- [X] T058 [US3] Change `bridge_call` in `gsm-sip-bridge/src/vowifi/mod.rs` to take its PBX-leg URI from `CallTarget`, handling the no-live-binding `Err` by declining the carrier call rather than placing a leg to nowhere; the veth leg and `pair_calls` are untouched
- [X] T059 [US3] Ensure the registrar's stop flag is signalled on the telephony side's shutdown path in `gsm-sip-bridge/src/vowifi/mod.rs`, mirroring T044
- [X] T060 [US3] Add a test asserting that exactly one component claims the registrar for a given config — that `register_trunk` and the server-mode host decision cannot both be true — in `gsm-sip-bridge/tests/test_sip_registration.rs`

**Checkpoint**: all three inbound call paths ring a registered phone (FR-019).

---

## Phase 6: User Story 4 — Diagnose a deployment that is not ringing (Priority: P3)

**Goal**: An operator can distinguish "never registered", "refused", and
"registered then lapsed" from logs and metrics alone.

**Independent test**: Query metrics with no phone registered, with one
registered, and after a refused attempt; confirm the three states differ.

**Commit**: 12 (`feat(sip): observe the embedded registrar`)

- [X] T061 [P] [US4] Add the five metric series from `data-model.md` §4 to `gsm-sip-bridge/src/metrics/mod.rs`, beside the existing `SIP_REGISTERED`, with a comment explaining why `ring_aor_registered` is a separate gauge rather than folded into an aggregate
- [X] T062 [US4] Emit `bindings` and `ring_aor_registered` from the idle-tick sweep in `gsm-sip-bridge/src/sip/server/mod.rs`
- [X] T063 [US4] Emit `registrations_total{outcome}` for all seven outcomes and `requests_total{method,status}` for every dispatched request in `gsm-sip-bridge/src/sip/server/mod.rs`
- [X] T064 [US4] Emit `ring_target_missing_total` at the `Ring` call site in `gsm-sip-bridge/src/modules/mod.rs` alongside the WARN added in T045
- [X] T065 [P] [US4] Document the five series in `docs/observability.md`; first check `tests/test_metric_renames.rs` and `tests/test_migration_guide.rs` in case new series need registering there

**Checkpoint**: the failure states behind SC-005 are all observable.

---

## Phase 7: Polish & Cross-Cutting Concerns

**Commits**: 13 (`docs: …`), 14 (`chore(release): 8.3.0`)

- [X] T066 [P] Add a SIP-server-mode subsection to `docs/architecture.md` — it is an alternative SIP-side topology, not a fourth inbound call path, so it must not be added as a fourth item to the existing three-paths list
- [X] T067 [P] Update the mermaid diagram in `docs/architecture.md` and its duplicate in `README.md` with the variant where the bridge talks to the IP phone directly, with no PBX
- [X] T068 [P] Add the operations runbook to `docs/operations.md`: enabling the mode, the port pair, provisioning a handset, the "accept SIP only from proxy" caveat from research.md R-002, diagnosing `no live registration for AOR`, and the new metrics
- [X] T069 [P] Add the new doc references to the index tables in `docs/README.md`
- [X] T070 Bump the workspace version in `Cargo.toml` from `8.2.0` to `8.3.0` and add the `RELEASE_NOTES.md` entry — additive and opt-in, so a minor bump
- [X] T071 Run the end-to-end verification from `quickstart.md` in the Docker image with `pjsip-linked` enabled, since `Account::local` and the pjsua call path are the one surface `make test` cannot cover — **done, partially**: the registrar, `Account::local` against real PJSIP, and both endpoints binding simultaneously are all verified (see `quickstart.md`, "What the container run verified"), and the run found the wildcard-identity defect. The call leg itself (`Call::make` toward a registered Contact, media, teardown) still needs a SIM and remains open
- [X] T072 Run the regression check: an end-to-end call with `[sip_server].enabled = false` against the real PBX must behave exactly as before (FR-024, SC-006) — T037/T038 are the changes that make this necessary

---

## Dependencies & Execution Order

### Phase dependencies

```
Phase 1 (Setup)         ── blocks everything; nothing else compiles without it
    ▼
Phase 2 (Config)        ── blocks every user story; all of them read this config
    ▼
Phase 3 (US1, P1) 🎯 MVP ── the registrar, CallTarget, Account::local, CS wiring
    ▼
    ├─▶ Phase 4 (US2, P2)  lifecycle + refusal paths — extends Phase 3's modules
    ├─▶ Phase 5 (US3, P2)  telephony-side hosting — needs CallTarget + Registrar
    └─▶ Phase 6 (US4, P3)  metrics — needs call sites from Phases 3–5
                ▼
        Phase 7 (Polish)
```

Phases 4, 5 and 6 are independent of one another and may be done in any order
once Phase 3 lands.

### Story independence

- **US1** is the MVP and stands alone: it delivers a ringing phone.
- **US2** extends US1's modules rather than adding new ones. Testable on its own
  by exercising the refusal and lifecycle paths.
- **US3** touches only `vowifi/mod.rs` and reuses everything US1 built.
- **US4** is purely additive instrumentation.

### Parallel opportunities

- T015, T016, T017 — config tests, reference docs, example file: three files, no
  interdependency.
- T018–T021 (bindings) and T022–T026 (auth) are separate files and can be built
  concurrently; both must land before T030.
- T061 and T065 — metric definitions and their documentation.
- T066–T069 — four independent documentation files.

**Not parallelisable**: T037/T038 must follow T035; T042–T044 must follow T039;
T045 must follow T043.

---

## Implementation Strategy

**MVP scope**: Phases 1–3 (T001–T046). This delivers User Story 1 in full — a
small deployment taking a mobile call on an IP phone with no PBX — and satisfies
SC-001, SC-002 and SC-007.

**Incremental delivery**: ship Phase 3, then Phase 4 before exposing the mode on
any network that is not fully trusted, then Phase 5 for VoWiFi and VoLTE
deployments, then Phase 6.

**Task count**: 72 tasks — 7 setup, 10 foundational, 29 for US1, 10 for US2,
4 for US3, 5 for US4, 7 polish.

**Test posture**: every test runs against real components — real UDP sockets,
the real config pipeline, the real digest implementation. **No mocks are
introduced**, so Constitution Principle I's mock-justification requirement has
nothing to discharge in this feature. The only surface `make test` cannot reach
is the pjsua triple behind `pjsip-linked`, which is pre-existing; T071 covers it
manually.
