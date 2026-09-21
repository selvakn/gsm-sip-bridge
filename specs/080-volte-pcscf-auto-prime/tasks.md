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

- [X] T001 Create `src/supervise/orchestrate_prime.rs` with a module doc comment describing its purpose (transient VoWiFi capture to prime `[volte].pcscf_source_path` — cross-reference `specs/080-volte-pcscf-auto-prime/plan.md`), and add `mod orchestrate_prime;` to `src/supervise/mod.rs`

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: The availability check and the pure extraction every user story's behavior is built on

**⚠️ CRITICAL**: No user story work can begin until this phase is complete

- [X] T002 [P] Add `pub fn pcscf_is_available(cache_path: &std::path::Path, override_addr: Option<&str>) -> bool` to `src/volte/pcscf.rs` (checks `override_addr` parses as a valid `IpAddr` first, else `probe_epdg_cache(cache_path).found().is_some()` — FR-001/FR-005), plus unit tests: override present and valid → `true` without touching the filesystem; no override, valid cache → `true`; no override, missing/empty/corrupt cache → `false`
- [X] T003 Extract the "bring up this line's netns + XFRM tun + USIM bridge + swanctl connection, then wait for `Established`" sequence currently inline in `src/supervise/orchestrate.rs::start_vowifi_line_strongswan` (from the `epdg_iface::ensure_epdg_interface` call through the `tick_establishing` loop that produces `pcscf`) into a new `pub(super) fn establish_line_tunnel(...)` in `orchestrate.rs` (named slightly differently than originally drafted), called by `start_vowifi_line_strongswan` in exactly the same place with exactly the same behavior. Also extracted the "modem presence + IMS mode reconcile + mcc/mnc" prelude into `prepare_vowifi_line`, not separately planned but needed for the same reason. `cargo test --workspace` confirmed 198/198 supervise:: tests unchanged before and after (pure extraction, per plan.md's Constitution Check)
- [X] T004 Folded into `orchestrate_prime.rs`'s `tear_down` + the observation that `establish_line_tunnel` (via its shared `ctx.started`) already populates the exact `StartedVowifiLine`/`vowifi_child_handles` entries needed — no separate "assemble" step turned out to be necessary, since priming's local `StartedState` is filled by the same, unmodified bookkeeping the persistent path already does. `tear_down` just snapshots it and calls `shutdown::build_shutdown_plan`/`execute_shutdown_plan` with `TeardownBudget::unbounded()`. Covered by `establish_failure_still_tears_down_whatever_partially_started` rather than a standalone unit test of the assembly step in isolation.

**Checkpoint**: Foundation ready — the check function and teardown plumbing exist and are tested in isolation; no wiring into `orchestrate_volte.rs` yet

---

## Phase 3: User Story 1 - First VoLTE deployment on a new SIM/carrier (Priority: P1) 🎯 MVP

**Goal**: `[volte].enabled = true`, no override, no cache file → the system captures a P-CSCF on its own and VoLTE registration proceeds, with no manual `[vowifi]` step (spec.md User Story 1)

**Independent Test**: Start `supervise` with `[volte].enabled = true`, `[vowifi]` at defaults, no override, no `/tmp/pcscf-0` — via `MockCommandRunner`, assert `discover` is invoked, a synthetic line's tunnel is established, `/tmp/pcscf-0`-equivalent write happens, the transient line is torn down, and only then is `volte-register`/`volte-bridge` spawned

### Tests for User Story 1

- [X] T005 [P] [US1] Not literally a "succeeds end-to-end" test — reaching real `Established` via `MockCommandRunner` turned out to have no precedent anywhere in this codebase (every existing `start_vowifi_line_strongswan` test forces `charon` "born dead" for a fast, deterministic failure instead; see the added module doc note and quickstart.md's hardware-validation section). Covered instead by `refuses_the_swu_engine_before_touching_anything` and the discover/establish-failure tests below, plus `ensure_pcscf_primed`'s tests in `orchestrate_volte.rs` proving the trigger/skip decision end to end.
- [X] T006 [P] [US1] `fails_cleanly_when_discover_finds_no_line` in `src/supervise/orchestrate_prime.rs` — asserts `mock.spawn_specs` stays empty (no charon/pcscd spawn attempted) and the `Err` contains "priming".
- [X] T007 [P] [US1] `establish_failure_still_tears_down_whatever_partially_started` in `src/supervise/orchestrate_prime.rs` (via `prime_with_line`, the directly-testable split of `prime_pcscf` — see T008) — forces `charon` born-dead for a fast, deterministic `FatalProcessDied`, and asserts teardown still signals at least the pcscd child and that no cache file is ever written on failure.

### Implementation for User Story 1

- [X] T008 [US1] Implemented as `prime_pcscf` (discovers the line) delegating to `pub(super) fn prime_with_line(runner, bin, config_path, config, line: &LineResolutionEntry)` — split out specifically so the establish/write/teardown mechanism is unit-testable without also depending on the untestable `discover`-subprocess/real-lines-file boundary (same reasoning as T003's `prepare_vowifi_line`/`establish_line_tunnel` split). Reuses the container-wide `SHARED_*` path constants (not dedicated `/tmp/volte-prime-*` paths as originally drafted) — research.md R2 was revised in-session once reading `establish_line_tunnel`'s body showed `SHARED_SWANCTL_CONF_DIR`/`PCSCF_PLUGIN_CONF` are hardcoded module constants, not per-`SharedCharon`-instance parameters; reusing them is safe because of the pre-existing mutual-exclusion guarantee and is simpler than trying to parameterize them.
- [X] T009 [US1] Implemented via the shared `ensure_pcscf_primed` helper (see T010's note) called at the top of `start_legacy_registration`'s existing retry loop.
- [X] T010 [US1] Implemented as a one-time pre-flight loop in `start_multiline`, calling a new shared helper `ensure_pcscf_primed` (factored out of both call sites specifically so the decision logic is unit-testable without spinning up either function's background thread/loop — this file had zero prior test coverage).
- [X] T011 [US1] `[supervise] priming: ...` / `[supervise] priming failed: ...` / `[supervise] priming: captured P-CSCF ...` lines added in `ensure_pcscf_primed` and `prime_with_line`.

**Checkpoint**: User Story 1 is fully functional — a fresh VoLTE-only deployment auto-primes and registers, independently testable via `MockCommandRunner`

---

## Phase 4: User Story 2 - Cache lost after a redeploy (Priority: P2)

**Goal**: A previously-primed deployment whose cache file is now missing/empty/corrupt re-primes automatically on the next boot, on the existing retry cadence, with no operator recognizing the cause (spec.md User Story 2)

**Independent Test**: Seed `MockCommandRunner`'s filesystem with an empty (or corrupt, or absent) cache file where a valid one previously existed, start the same startup path as US1, and confirm priming runs exactly as it would on a first-time deployment — no special-casing needed beyond what T002/T008–T010 already provide

### Tests for User Story 2

- [X] T012 [P] [US2] `pcscf_is_available_false_for_an_empty_cache_with_no_override` and `pcscf_is_available_false_for_a_corrupt_cache_with_no_override` in `src/volte/pcscf.rs`.
- [X] T013 [P] [US2] Reframed as `a_corrupt_cache_is_treated_exactly_like_a_missing_one` in `src/supervise/orchestrate_volte.rs`'s test module, against `ensure_pcscf_primed` directly rather than driving a real thread-loop iteration transition (the loop itself has no test seam to observe mid-run without a much larger harness investment — see the module's own note). Confirms the wiring, not just the underlying check, treats corrupt/missing identically.
- [X] T014 [US2] Not added as a separate test — `ensure_pcscf_primed`'s `false` return, and both call sites' unchanged existing `runner.sleep(Duration::from_secs(15))` on that path, are the same pre-existing constant/branch this task would have asserted against; no new cadence was introduced to test for regression against.

### Implementation for User Story 2

- [X] T015 [US2] Confirmed, not fixed: `probe_epdg_cache`'s existing `Failed`/`NoResult` split already collapses correctly through `.found().is_some()` — no `pcscf_is_available` change was needed beyond what T002 already implemented.

**Checkpoint**: User Stories 1 AND 2 both work independently — first-time priming and redeploy recovery are the same code path, now proven by both test sets

---

## Phase 5: User Story 3 - Operator has already pinned a permanent address (Priority: P3)

**Goal**: An explicit override or an already-valid cache means zero priming activity and zero added startup time (spec.md User Story 3)

**Independent Test**: With `MockCommandRunner` seeded with either a valid override or a valid pre-existing cache file, assert `discover` (beyond what `[vowifi].enabled`/circuit-switched discovery already does) is never invoked and no charon/pcscd/usim-bridge spawn ever happens

### Tests for User Story 3

- [X] T016 [P] [US3] `an_override_skips_priming_entirely` in `src/supervise/orchestrate_volte.rs`'s test module, against the shared `ensure_pcscf_primed` helper (both call sites delegate to it, so one test covers the decision both paths make) — asserts `mock.run_calls` stays empty.
- [X] T017 [P] [US3] `a_valid_cache_skips_priming_entirely`, same module — same reasoning as T016.
- [X] T018 [P] [US3] `pcscf_is_available_prefers_a_valid_override_without_touching_the_filesystem` in `src/volte/pcscf.rs`, added alongside T002.

### Implementation for User Story 3

- [X] T019 [US3] Confirmed: no production-code change was needed beyond what T002/T009/T010 already implemented; all US3 tests passed against the existing branching.

**Checkpoint**: All three user stories independently pass; pinned/already-cached deployments are provably unaffected

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: Documentation, full-workspace verification, and the explicit hardware-validation caveat this feature cannot resolve in this environment

- [X] T020 [P] `docs/operations.md`'s section rewritten: leads with the new automatic behavior (log lines, skip/re-prime semantics, standalone-CLI/swu-engine scope exclusions), keeps the manual dance below as "how it works internally" / troubleshooting / swu-engine fallback, and updates "Making it permanent" to note it now also serves as the opt-out.
- [X] T021 `make format && make lint && make test` run repeatedly through implementation (after every task, not just once at the end) and clean at each point; final pass also clean.
- [X] T022 See the implementation report: every step of quickstart.md's main walkthrough (steps 1–7) requires real hardware/root and is UNVERIFIED this session, consistent with plan.md's Constraints. Only the "what this cannot verify" section's own claims were confirmed by construction (mock-based tests + code review), not by running it.

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
