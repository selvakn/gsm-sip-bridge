# Tasks: Inbound Call Bridging over the Host-Side LTE Registration

**Input**: Design documents from `/specs/017-volte-inbound-bridge/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: Test tasks ARE included — the constitution makes integration-first
testing non-negotiable and requires a green suite on every commit.

**Organization**: Grouped by user story, in the spec's priority order. US1, US2
and US5 are all P1; they are sequenced by dependency, not importance.

**Numbering**: T001–T088 were the original breakdown; T089 was added after a
coverage audit found a requirement with no task against it. IDs are append-only
so existing references stay valid.

**Status**: Phase 2's pure core implemented 2026-07-22. Tests for pure logic
live in the module rather than `tests/`, matching the convention `media_stats`
and `echo` already follow in this project.

**Originally**: Generated before implementation. Every task unchecked — verified
against the tree: `volte/bridge.rs` and `volte/sms.rs` do not exist.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to

## Path Conventions

Single Rust workspace. Source at `gsm-sip-bridge/src/`, tests at
`gsm-sip-bridge/tests/`.

---

## Phase 1: Foundational — one implementation, not two

**Purpose**: Extract the shared machinery out of `ims::agent` so a single
implementation serves both paths (FR-019, SC-008). **MUST complete before any
user story**, and **MUST land alone** — it touches the production Wi-Fi path,
and nothing else should be in flight when it does.

- [X] T001 Identify the reusable surface in `gsm-sip-bridge/src/ims/agent.rs`: inbound request dispatch, renewal deferral, message acknowledgement, status reporting
- [X] T002 Extract inbound request dispatch into a transport-agnostic form in `gsm-sip-bridge/src/ims/agent.rs`
- [X] T003 Extract the registration-lifecycle loop (renewal deferral while a call is active) into a reusable form in `gsm-sip-bridge/src/ims/agent.rs`
- [X] T004 Make the extracted pieces reachable from `volte/` without duplicating them, in `gsm-sip-bridge/src/ims/mod.rs`
- [X] T005 **Prove the Wi-Fi path unchanged**: its suite passes unmodified after the extraction (FR-020)
- [X] T006 **Place a live Wi-Fi call** to confirm no behavioural regression (SC-007) — outstanding since the transport refactor in feature 015. **Cannot be done from this environment**: production Wi-Fi runs elsewhere and the SIM here is the cellular test line. Operator to run before merge

---

## Phase 2: User Story 5 — Keep receiving text messages (P1)

**Goal**: No text is lost when this service takes the registration.

**Independent test**: Send a text to the SIM; it is recorded and forwarded
exactly as today.

**Why first among the stories**: it is pure, needs no carrier to build, and
covers a **regression** rather than an addition — the capability disappears
unless handled.

- [X] T007 [P] [US5] Create `gsm-sip-bridge/src/volte/sms.rs` and register it in `gsm-sip-bridge/src/volte/mod.rs`
- [X] T008 [US5] Define `InboundMessage` with its delivery route in `gsm-sip-bridge/src/volte/sms.rs`
- [X] T009 [US5] Implement the deduplication key so a retransmission is recognised (FR-027, FR-037) in `gsm-sip-bridge/src/volte/sms.rs`
- [ ] T010 [US5] Handle messages arriving over the registration, reusing the existing acknowledgement (FR-025) in `gsm-sip-bridge/src/volte/sms.rs` — **blocked on Phase 3**: needs the service that owns the registration
- [ ] T011 [US5] Handle messages the network leaves in modem storage, reusing `gsm-sip-bridge/src/sms/reader.rs` (FR-036) — **partial**: `list_sms_indexes` added to the reader; the notification handler needs the service
- [ ] T012 [US5] Recover messages already in modem storage at startup (US5 scenario 7) in `gsm-sip-bridge/src/volte/sms.rs` — **partial**: listing and parsing done and tested; the startup sweep needs the service
- [ ] T013 [US5] Converge both routes on the existing `sms::record_and_forward` (FR-025, FR-036) in `gsm-sip-bridge/src/volte/sms.rs`
- [ ] T014 [US5] **Acknowledge only after recording**, and clear modem storage only after recording (FR-026, FR-036) in `gsm-sip-bridge/src/volte/sms.rs`
- [ ] T015 [US5] Record even when forwarding fails, and report the failure (FR-029) in `gsm-sip-bridge/src/volte/sms.rs`
- [ ] T089 [US5] Handle a message arriving during a call, and a call arriving while a message is being processed, without either displacing the other (FR-028) in `gsm-sip-bridge/src/volte/bridge.rs`

### Tests

- [X] T016 [P] [US5] Create `gsm-sip-bridge/tests/test_volte_sms.rs`: a message on each route is recorded and forwarded once
- [X] T017 [P] [US5] Test the same message arriving on **both** routes is recorded once (FR-037) in `gsm-sip-bridge/tests/test_volte_sms.rs`
- [X] T018 [P] [US5] Test a retransmission is acknowledged but not duplicated (FR-027) in `gsm-sip-bridge/tests/test_volte_sms.rs`
- [ ] T019 [US5] **Test that a crash between acknowledging and recording cannot lose a message** — ordering is the whole safety property, in `gsm-sip-bridge/tests/test_volte_sms.rs` — **blocked on Phase 3**: the ordering is documented and the dedupe that absorbs the resulting retransmission is tested, but the ordering itself cannot be tested until there is I/O to order
- [ ] T020 [P] [US5] Test recording survives a forwarding failure (FR-029) in `gsm-sip-bridge/tests/test_volte_sms.rs`

---

## Phase 3: User Story 1 — Answer a call and connect it through (P1)

**Goal**: An incoming cellular call reaches the operator's telephone system.

**Independent test**: Dial the SIM; the telephone system rings and a
conversation is possible both ways.

- [X] T021 [P] [US1] Create `gsm-sip-bridge/src/volte/bridge.rs` and register it in `gsm-sip-bridge/src/volte/mod.rs`
- [X] T022 [US1] Define `BridgedCall` and `CallStage` per the data model in `gsm-sip-bridge/src/volte/bridge.rs`
- [X] T023 [US1] Accept an incoming call over the registration, reusing the extracted dispatch (FR-001) in `gsm-sip-bridge/src/volte/bridge.rs`
- [X] T024 [US1] **Give this service its own telephone-side local port** — two endpoints already race for one; a third must not join them (research R3) in `gsm-sip-bridge/src/volte/bridge.rs`
- [X] T025 [US1] Place the telephone-system leg and pair the two legs (FR-002) in `gsm-sip-bridge/src/volte/bridge.rs`
- [X] T026 [US1] Present the caller's number and display name onward (FR-003) in `gsm-sip-bridge/src/volte/bridge.rs`
- [X] T027 [US1] Choose the answer's audio format deliberately, preferring wideband (FR-007) in `gsm-sip-bridge/src/ims/sdp.rs`
- [X] T028 [US1] End both legs when either ends, recording which (FR-004) in `gsm-sip-bridge/src/volte/bridge.rs`
- [X] T029 [US1] Give the caller a defined outcome when the telephone system does not answer or is unreachable (FR-005) in `gsm-sip-bridge/src/volte/bridge.rs`
- [X] T030 [US1] Reject a second concurrent call as busy, without disturbing the call in progress (FR-006) in `gsm-sip-bridge/src/volte/bridge.rs`
- [X] T031 [US1] Withdraw the telephone-system leg if the caller hangs up while it is still ringing (edge case) in `gsm-sip-bridge/src/volte/bridge.rs`
- [X] T032 [US1] Add the service subcommand in `gsm-sip-bridge/src/cli.rs` and wire it in `gsm-sip-bridge/src/main.rs`
- [X] T033 [US1] Refuse to start while the Wi-Fi path holds the same subscriber's registration, reusing `gsm-sip-bridge/src/volte/guard.rs` (FR-022)

### Tests

- [ ] T034 [P] [US1] Create `gsm-sip-bridge/tests/test_volte_bridge.rs`: a second call while bridged is rejected busy and the first is undisturbed
- [X] T035 [P] [US1] Test the answer-side format preference selects wideband when the offer allows it (FR-007) in `gsm-sip-bridge/src/ims/sdp.rs`
- [X] T036 [P] [US1] Test call-stage transitions, including that only `Bridged` can succeed, in `gsm-sip-bridge/tests/test_volte_bridge.rs`

---

## Phase 4: User Story 2 — Keep answering, indefinitely (P1)

**Goal**: The service survives renewals and the carrier's periodic teardowns.

**Independent test**: Run for hours across a teardown; calls connect before and
after.

- [X] T037 [US2] Hold one registration for both liveness and calls, renewed before expiry; never a second per call (FR-008, FR-012) in `gsm-sip-bridge/src/volte/bridge.rs`
- [X] T038 [US2] Defer renewal while a call is in progress, reusing the extracted lifecycle (FR-009) in `gsm-sip-bridge/src/volte/registration.rs`
- [X] T039 [US2] **Defer re-attachment while a call is in progress** — the genuinely new hazard; the existing deferral covers renewal only (FR-009, research R2) in `gsm-sip-bridge/src/volte/registration.rs`
- [X] T040 [US2] Recover attachment and registration automatically when lost while idle (FR-010) in `gsm-sip-bridge/src/volte/bridge.rs`
- [ ] T041 [US2] End a call with the attachment named as the cause when it is genuinely lost mid-call, distinct from the caller hanging up (FR-011) in `gsm-sip-bridge/src/volte/bridge.rs`
- [ ] T042 [US2] Let a call outlive its registration rather than cutting it short (spec Assumptions) in `gsm-sip-bridge/src/volte/registration.rs`
- [ ] T043 [US2] Make a persistent inability to register or attach visible rather than silent (FR-013, FR-035) in `gsm-sip-bridge/src/volte/bridge.rs`

### Tests

- [ ] T044 [P] [US2] Test renewal falling due during a call is deferred until it ends, in `gsm-sip-bridge/tests/test_volte_bridge.rs`
- [ ] T045 [P] [US2] **Test re-attachment falling due during a call is deferred** — the case the existing implementation does not cover, in `gsm-sip-bridge/tests/test_volte_bridge.rs`
- [ ] T046 [P] [US2] Test a genuine attachment loss mid-call ends the call attributed to the attachment, in `gsm-sip-bridge/tests/test_volte_bridge.rs`

---

## Phase 5: User Story 3 — See what the service is doing (P2)

**Goal**: An operator can tell whether it is healthy and why a call failed.

**Independent test**: Query while idle, during a call, and after a failure.

- [ ] T047 [US3] Answer a live status query over the existing control channel (FR-014, FR-033) in `gsm-sip-bridge/src/volte/bridge.rs`
- [ ] T048 [US3] Report registration state, current call and remaining lifetime in the shared vocabulary (FR-014, FR-018) in `gsm-sip-bridge/src/volte/bridge.rs`
- [ ] T049 [US3] Derive `can_answer`, and never optimistically (SC-009) in `gsm-sip-bridge/src/volte/bridge.rs`
- [ ] T050 [US3] Record recent call outcomes with enough detail to tell a normal call from a failed one (FR-015) in `gsm-sip-bridge/src/volte/bridge.rs`
- [ ] T051 [US3] Name the stage a failed call reached (FR-016) in `gsm-sip-bridge/src/volte/bridge.rs`
- [ ] T052 [US3] **Never report a call that carried no audio, or one-way audio, as successful** — reuse the existing verdict (FR-017) in `gsm-sip-bridge/src/volte/bridge.rs`
- [ ] T053 [US3] Report calls through the **existing** call measurements, tagged as this path (FR-030) in `gsm-sip-bridge/src/metrics/mod.rs`
- [ ] T054 [US3] Keep registration and attachment measurements distinct from the other paths' (FR-031) in `gsm-sip-bridge/src/metrics/mod.rs`
- [ ] T055 [US3] Extend the status command to query this service in `gsm-sip-bridge/src/main.rs`

### Tests

- [ ] T056 [P] [US3] Test `can_answer` is false when unregistered, unattached, or busy, in `gsm-sip-bridge/tests/test_volte_bridge.rs`
- [ ] T057 [P] [US3] Test a one-way call is reported as failed with the direction named, in `gsm-sip-bridge/tests/test_volte_bridge.rs`

---

## Phase 6: User Story 4 — Choose which cards use this path (P3)

**Goal**: Per-card selection, opt-in, exclusive.

- [ ] T058 [US4] Add per-card selection to the `[volte]` section (FR-023) in `gsm-sip-bridge/src/config/mod.rs`
- [ ] T059 [US4] **Default to the existing arrangement when unset**, leaving the modem-internal path available and unchanged (FR-021, FR-024) — what makes this safe to merge — in `gsm-sip-bridge/src/config/mod.rs`
- [ ] T060 [US4] Add this service as a third exclusive subsystem in `gsm-sip-bridge/src/modules/discovery.rs`
- [ ] T061 [US4] Ensure a card assigned here is not driven by the circuit-switched daemon (FR-034) in `gsm-sip-bridge/src/modules/discovery.rs`
- [ ] T062 [US4] Refuse when both this path and the Wi-Fi path are enabled for one subscriber (US4 scenario 3) in `docker/entrypoint.sh`
- [ ] T063 [US4] Supervise the service from `docker/entrypoint.sh`, releasing the attachment on shutdown
- [ ] T064 [P] [US4] Document the selection in `config.toml.example`

### Tests

- [ ] T065 [P] [US4] Test an absent selection yields the existing arrangement, in `gsm-sip-bridge/src/config/mod.rs`
- [ ] T066 [P] [US4] Test a card cannot be claimed by two subsystems, in `gsm-sip-bridge/src/modules/discovery.rs`

---

## Phase 7: Live validation

**Gates**: B1, B2, B3, B4. None of this can be simulated.

- [ ] T067 Stop anything else holding the registration — they displace each other
- [ ] T068 Re-confirm incoming calls still reach us with `volte-listen`, isolating "the network is fine" from "our service is broken"
- [ ] T069 **Gate B1**: first bridged call — telephone system rings promptly, is answered, and carries a conversation both ways for 60s (SC-001, SC-002)
- [ ] T070 Test teardown from the calling side, then from the telephone-system side; both legs end cleanly each way
- [ ] T071 **Gate B2**: sample the quality class during an inbound call on a second AT port; record whether a voice-class context appears. **A confirmed absence is a result**
- [ ] T072 **Gate B4**: send a text while the service runs; confirm it is recorded and forwarded once, indistinguishably from today, and **record which route delivered it** (SC-010, SC-011)
- [ ] T073 Send a text during a call; both must be handled
- [ ] T074 Restart with a text already in modem storage; confirm it is recovered
- [ ] T075 **Gate B3**: confirm existing call dashboards show these calls with no panel modified, and identify any panel that splits by transport (FR-032, SC-012)
- [ ] T076 Confirm this registration's health is distinguishable from the Wi-Fi path's while one is down and the other up (SC-013)
- [ ] T077 Dial a second call while one is bridged; expect busy, first call undisturbed
- [ ] T078 Stop the telephone system, then dial in; expect a defined outcome quickly, not silence
- [ ] T079 **US2 soak**: run 4+ hours across at least one attachment teardown and several renewals; a call connects at the end (SC-003)
- [ ] T080 Confirm no call was interrupted by the service's own maintenance (SC-004)
- [ ] T081 If possible, be mid-call during a teardown; the call must end attributed to the attachment and the service must recover
- [ ] T082 Record all findings — including any negative — in `specs/017-volte-inbound-bridge/research.md`

---

## Phase 8: Polish

- [ ] T083 [P] Document the service in `docs/operations.md`
- [ ] T084 [P] Update `specs/017-volte-inbound-bridge/quickstart.md` with anything the live run proved wrong
- [ ] T085 Keep `make lint` clean and the workspace suite green on every commit
- [ ] T086 [P] Verify SC-005: no failure mode exercised reports a silent call as successful
- [ ] T087 [P] Verify SC-006: every failure mode names its stage
- [ ] T088 Verify SC-008: registration, authentication, signalling protection, call handling and audio exist **once** and serve both paths

---

## Dependencies

```
Phase 1 (Extract shared machinery)   ← lands ALONE; touches the production path
      ↓
