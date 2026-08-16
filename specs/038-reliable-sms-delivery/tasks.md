---

description: "Task list for Reliable SMS Delivery"
---

# Tasks: Reliable SMS Delivery

**Input**: Design documents from `/specs/038-reliable-sms-delivery/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, quickstart.md

**Tests**: Included — this codebase's constitution (Integration-First Testing, NON-NEGOTIABLE) requires them, and the feature's spec calls out cross-bearer duplicate suppression as a functional requirement that is not safely verifiable by inspection alone.

**Organization**: Tasks are grouped by user story (spec.md priorities P1–P3) so each can be delivered and validated independently.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies on incomplete tasks)
- **[Story]**: US1 (P1, no lost SMS), US2 (P2, no duplicate SMS), US3 (P2, VoLTE parity), US4 (P3, CS-only unaffected)
- File paths are exact and relative to the repository root

## Phase 1: Setup

**Purpose**: Establish a clean, known-good baseline before touching anything.

- [X] T001 Run `make format && make lint && make test` on the current branch tip and confirm a clean pass, so any later failure is attributable to this feature's changes, not pre-existing state

**Checkpoint**: Baseline confirmed green.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: `volte::sms::run_modem_reader`/`sweep_modem_storage` currently construct their own private `Dedupe` internally, and nothing outside that thread ever calls `decide()` in production (research.md Decision 2). Every user story below depends on this being fixed first — US1 needs the reader wired for VoWiFi at all, US2/US3 need the `Dedupe` instance to be shared with the registration-message handler, and none of that is possible without changing this shared signature once, up front, rather than reworking it per-story.

**⚠️ CRITICAL**: No user story task may begin until this phase compiles and passes existing tests.

- [X] T002 In `gsm-sip-bridge/src/volte/sms.rs`, change `run_modem_reader(modem_port, control_addr, modem_lock)` to also accept `dedupe: Arc<Mutex<Dedupe>>`, removing its internal `Dedupe::default()`; update `sweep_modem_storage` to take `dedupe: &mut Dedupe` reached through the caller's mutex guard instead of an owned local
- [X] T003 In `gsm-sip-bridge/src/commands/volte.rs` (`volte-carrier-agent` subcommand, around line 1162) and `gsm-sip-bridge/src/volte/bridge.rs` (`run_line`, around line 321), construct one `Arc<Mutex<Dedupe>>` per line alongside the existing `modem_lock`, and pass it into the now-updated `run_modem_reader` call — these are VoLTE's existing call sites and must keep compiling and passing `test_volte_sms.rs` unchanged in behavior at this stage
- [X] T004 In `gsm-sip-bridge/src/ims/agent/mod.rs`, add a `dedupe: Arc<Mutex<Dedupe>>` field to `InboundParams`/`DispatchParams` (mirroring how `modem_lock` is already threaded through both), and construct one per line at the top of `run_inner` (used by both the VoWiFi and — via `volte::carrier_agent::run`'s call into `serve_inbound` — VoLTE call paths); `handle_message` does not yet consult it (that is US2/T009) — this task only makes the shared instance reachable
- [X] T005 Run `cargo build --workspace` and `make test` to confirm the foundational refactor compiles cleanly and all pre-existing tests (especially `test_volte_sms.rs`) still pass with unchanged behavior

**Checkpoint**: Shared `Dedupe` plumbing exists and compiles; no observable behavior has changed yet.

---

## Phase 3: User Story 1 - No SMS is silently lost on a VoWiFi- or VoLTE-only line (Priority: P1) 🎯 MVP

**Goal**: A VoWiFi (or VoLTE) line with `[cs].enabled = false` forwards to the operator every SMS the carrier delivers, whether it arrives over the IMS registration or the modem's own storage — closing the confirmed data-loss bug (7-part SMS, 6 of 7 parts stuck unread).

**Independent Test**: Configure a line with `[cs].enabled = false` and VoWiFi enabled against a real modem (`pcsc_reader = false`). Have a text delivered into the modem's own storage. Confirm it reaches the operator (Discord + `sms` table) without any other subsystem enabled. See quickstart.md for the full manual procedure.

### Tests for User Story 1

- [X] T006 [P] [US1] In `gsm-sip-bridge/src/ims/agent/mod.rs`'s existing `#[cfg(test)] mod tests` block, add a test for a new pure predicate `fn wants_modem_sms_reader(pcsc_reader: bool) -> bool` (see T008) asserting it returns `true` when `pcsc_reader` is `false` and `false` when `true` — mirrors the existing `plan_startup`-style pure-decision-function pattern used elsewhere in this codebase (`commands/daemon.rs`)
- [X] T007 [P] [US1] In `gsm-sip-bridge/tests/test_volte_sms.rs`, add a regression test asserting `parse_cmgl_indexes`/backlog-recovery behavior is unaffected by the `Dedupe` now being externally-owned (construct an `Arc::new(Mutex::new(Dedupe::default()))`, confirm `decide()` through the shared lock behaves identically to the pre-refactor owned-`Dedupe` case)

