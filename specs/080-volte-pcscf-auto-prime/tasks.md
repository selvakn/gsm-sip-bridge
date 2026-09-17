---

description: "Task list for Automatic VoLTE P-CSCF Priming"
---

# Tasks: Automatic VoLTE P-CSCF Priming

**Input**: Design documents from `/specs/080-volte-pcscf-auto-prime/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/volte-auto-prime-contract.md, quickstart.md

**Tests**: Included — this project's constitution defaults to Integration-First
Testing and TDD (`.specify/memory/constitution.md` Principles I and the
Development Workflow section), and CLAUDE.md requires `make lint`/`make
test` to pass before every commit. All tests here run against
`supervise::runner::MockCommandRunner`, the same seam the code they extend
already uses — no new mocks are introduced.

**Organization**: Tasks are grouped by user story (spec.md P1/P2/P3), after a
shared Setup/Foundational phase that only the check function and module
scaffolding need (the actual capture mechanism is delivered as US1, since
that IS the feature's core mechanism — see plan.md's Summary).

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to (US1/US2/US3)
- File paths are relative to `gsm-sip-bridge/` (the Cargo member crate), matching plan.md's Project Structure

---

## Phase 1: Setup

**Purpose**: Register the new module so later tasks have somewhere to land

- [ ] T001 Create `src/supervise/orchestrate_prime.rs` with a module doc comment describing its purpose (transient VoWiFi capture to prime `[volte].pcscf_source_path` — cross-reference `specs/080-volte-pcscf-auto-prime/plan.md`), and add `mod orchestrate_prime;` to `src/supervise/mod.rs`

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: The availability check and the pure extraction every user story's behavior is built on

**⚠️ CRITICAL**: No user story work can begin until this phase is complete

- [ ] T002 [P] Add `pub fn pcscf_is_available(cache_path: &std::path::Path, override_addr: Option<&str>) -> bool` to `src/volte/pcscf.rs` (checks `override_addr` parses as a valid `IpAddr` first, else `probe_epdg_cache(cache_path).found().is_some()` — FR-001/FR-005), plus unit tests: override present and valid → `true` without touching the filesystem; no override, valid cache → `true`; no override, missing/empty/corrupt cache → `false`
- [ ] T003 Extract the "bring up this line's netns + XFRM tun + USIM bridge + swanctl connection, then wait for `Established`" sequence currently inline in `src/supervise/orchestrate.rs::start_vowifi_line_strongswan` (from the `epdg_iface::ensure_epdg_interface` call through the `tick_establishing` loop that produces `pcscf`) into a new `pub(super) fn establish_line_and_capture_pcscf(...)` in `orchestrate.rs`, called by `start_vowifi_line_strongswan` in exactly the same place with exactly the same behavior. Run `cargo test --workspace` before and after to confirm zero test-output diff (pure extraction, per plan.md's Constitution Check)
- [ ] T004 In `src/supervise/orchestrate_prime.rs`, add a function that assembles a `shutdown::StartedState` containing exactly one `shutdown::StartedVowifiLine` (plus its charon/pcscd/usim-bridge handles in `vowifi_child_handles`) from a priming attempt's handles, and a thin wrapper calling `shutdown::build_shutdown_plan` then `shutdown::execute_shutdown_plan` against it (data-model.md's "Synthetic teardown state"). Unit test with `MockCommandRunner`: given fake handles/netns/conn-name, assert the resulting `TeardownStep` sequence terminates the IKE SA, kills every child handle, deletes the tun and veth links, and deletes the netns — mirroring the existing assertion style in `src/supervise/shutdown.rs`'s own tests

**Checkpoint**: Foundation ready — the check function and teardown plumbing exist and are tested in isolation; no wiring into `orchestrate_volte.rs` yet

---

## Phase 3: User Story 1 - First VoLTE deployment on a new SIM/carrier (Priority: P1) 🎯 MVP

**Goal**: `[volte].enabled = true`, no override, no cache file → the system captures a P-CSCF on its own and VoLTE registration proceeds, with no manual `[vowifi]` step (spec.md User Story 1)

**Independent Test**: Start `supervise` with `[volte].enabled = true`, `[vowifi]` at defaults, no override, no `/tmp/pcscf-0` — via `MockCommandRunner`, assert `discover` is invoked, a synthetic line's tunnel is established, `/tmp/pcscf-0`-equivalent write happens, the transient line is torn down, and only then is `volte-register`/`volte-bridge` spawned

### Tests for User Story 1

- [ ] T005 [P] [US1] Unit test in `src/supervise/orchestrate_prime.rs`: `prime_pcscf` succeeds end-to-end against `MockCommandRunner` (discover returns one line → establish reaches `Established` → cache file written with the captured address → teardown steps issued) — assert call ordering matches data-model.md's state-transition diagram
- [ ] T006 [P] [US1] Unit test in `src/supervise/orchestrate_prime.rs`: `prime_pcscf` fails cleanly when `discover` yields no usable line (no candidate modem/SIM) — assert no charon/pcscd spawn is attempted and the returned `Err` names the reason (FR-007)
- [ ] T007 [P] [US1] Unit test in `src/supervise/orchestrate_prime.rs`: `prime_pcscf` fails cleanly when establish never reaches `Established` within the bounded timeout (research.md R3) — assert teardown still runs for whatever partially started (charon/pcscd killed, netns/veth deleted) and the `Err` is distinguishable from a carrier registration failure

### Implementation for User Story 1

- [ ] T008 [US1] Implement `pub fn prime_pcscf(runner: Arc<dyn CommandRunner>, bin: &str, config_path: &str, config: &AppConfig) -> Result<(), String>` in `src/supervise/orchestrate_prime.rs`: (1) run the `discover` subcommand via `runner.run` and read `vowifi::discovery::read_line_resolution`, taking `resolution.lines[0]` (FR-002b) or returning `Err` if empty; (2) build a request-scoped `engines::SharedCharon` and private pcscd/vpcd pair (`vpcd::start_pcscd_with_retries`) on dedicated `/tmp/volte-prime-*` paths (research.md R2); (3) call T003's `establish_line_and_capture_pcscf` with a bounded timeout/attempt cap; (4) on success, write the captured address to `config.volte.pcscf_source_path` (FR-003) and tear down via T004's helper; (5) on any failure, tear down whatever partially started via T004's helper and return `Err` with a message that names "priming" explicitly (FR-007)
- [ ] T009 [US1] In `src/supervise/orchestrate_volte.rs::start_legacy_registration`, add a `pcscf_is_available(...)` check (T002) at the top of the existing retry loop body, before spawning `volte-register`; when `false`, call `orchestrate_prime::prime_pcscf(...)`, log its outcome, and `continue` to the loop's existing sleep/retry on failure rather than spawning `volte-register` this cycle
- [ ] T010 [US1] In `src/supervise/orchestrate_volte.rs::start_multiline`, add a one-time pre-flight `pcscf_is_available(...)` check (T002) after `volte-discover-lines` succeeds and before the per-line spawn loops; when `false`, loop calling `orchestrate_prime::prime_pcscf(...)` on the same cadence as `start_legacy_registration`'s existing retry delay until it succeeds (FR-008), logging each attempt's outcome, before proceeding to spawn any line
- [ ] T011 [US1] Ensure `orchestrate_prime::prime_pcscf`'s log output and `orchestrate_volte.rs`'s call sites print an unambiguous "[supervise] priming P-CSCF ..." / "[supervise] priming succeeded/failed: ..." line distinct from any VoLTE registration failure message (FR-007, SC-004; verified by contracts/volte-auto-prime-contract.md's "Priming outcome behavior" table)

**Checkpoint**: User Story 1 is fully functional — a fresh VoLTE-only deployment auto-primes and registers, independently testable via `MockCommandRunner`

---

## Phase 4: User Story 2 - Cache lost after a redeploy (Priority: P2)

**Goal**: A previously-primed deployment whose cache file is now missing/empty/corrupt re-primes automatically on the next boot, on the existing retry cadence, with no operator recognizing the cause (spec.md User Story 2)

**Independent Test**: Seed `MockCommandRunner`'s filesystem with an empty (or corrupt, or absent) cache file where a valid one previously existed, start the same startup path as US1, and confirm priming runs exactly as it would on a first-time deployment — no special-casing needed beyond what T002/T008–T010 already provide

### Tests for User Story 2

- [ ] T012 [P] [US2] Unit test: `pcscf_is_available` (T002) returns `false` for an empty file and for a file containing non-address text, not just a missing file — extends T002's test module, closing the "corrupted cache treated as missing" acceptance scenario
- [ ] T013 [P] [US2] Unit test in `src/supervise/orchestrate_volte.rs`'s test module: `start_legacy_registration`'s loop, given a cache file that flips from valid (skip priming, spawn `volte-register`) to missing on a later iteration (simulating a mid-run wipe), calls `prime_pcscf` again on the next iteration rather than continuing to spawn `volte-register` with a stale/absent address
- [ ] T014 [US2] Unit test in `src/supervise/orchestrate_volte.rs`'s test module: a `prime_pcscf` failure is retried on the same interval already used for a `volte-register`/`volte-bridge` spawn failure (assert against the existing sleep-duration constant, not a new one — FR-008, research.md R4)

### Implementation for User Story 2

- [ ] T015 [US2] Fix any gap T012 surfaces in `probe_epdg_cache`/`pcscf_is_available`'s handling of an empty or non-address file (expected to already be correct per existing `a_corrupt_epdg_capture_is_a_failure_not_an_empty_result` test in `src/volte/pcscf.rs` — this task is "confirm and, if needed, adjust `pcscf_is_available` to treat `Failed` the same as `NoResult`", not a rewrite)

**Checkpoint**: User Stories 1 AND 2 both work independently — first-time priming and redeploy recovery are the same code path, now proven by both test sets

---

## Phase 5: User Story 3 - Operator has already pinned a permanent address (Priority: P3)

**Goal**: An explicit override or an already-valid cache means zero priming activity and zero added startup time (spec.md User Story 3)

**Independent Test**: With `MockCommandRunner` seeded with either a valid override or a valid pre-existing cache file, assert `discover` (beyond what `[vowifi].enabled`/circuit-switched discovery already does) is never invoked and no charon/pcscd/usim-bridge spawn ever happens

### Tests for User Story 3

- [ ] T016 [P] [US3] Unit test in `src/supervise/orchestrate_volte.rs`'s test module: `start_legacy_registration`'s loop, given a valid `[[volte.line]].pcscf` override, never calls `prime_pcscf` (assert via a call counter / `MockCommandRunner`'s recorded commands showing no priming-specific `discover`/charon invocation)
- [ ] T017 [P] [US3] Unit test in `src/supervise/orchestrate_volte.rs`'s test module: same assertion for `start_multiline`'s pre-flight check, given a valid pre-existing cache file
- [ ] T018 [P] [US3] Unit test: `pcscf_is_available` returns `true` (and thus short-circuits priming) when the override is present even if the cache file is simultaneously missing/invalid — override always wins, per FR-001

### Implementation for User Story 3

- [ ] T019 [US3] No new production code expected — this phase should be pure regression coverage confirming T002/T009/T010's existing branching already satisfies FR-005/SC-003. If any test fails, fix `pcscf_is_available` or its call sites (not the tests) to match FR-005 exactly.

**Checkpoint**: All three user stories independently pass; pinned/already-cached deployments are provably unaffected

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: Documentation, full-workspace verification, and the explicit hardware-validation caveat this feature cannot resolve in this environment

- [ ] T020 [P] Update `docs/operations.md`'s "The VoWiFi priming dance" section to state that `supervise` now performs this automatically on every `[volte].enabled` boot when no override/valid cache exists, keeping the manual procedure documented only as: (a) what happens internally, for troubleshooting, and (b) the "Making it permanent" pinning option, which still works unchanged and now also serves as the opt-out for an operator who wants zero priming activity ever
- [ ] T021 Run `make format && make lint && make test` across the whole workspace and fix any violation before considering this feature done, per CLAUDE.md's mandatory pre-commit checklist
- [ ] T022 Walk `specs/080-volte-pcscf-auto-prime/quickstart.md` step by step; for every step that requires real hardware/root (this sandbox has neither — see project memory on sandboxed network testing), explicitly record in the final report that it is UNVERIFIED rather than silently skipping it, per plan.md's Constraints section

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies
- **Foundational (Phase 2)**: Depends on Setup — BLOCKS all user stories
- **User Story 1 (Phase 3)**: Depends on Foundational only — delivers the actual priming mechanism (the MVP)
- **User Story 2 (Phase 4)**: Depends on Foundational + US1 (reuses `prime_pcscf`/the wiring US1 built; adds no new mechanism, only characterizes existing behavior against the redeploy scenario)
- **User Story 3 (Phase 5)**: Depends on Foundational + US1 (reuses T002/T009/T010's branching; adds no new mechanism, only proves the skip path)
- **Polish (Phase 6)**: Depends on US1–US3 all being complete

### Within Each Phase

- Tests are written first and must fail before their corresponding implementation task, per the constitution's TDD default
- T003 (extraction) must land, and `cargo test --workspace` must be green, before T008 (which calls the extracted function)
- T009 and T010 both depend on T008 (need `prime_pcscf` to exist) but not on each other — parallelizable once T008 lands

### Parallel Opportunities

- T002, T003 can run in parallel (different files: `volte/pcscf.rs` vs `orchestrate.rs`)
- T005, T006, T007 (US1 tests) can be written in parallel once T004 exists, ahead of T008
- T012, T013 (US2 tests) can run in parallel
- T016, T017, T018 (US3 tests) can all run in parallel

---

## Parallel Example: Foundational Phase

```bash
Task: "Add pcscf_is_available to src/volte/pcscf.rs with unit tests"
Task: "Extract establish_line_and_capture_pcscf from orchestrate.rs::start_vowifi_line_strongswan"
```

## Parallel Example: User Story 1 tests

```bash
Task: "Unit test: prime_pcscf succeeds end-to-end against MockCommandRunner"
Task: "Unit test: prime_pcscf fails cleanly when discover yields no usable line"
Task: "Unit test: prime_pcscf fails cleanly on establish timeout, with teardown still running"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Complete Phase 1: Setup
2. Complete Phase 2: Foundational (T002–T004)
3. Complete Phase 3: User Story 1 (T005–T011) — this alone delivers the feature's entire value proposition (spec.md SC-001)
4. **STOP and VALIDATE**: `cargo test --workspace`, then walk quickstart.md as far as this sandbox allows (T022)
5. Only then continue to US2/US3, which are almost entirely regression coverage over the same mechanism

### Incremental Delivery

1. Setup + Foundational → mechanism's building blocks exist and are unit-tested in isolation
2. US1 → first-boot auto-priming works end-to-end (MVP)
3. US2 → redeploy-recovery proven to be the same code path, no special-casing needed
4. US3 → already-pinned/already-cached deployments proven unaffected
5. Polish → docs, full lint/test gate, explicit hardware-validation caveat recorded

---

## Notes

- No task in this list can be fully verified against real hardware in this
  session (plan.md's Constraints, quickstart.md's final section) — every
  test task uses `MockCommandRunner`, matching how the code being extended
  (`orchestrate.rs`'s existing per-line and shutdown-plan tests) is already
  tested. This is a real limitation, not a shortcut: flag it explicitly
  (T022) rather than claiming hardware-level confidence this session cannot
  produce.
- [P] tasks touch different files or independent test functions within the
  same file — no two [P] tasks in the same phase write to the same lines.
- Commit after each task, per CLAUDE.md and the constitution's Frequent
  Atomic Commits principle; run `make format && make lint && make test`
  before every commit (T021 is the final whole-workspace pass, not the only
  one).
