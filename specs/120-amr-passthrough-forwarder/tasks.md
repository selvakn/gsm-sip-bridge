# Tasks: AMR passthrough for AMR-capable clients

**Input**: `specs/120-amr-passthrough-forwarder/` — spec.md, plan.md, research.md (R1–R13), data-model.md, contracts/, quickstart.md
**Tests**: required by the spec (FR-020) and the constitution (integration-first, no mocks). Tests come before the code they cover.
**Commit rule**: every phase ends with `make format && make lint && make test`, then one commit per task group (CLAUDE.md pre-commit checklist). Use only synthetic numbers (`+919000000000`) in fixtures.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: different files, no dependency on an unfinished task
- **[US1..US4]**: user story from spec.md. Setup, foundational and polish tasks carry no story label

---

## Phase 1: Gate spikes (block all product code)

**Purpose**: prove the two unproven PJSIP behaviours from research R2/R3/R11 on stock pjproject 2.16. If S1 fails, stop and revisit the design (plan Summary, R1/R2) before Phase 2.

- [X] T001 S1 — extend `specs/111-amr-passthrough-spike/harness/b2.c` (or add `b3.c`) so B places a normal call to Y, then registers a forward entry and re-INVITEs with `PJSUA_CALL_REINIT_MEDIA`. Production call settings (`aud_cnt=1, vid_cnt=0, txt_cnt=0`). Capture to `specs/111-amr-passthrough-spike/results/s1-*.txt`
- [X] T002 S1 — verify and record in `specs/120-amr-passthrough-forwarder/spike-s1.md`: re-INVITE SDP carries the forwarder's `c=`/`m=`; PJSIP's old socket released; stream created paused (sent 0 / received 0); AMR-WB frames bit-identical both ways; hold/resume intact; Y's incompatible re-INVITE rejected 488 via `on_call_rx_reinvite`; no leaked UDP port after hangup; also the downgrade re-INVITE (no forward entry) restores the base transport
- [X] T003 S2 — in the same harness, confirm a call unpaired from the conference stays silent for a whole call and that a UAS which sends/receives nothing on it is undisturbed. Record in `spike-s1.md`
- [X] T004 Decision gate — write the go / no-go and any design change at the top of `spike-s1.md`; if no-go, update research.md R1–R3 before continuing

**Checkpoint**: S1 and S2 recorded as go.

---

## Phase 2: Foundational (pure logic, config, protocol)

**Purpose**: everything both agents and every story need. No PJSIP yet.

### Config (FR-015)

- [X] T005 [P] Test first: `amr_passthrough` defaults to `false`, accepts `true`, rejects a non-boolean at load — beside `idle_keepalive`'s tests in `gsm-sip-bridge/src/config/mod.rs`; extend `gsm-sip-bridge/tests/test_config.rs` and `test_config_docs.rs` as that file's pattern requires
- [X] T006 Add `amr_passthrough` to `VowifiConfig` (`gsm-sip-bridge/src/config/{mod,raw,build}.rs`), default `false`
- [X] T007 [P] Export `AMR_PASSTHROUGH` in `gsm-sip-bridge/src/commands/config.rs`; extend `gsm-sip-bridge/tests/test_shell_env_contracts.rs`

### Parameters and the compatibility rule (FR-003)

- [X] T008 [P] Test first, `gsm-sip-bridge/src/amr_forward/params.rs`: parse `AmrLegParams` from fmtp/rtpmap for NB and WB (`octet-align`, `mode-set`, `crc`, `robust-sorting`, `interleaving`, `ptime`/`maxptime`, telephone-event PT); malformed `mode-set` → feature error
- [X] T009 [P] Test first, same file: compatibility matrix, one case per FR-003 clause both directions — flavour differs (WB/NB), mode sets (unrestricted vs restricted, subset, superset, disjoint), each feature flag, frames-per-packet, packing differs but compatible, DTMF mismatch → `Reason::Dtmf` (R10). Assert the `Reason::as_str()` strings from data-model.md
- [X] T010 Create `gsm-sip-bridge/src/amr_forward/{mod,params}.rs` (`AmrLegParams`, `MediaPath`, `Reason`, `compatible()`), register the module in `lib.rs`
- [X] T011 Extend `gsm-sip-bridge/src/ims/sdp.rs` so the chosen AMR codec carries `AmrLegParams` (mode-set and feature flags added to what `ChosenCodec` already holds); existing sdp tests must pass unchanged