Phase 2 (US5 messages, P1)           ← pure, no carrier needed
      ↓
Phase 3 (US1 answer + bridge, P1)    ← Gate B1
      ↓
Phase 4 (US2 keep running, P1)       ← needs a working call to protect
      ↓
Phase 5 (US3 observability, P2)      ← Gate B3
      ↓
Phase 6 (US4 selection, P3)
      ↓
Phase 7 (Live validation)            ← Gates B1, B2, B3, B4
      ↓
Phase 8 (Polish)
```

**Story independence**: US5 is fully independent of the call path and could
ship alone. US1 needs Phase 1. US2 protects US1 so must follow it. US3 and US4
are independent of each other.

## Parallel Opportunities

- **Phase 2**: T016–T020 are independent tests of one module.
- **Phase 3**: T034–T036 test different concerns.
- **Phase 4**: T044–T046 are independent lifecycle cases.
- **Phase 6/8**: T064, T065, T066, T083, T084, T086, T087 touch disjoint files.

## Implementation Strategy

**MVP**: Phases 1–3. A card that answers incoming calls into the telephone
system and does not lose text messages. That is the first genuinely useful
outcome of this whole line of work.

**Increment 2**: Phase 4 — survives unattended.
**Increment 3**: Phases 5–6 — observable and selectable.
**Increment 4**: Phase 7–8 — validated and documented.

---

# Risk Register

| Gate | Question | If it fails |
|---|---|---|
| **B1** | Does a bridged call connect end to end? | The feature does not work. Everything else is moot |
| **B2** | Does an inbound call get voice treatment? | Inbound audio is carried as ordinary data. The outbound experiment measured ~45× the packet loss in that situation. **A confirmed absence is a publishable result**, not a failure to retry |
| **B3** | Do existing dashboards still work? | Panels grouping by transport split into two series — visual, not broken, but must be identified rather than discovered by an operator |
| **B4** | Are texts delivered, by either route? | Silent message loss, which nobody notices until someone asks why they never got one |

**The most dangerous task in this list is T001–T005.** Extracting shared
machinery out of `ims::agent` touches the **production Wi-Fi path**. A copy
would be faster and would satisfy FR-019 and SC-008 only in appearance — two
copies of registration and renewal logic drift, and the drift surfaces on
whichever path is tested less. T006 exists because a live Wi-Fi call has still
not been placed since the transport refactor two features ago.

**The subtlest is T039 and T045.** Renewal deferral during a call already
exists and is easy to reuse. **Re-attachment deferral does not** — that hazard
arrived with the LTE transport, whose attachment the carrier tears down roughly
every two hours. Reusing the existing lifecycle without adding it would look
correct, pass every existing test, and drop a call roughly every two hours.

**The easiest to get silently wrong is T014.** Acknowledging a message before
recording it means a crash in between loses it outright, with the network
believing it was delivered. Acknowledging after means a crash causes a
retransmission, which T009's deduplication then absorbs. The ordering is the
entire safety property.