### Implementation for User Story 1

- [X] T008 [US1] In `gsm-sip-bridge/src/ims/agent/mod.rs`, add the pure predicate `fn wants_modem_sms_reader(pcsc_reader: bool) -> bool { !pcsc_reader }` near `run_inner`
- [X] T009 [US1] In `gsm-sip-bridge/src/ims/agent/mod.rs::run_inner`, construct a real `modem_lock: Arc<Mutex<()>>` (replacing the current hardcoded `None` passed to `InboundParams`) and, when `wants_modem_sms_reader(config.pcsc_reader)` is true, spawn a background thread named `"vowifi-sms-{card_id}"` calling `crate::volte::sms::run_modem_reader(PathBuf::from(&config.modem_port), control_addr, modem_lock.clone(), dedupe.clone())` (the `dedupe` from T004); pass `modem_lock: Some(modem_lock)` into `InboundParams` instead of `None`; update the stale comment ("No LTE modem on this path, so nothing competes for an AT port") which is incorrect for any non-`pcsc_reader` line
- [X] T010 [US1] Update the module-level doc comment at the top of `gsm-sip-bridge/src/volte/sms.rs` (currently "Text messages over the host-side LTE path") to reflect that it now also serves VoWiFi, so the doc does not misdescribe its own scope
- [X] T011 [US1] Add a note under `[[vowifi.line]]` in `docs/configuration.md` explaining that the line's modem storage is now also swept for SMS the carrier delivers through the classic cellular bearer, independent of `[cs].enabled`
- [X] T012 [US1] Run `make format && make lint && make test`; fix any failures before proceeding

**Checkpoint**: VoWiFi-only deployments no longer lose SMS delivered through modem storage. This is the MVP — independently valuable and deployable on its own.

---

## Phase 4: User Story 2 - No SMS is shown to the operator twice (Priority: P2)

**Goal**: When the carrier delivers the same text over both bearers for one line, the operator sees it exactly once.

**Independent Test**: Feed the identical sender/body through both `MessageRoute::OverRegistration` and `MessageRoute::ThroughModem` against one shared `Dedupe` and confirm only the first is `Disposition::Handle`; at the wiring level, confirm `handle_message` and `sweep_modem_storage` for one line consult the same instance.

### Tests for User Story 2

- [X] T013 [P] [US2] In `gsm-sip-bridge/tests/test_volte_sms.rs`, add a test proving the *shared-instance* case end-to-end at the type level: construct one `Arc<Mutex<Dedupe>>`, run an `OverRegistration` message through `decide()`, then a `ThroughModem` message with the same sender/body through the *same* locked instance, and assert the second is `Disposition::AcknowledgeOnly` — this exercises what production wiring now does, distinct from the pre-existing test of the same logic against a bare (non-`Arc<Mutex<_>>`) `Dedupe`

### Implementation for User Story 2

- [X] T014 [US2] In `gsm-sip-bridge/src/ims/agent/mod.rs::handle_message`: build `InboundMessage { route: MessageRoute::OverRegistration, ... }`, check `dedupe.contains(&key)` (not `decide()` — admitting before the relay is known to succeed would let a failed-relay retransmission be silently swallowed as "already seen" instead of retried, mirroring the ordering `sweep_modem_storage` already uses). If already seen, ack-only and return. Otherwise relay as today, and only call `dedupe.admit(&key)` after a successful relay, before acknowledging
- [X] T015 [US2] Add a `route = MessageRoute::as_str()` field to the existing `tracing::info!("received SIP MESSAGE", ...)` event in `handle_message`, and to the per-message relay log inside `sweep_modem_storage` in `gsm-sip-bridge/src/volte/sms.rs` (FR-009 — bearer becomes observable in logs without a schema change, per research.md Decision 3)
- [X] T016 [US2] Run `make format && make lint && make test`; fix any failures before proceeding

**Checkpoint**: No duplicate notifications across bearers, for any line using this shared wiring; delivery bearer is now visible in logs.

---

## Phase 5: User Story 3 - VoLTE lines keep their existing reliability, with or without CS (Priority: P2)

**Goal**: VoLTE's pre-existing modem-storage-recovery guarantee is unchanged in behavior, and now also benefits from the cross-bearer duplicate suppression US2 introduced (a gap that, per research.md Decision 2, existed for VoLTE too before this feature).