### Repacking (FR-003, FR-007)

- [X] T012 [P] Test first, `gsm-sip-bridge/src/amr_forward/repack.rs`: `repack` round-trip a→b→a identical for every frame type (NB 0–7 + SID + NO_DATA, WB 0–8 + SID + NO_DATA), 1–3 frames per packet, CMR preserved; cross-check speech bits against `ims::amr_rtp::payload_to_frame`; truncated/garbled input → `None`
- [X] T013 Expose the needed internals in `gsm-sip-bridge/src/ims/amr_rtp.rs` as `pub(crate)` and implement `repack.rs`

### Control protocol (contracts/control-protocol.md)

- [X] T014 [P] Test first, `gsm-sip-bridge/src/vowifi/control.rs` tests: round-trip of `IncomingCall{carrier_amr}`, `ForwardMedia`, `ForwardAck`, `ForwardCancel`; an old `incoming_call` without `carrier_amr` parses to `None`; a `CallRecord` without `media_path` parses; `call_id()` covers the new variants
- [X] T015 Add the fields and variants in `gsm-sip-bridge/src/vowifi/control.rs`; fix every exhaustive match the compiler reports

### Relay (R6, FR-007, FR-008, FR-010, FR-013)

- [X] T016 [P] Test first, `gsm-sip-bridge/src/amr_forward/relay.rs`, real loopback UDP sockets, no mocks: payload bytes bit-identical both ways; SSRC/seq/ts untouched; audio PT and telephone-event PT relabelled per leg; repack when packing differs; direction gate stops sending towards a held leg and resumes; remote address update takes effect for the next packet; SID/NO_DATA/CMR pass through; counters per direction; `stop` ends the threads and frees the ports (rebind succeeds)
- [X] T017 Implement `relay.rs` (`ForwardLeg` pair, `ForwardSession` with `Drop`, `Direction` atomics, counters), generalising `ims/agent/veth.rs::forward`; make the RTCP clock rate a parameter instead of the PCMU constant
- [X] T018 Make `veth.rs::forward` and the new relay share the SSRC tracker / RTCP role hooks rather than copying them; existing veth tests unchanged

**Checkpoint**: `make format && make lint && make test` green; commit groups: config, params+sdp, repack, control protocol, relay.

---

## Phase 3: User Story 1 — inbound call forwarded to an AMR client (P1) 🎯 MVP

**Goal**: with the setting on, a qualifying inbound VoWiFi/VoLTE call carries bit-identical AMR frames both ways, with DTMF and hold/resume.
**Independent test**: quickstart §2 steps 1–4.

### Tests first

- [ ] T019 [P] [US1] `pjsua-safe` hook test in the privileged container (`test/`, add a `make` target per quickstart §1): real PJSIP, no forward entry → base transport returned and a normal stream; with an entry → advertised `c=`/`m=` is the forwarder's, stream paused (sent 0 / received 0), old socket closed
- [ ] T020 [P] [US1] Same container: hold/resume on a forwarded call re-pauses and refreshes remote address and direction; an incompatible re-INVITE is answered 488 and the existing media continues; hangup leaves no bound port and no registry entry
- [ ] T021 [P] [US1] Agent A test in `gsm-sip-bridge/src/ims/agent/` (real sockets, style of the existing inbound tests): `ForwardMedia` for an AMR carrier call makes A spawn the forwarding relay; `ForwardCancel`, a nack, or no message makes it use the transcoding relay exactly as today; `IncomingCall` carries `carrier_amr` only when the setting is on and the codec is AMR
- [ ] T022 [P] [US1] Agent B test in `gsm-sip-bridge/src/vowifi/` : the upgrade sequence (bind → `ForwardMedia` → ack → re-INVITE → recheck → `CallAnswered`) against a stub control peer and real sockets; every failure branch (`ack_timeout`, nack, re-INVITE refused, recheck incompatible) leaves a transcoded call and sends `ForwardCancel` where required; `Drop` frees everything on each branch (FR-017)

