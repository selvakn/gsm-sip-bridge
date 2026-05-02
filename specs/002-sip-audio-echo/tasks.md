# Tasks: SIP Audio Echo Server

**Input**: Design documents from `/specs/002-sip-audio-echo/`
**Prerequisites**: plan.md (required), spec.md (required), research.md, data-model.md, contracts/

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Project structure and dependency setup for SIP echo module

- [ ] T001 Update CMakeLists.txt to add sip-echo binary target, link PJSIP via pkg-config, and add sip test target
- [ ] T002 [P] Add mINI header-only library to vendor/mini/ini.h
- [ ] T003 [P] Update Makefile with run-sip target and ensure build/test/clean cover the new binary
- [ ] T004 [P] Create config.ini.example with documented SIP fields in project root

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Config parsing and PJSIP initialization that all user stories depend on

- [ ] T005 Implement SipConfig struct and INI parser in src/sip/sip_config.h and src/sip/sip_config.cpp
- [ ] T006 [P] Write integration test for SipConfig parsing (happy + sad paths) in tests/integration/test_sip_config.cpp
- [ ] T007 Create src/sip/main.cpp with CLI argument parsing, signal handling, and PJSIP Endpoint lifecycle (init, null audio device, shutdown)

**Checkpoint**: Config parses correctly, PJSIP endpoint starts and shuts down cleanly

---

## Phase 3: User Story 1 - SIP Registration and Audio Echo (Priority: P1)

**Goal**: Register with SIP server, answer incoming calls, echo audio back to the caller

**Independent Test**: Start server with valid config, call the extension, hear your own voice echoed back

### Tests for User Story 1

- [ ] T008 [P] [US1] Write integration test for SIP registration and call echo lifecycle using PJSIP loop transport in tests/integration/test_sip_echo.cpp

### Implementation for User Story 1

- [ ] T009 [US1] Implement EchoAccount (pj::Account subclass) in src/sip/echo_account.h and src/sip/echo_account.cpp: onRegState callback, onIncomingCall to create EchoCall and auto-answer
- [ ] T010 [US1] Implement EchoCall (pj::Call subclass) in src/sip/echo_call.h and src/sip/echo_call.cpp: onCallState logging, onCallMediaState to connect AudioMedia back to itself for echo
- [ ] T011 [US1] Wire EchoAccount and EchoCall into src/sip/main.cpp: create transport, create account config from SipConfig, register, run event loop

**Checkpoint**: Server registers, answers calls, echoes audio, handles BYE

---

## Phase 4: User Story 2 - Configuration Management (Priority: P2)

**Goal**: Robust config validation with clear error messages for missing or invalid fields

**Independent Test**: Start server with various broken config files and verify specific error messages

### Implementation for User Story 2

- [ ] T012 [US2] Add field-level validation to SipConfig::load(): required field checks, port range validation, transport enum validation with specific error messages per field
- [ ] T013 [US2] Add --config CLI flag to src/sip/main.cpp for custom config path (default: config.ini)
- [ ] T014 [P] [US2] Add sad-path integration tests for config validation in tests/integration/test_sip_config.cpp: missing file, missing fields, invalid port, invalid transport

**Checkpoint**: Config errors produce clear, actionable messages with specific field names

---

## Phase 5: User Story 3 - Continuous Operation and Error Recovery (Priority: P3)

**Goal**: Handle sequential calls, re-registration on loss, clean shutdown

**Independent Test**: Make multiple calls, simulate registration loss, verify recovery

### Implementation for User Story 3

- [ ] T015 [US3] Add re-registration logic to EchoAccount::onRegState: detect registration loss and trigger re-register
- [ ] T016 [US3] Add concurrent call rejection in EchoAccount::onIncomingCall: respond 486 Busy Here when a call is already active
- [ ] T017 [US3] Add clean shutdown to src/sip/main.cpp: on SIGINT/SIGTERM, hangup active call, unregister account, destroy endpoint
- [ ] T018 [P] [US3] Write integration test for sequential calls and busy rejection in tests/integration/test_sip_echo.cpp

**Checkpoint**: Server handles multiple calls, re-registers, shuts down cleanly

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: Documentation and final validation

- [ ] T019 Update README.md with SIP echo server section (prerequisites, config, usage)
- [ ] T020 Run quickstart.md validation: verify build, config, and run instructions work end-to-end

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies
- **Foundational (Phase 2)**: Depends on Phase 1
- **US1 (Phase 3)**: Depends on Phase 2
- **US2 (Phase 4)**: Depends on Phase 2, can run in parallel with US1
- **US3 (Phase 5)**: Depends on US1 (needs EchoAccount/EchoCall)
- **Polish (Phase 6)**: Depends on all user stories

### Parallel Opportunities

- T002, T003, T004 can run in parallel (Phase 1)
- T006 can run in parallel with T005 (write test first, TDD)
- T008 can start before T009-T011 (TDD: write failing test first)
- T014 can run in parallel with T012-T013

## Implementation Strategy

### MVP First (Phase 1 + 2 + 3)

1. Setup + Foundational: config parsing, PJSIP init
2. US1: registration + echo = working echo server
3. STOP and manually validate with a real SIP client

### Full Delivery (All Phases)

4. US2: config validation hardening
5. US3: resilience (re-register, busy, clean shutdown)
6. Polish: README, quickstart validation