**Independent Test**: Run a VoLTE line with `[cs].enabled = false`, and separately with `[cs].enabled = true` but this modem exclusively assigned to VoLTE. In both cases, a text delivered through modem storage reaches the operator exactly once, whether or not it is also (redundantly) delivered over the registration.

### Implementation for User Story 3

- [X] T017 [US3] Verify (and fix if not already true after T003) that `gsm-sip-bridge/src/commands/volte.rs` and `gsm-sip-bridge/src/volte/carrier_agent.rs` pass the *same* `Arc<Mutex<Dedupe>>` instance into both `run_modem_reader` and whatever reaches `ims::agent::serve_inbound`/`handle_message` for that line — two independently-constructed instances would silently defeat US2 for VoLTE specifically
- [X] T018 [P] [US3] In `gsm-sip-bridge/tests/test_volte_sms.rs`, add a regression test asserting VoLTE's startup backlog recovery (`parse_cmgl_indexes`) and dedupe behavior are unchanged for both `[cs].enabled = false` and `[cs].enabled = true` configurations, matching the spec's User Story 3 acceptance scenarios
- [X] T019 Run `make format && make lint && make test`; fix any failures before proceeding

**Checkpoint**: VoLTE keeps its existing guarantee and now shares US2's fix.

---

## Phase 6: User Story 4 - CS-only deployments are unaffected (Priority: P3)

**Goal**: A deployment with no VoWiFi/VoLTE enabled sees no behavior change at all.

**Independent Test**: Run existing CS-only test coverage unmodified and confirm it still passes — this feature touches no code on the CS-only path (`modules::mod`'s `BridgeEvent::SmsReceived` handler, `AT+CMTI`/`AT+CMGR` flow) at all.

### Implementation for User Story 4

- [X] T020 [US4] Run `gsm-sip-bridge/tests/test_cs_disabled.rs`, `test_sms_reader.rs`, and `test_sms_handler.rs` and confirm they pass unmodified, confirming no regression on the CS-only path; no production code changes are expected for this story

**Checkpoint**: All four user stories independently verified.

---

## Phase 7: Polish & Cross-Cutting Concerns

- [X] T021 [P] Re-read `gsm-sip-bridge/src/ims/agent/mod.rs`'s and `gsm-sip-bridge/src/volte/sms.rs`'s doc comments touched by this feature end-to-end for accuracy (no stale "no modem on this path" claims left over)
- [X] T022 Run `make format && make lint && make test` for the full workspace as the final gate before considering this feature complete
- [ ] T023 Follow `specs/038-reliable-sms-delivery/quickstart.md`'s manual, on-real-hardware verification steps against a live deployment (e.g. the `gsm-jio-cap` container used to originally diagnose this bug) to confirm the backlog of already-stuck messages drains and no new ones accumulate — this is a deployment/validation step outside the automated test suite and requires a rebuilt binary running against real modem hardware

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies
- **Foundational (Phase 2)**: Depends on Setup — BLOCKS all user stories (changes a shared function signature every story's implementation touches)
- **User Story 1 (Phase 3)**: Depends on Foundational only — deliverable as a standalone MVP
- **User Story 2 (Phase 4)**: Depends on Foundational; does not require US1's VoWiFi wiring to exist first (it patches `handle_message`, which both subsystems already reach), but is easiest to validate live once US1 is deployed, since that is what exposes the VoWiFi cross-bearer case
- **User Story 3 (Phase 5)**: Depends on Foundational and, in practice, US2's `handle_message` change (T014) to have something to verify
- **User Story 4 (Phase 6)**: Depends on nothing this feature changes — can run any time, included last only because it is pure verification
- **Polish (Phase 7)**: Depends on all four stories being complete

### Parallel Opportunities

- T006 and T007 (US1 tests) can run in parallel — different files
- T013 (US2 test) has no file overlap with US1's remaining tasks and can be drafted in parallel once Foundational lands
- T018 (US3 test) and T021 (doc pass) touch different files and can run in parallel with each other

---

## Implementation Strategy

### MVP First

1. Phase 1 (Setup) → Phase 2 (Foundational) → Phase 3 (US1)
2. **STOP and VALIDATE**: confirm on real hardware that a VoWiFi-only line no longer loses SMS (this is the confirmed bug that motivated the feature)
3. This alone is deployable — US2–US4 harden and protect the guarantee but US1 is the value delivery

### Incremental Delivery

1. Setup + Foundational → foundation ready, no behavior change yet
2. US1 → VoWiFi stops losing SMS (MVP, deploy)
3. US2 → duplicates across bearers stop showing (deploy)
4. US3 → VoLTE verified unaffected and gains the same dedupe fix (deploy)
5. US4 → CS-only verified unaffected (no deploy needed — pure regression check)