### Implementation

- [ ] T023 [US1] `pjsua-safe/src/passthrough.rs` (new, the only new `unsafe`): safe API `register_forward(call_id, ForwarderAddr)`, `clear_forward(call_id)`, `client_params(call_id) -> Option<ClientMedia{fmtp, pts, remote, dir}>`; the three C callbacks; loop-transport create + base close; stream pause; `on_call_rx_reinvite` rejection for forwarded calls (R2, R11). Every `unsafe` block carries the `// SAFETY:` comment the audit expects
- [ ] T024 [US1] Wire it into `pjsua-safe/src/endpoint.rs` (register callbacks, clear the entry when the call disconnects, keep `BRIDGE_PAIRS` semantics) and `pjsua-safe/src/call.rs` (`reinvite_onto_forwarder`, stream-info getter)
- [ ] T025 [US1] Agent A (`ims/agent/inbound.rs`, `veth.rs`, `mod.rs`): populate `carrier_amr` in `IncomingCall`; handle `ForwardMedia` inside the existing `await_pbx_answer` control drain; reply `ForwardAck`; on `CallAnswered` spawn the forwarding relay between the carrier socket and the veth forwarding socket (RTCP bundle roles as today) instead of `spawn_transcoding_relay`; log `forwarding=` in `call answered and bridged`
- [ ] T026 [US1] Agent B (`vowifi/mod.rs`): after `wait_for_pbx_answer` returns `Answered` and before `CallAnswered`, read the client's params, run `compatible()`, and run the upgrade sequence from research R3; unpair the veth call from the conference on success; own the `ForwardSession`; keep refreshing remote address/direction in the existing media-state poll
- [ ] T027 [US1] VoLTE parity: pass the setting through `volte/carrier_agent.rs` so the host-side bridge's lines behave the same; add a case to `gsm-sip-bridge/tests/test_volte_bridge.rs` if that file's pattern allows
- [ ] T028 [US1] Run the T019–T022 tests to green; then quickstart §2 steps 1–4 on the Vodafone rig (pcap bit-compare, DTMF both ways, hold/resume) and record the result in `specs/120-amr-passthrough-forwarder/live-verification.md`

**Checkpoint**: US1 works on the local rig. `make format && make lint && make test`; commits: pjsua-safe FFI, Agent A, Agent B, VoLTE.

---

## Phase 4: User Story 2 — everything else behaves as today (P1)

**Goal**: non-AMR targets, non-AMR carrier legs, incompatible AMR and the GSM path are untouched, and a decision can never fail a call.
**Independent test**: quickstart §2 step 6, plus the regression set (SC-002).

- [ ] T029 [P] [US2] Test: a compatible-looking call whose client answers a restricted `mode-set` the carrier may exceed connects transcoded with two-way audio (a real-socket test through the Agent B decision path) and logs `reason=mode_set`
- [ ] T030 [P] [US2] Test: carrier WB / client NB and the reverse → `flavour_differs`, transcoded; client answers PCMU/G.722/Opus → `client_not_amr`; no `ForwardMedia` is ever sent in these cases
- [ ] T031 [P] [US2] Test: with the setting off, Agent A omits `carrier_amr`, Agent B never calls `register_forward`, and the call takes today's exact path (assert the sequence of control messages equals today's)
- [ ] T032 [US2] Fill any gaps those tests expose in the decision code (T026) — no behaviour change for the non-qualifying branch
- [ ] T033 [US2] Confirm the circuit-switched path is untouched: `git diff` shows no change under `gsm-sip-bridge/src/sip/`, `modules/`, `line/` beyond shared helpers; run `gsm-sip-bridge/tests/test_cs_disabled.rs` and `test_bridge_call.rs`
- [ ] T034 [US2] Regression pass on the local rig (quickstart §2 step 6, built-in registrar and an external PBX, inbound and outbound, non-AMR target): per-leg codecs, audio and outcome identical to the previous release. Record in `live-verification.md`

**Checkpoint**: SC-002 and SC-003 evidenced.

---

## Phase 5: User Story 3 — outbound unaffected (P2)

- [ ] T035 [P] [US3] Test: an outbound call placed with the setting on never registers a forward entry and its call record reads `transcoded:outbound` (exercise `run_outbound_listener` / `bridge_outbound_leg` paths with the existing outbound test harness)
- [ ] T036 [US3] Set `media_path = transcoded:outbound` where the outbound call record is built; no other change in the outbound code (FR-016)
- [ ] T037 [US3] Live: one outbound call from baresip on the Vodafone rig and one on the Jio line with the setting on; two-way audio, `transcoded:outbound` in `vowifi-status`. Record in `live-verification.md`

---

## Phase 6: User Story 4 — operator visibility and kill switch (P2)

- [ ] T038 [P] [US4] Test: the `media path decided` line and `CallRecord.media_path` carry the exact strings in `contracts/logs.md` for forwarded (with each leg's packing) and for every `Reason`; non-AMR calls record nothing new
- [ ] T039 [US4] Emit the Agent B log line and set `media_path` on the record for every inbound call that reaches the decision (`vowifi/mod.rs`, `push_recent_call`); add `forwarding=` to Agent A's existing line if T025 did not
- [ ] T040 [US4] Verify `vowifi-status` prints `media_path` for recent calls (extend its formatter and test beside the existing `phone_codec` handling)
- [ ] T041 [US4] Live kill-switch run (quickstart §2 step 7): setting off + recreate → `transcoded:disabled` for a qualifying call; setting on + recreate → forwarded

---

## Phase 7: Polish and release verification

- [ ] T042 [P] Docs: `docs/configuration.md` (setting, default, inbound-only, restart needed), `docs/vowifi-bridge.md` and `docs/architecture.md` (when a call is forwarded and when not, the upgrade sequence, the PBX caveat that stock Asterisk does not pass AMR through, how to read the path from logs), `docs/operations.md` (kill switch), `config.toml.example` — FR-019, SC-007
- [ ] T043 [P] `CHANGELOG.md` and `RELEASE_NOTES.md`: **Feature name** bolded, plain description after (release-notes rule); state off by default, how to turn on, inbound only
- [ ] T044 Jio line live run on the Pi (arm64 via `arm-build-200`): quickstart §3, 5-minute soak with DTMF and hold/resume, plus an outbound call and a non-AMR target. Record in `live-verification.md` (SC-006, FR-020b)
- [ ] T045 Cost and delay on the Pi (quickstart §4): forwarded vs transcoded CPU (SC-004 ≥70%) and added delay (SC-005 ≥20 ms). Record numbers in `live-verification.md`; if a target is missed, say so in the release notes rather than rounding up
- [ ] T046 One-way-audio monitor check on a forwarded call (FR-013): kill one direction at the client, confirm the existing one-way-audio report fires; kill the client mid-call, confirm teardown and no leaked ports (FR-017)
- [ ] T047 Final `make format && make lint && make test`; confirm no real numbers anywhere in the diff (CLAUDE.md); update the spec's Status to Implemented

---

## Dependencies and order

- Phase 1 blocks everything. Phase 2 blocks Phases 3–6.
- Within Phase 2: T005→T006, T008/T009→T010→T011, T012→T013, T014→T015, T016→T017→T018; the five groups are independent of each other.
- Phase 3: T019–T022 first (they fail), then T023→T024→T025/T026 (A and B in parallel once T024 exists), T027, T028.
- Phases 4, 5, 6 depend on Phase 3's decision code and can proceed in parallel with each other.
- Phase 7: docs (T042, T043) can start once Phase 3 is stable; live runs (T044–T046) come last.

## MVP

Phases 1–3 deliver User Story 1 on the local rig. Phase 4 must also pass before this is releasable, because the feature must not regress other calls. Phases 5–7 complete the spec.

## Parallel example (Phase 2)

```text
T005 config test      T008/T009 params tests     T012 repack test
T014 control test     T016 relay test
```
