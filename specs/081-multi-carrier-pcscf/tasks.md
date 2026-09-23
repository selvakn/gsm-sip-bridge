# Tasks: Multi-Carrier VoLTE P-CSCF Priming

**Input**: Design documents from `/specs/081-multi-carrier-pcscf/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: Included — the project constitution (Integration-First Testing,
Green-on-Commit) makes this non-optional. Unit tests are colocated in the
same source files as the code they cover (`#[cfg(test)] mod tests`),
matching this codebase's existing convention in `orchestrate_prime.rs`,
`pcscf.rs`, and `discovery.rs`. A top-level integration test file
(matching `tests/test_volte_line_netns.rs`'s convention) was planned for
cross-module scenarios but dropped during implementation — see T012's note
— in favor of extracting the coordinator's actual decision logic into a
directly-testable pure function, deterministically covering the same
claims without driving `start_multiline`'s real background threads.

**Organization**: Tasks are grouped by user story (spec.md). US2 and US3 are
deliberately test-only phases: per research.md R2/R7, "cache missing/invalid
re-primes" and "a pinned line sees no activity" are not separate code paths
— they are the same per-line three-tier check US1 builds, evaluated fresh
at every startup. Their phases exist to prove that design decision true,
not to add new production code.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependency on an
  incomplete task)
- **[Story]**: Which user story this task belongs to (US1, US2, US3)

## Path Conventions

Single Cargo workspace member at `gsm-sip-bridge/` (repo root also contains
sibling crates — `amr-safe`, `pjsua-sys`, etc. — untouched by this
feature). All paths below are relative to `gsm-sip-bridge/`.

---

## Phase 1: Setup

- [X] T001 Confirm a clean baseline on branch `081-multi-carrier-pcscf`: `make format && make lint && make test` all pass before any change, so every later failure is attributable to this feature's own commits

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: The two pure building blocks every user-story task depends on — the per-line cache key formula, and the ability to resolve more than one priming-shaped line at once without resource collisions.

**⚠️ CRITICAL**: No User Story 1 task may start until T002–T004 are complete.

- [X] T002 [P] Add `pub fn per_line_cache_path(base: &str, card_id: &str) -> std::path::PathBuf` to `src/volte/pcscf.rs` (research.md R1: `<base>-<card_id>`, e.g. `/tmp/pcscf-0-ec20-ABCDEF`), plus unit tests in that file's existing `#[cfg(test)] mod tests` covering the formula and that two different `card_id`s never collide
- [X] T003 [P] Generalize `vowifi::discovery::resolve_single_line(modem, base)` in `src/vowifi/discovery.rs` — today `resolve_one_line(0, modem, base)`, hardcoded — to accept an explicit pass-local index (new parameter, or a sibling `resolve_priming_line(modem, base, pass_index)`); add a unit test in that file's existing test module asserting two lines resolved at pass-local indices 0 and 1 produce distinct `netns`, `strongswan_tun_iface`, `strongswan_if_id`, veth addresses, and `vpcd_port` (research.md R4) — mirror the existing `lines_are_ordered_by_card_id_not_input_order`-style table-driven tests already in this file
- [X] T004 Extend `resolve_line_pcscf` in `src/commands/volte.rs` with the new middle tier from research.md R2 — (1) explicit override unchanged, (2) **new**: `per_line_cache_path(volte.pcscf_source_path, card_id)` via `probe_epdg_cache`, (3) the existing literal `pcscf_source_path` file unchanged as fallback; needs `card_id` threaded into `resolve_line_pcscf`'s signature (currently takes `explicit`/`pcscf_port`/`source_path` — add `card_id: &str`), update both call sites (`volte_bridge_manifest_lines`, the carrier-agent line-resolution call site) accordingly; unit tests confirming all three tiers in precedence order, and that a deployment with only the legacy file populated behaves exactly as before (depends on T002)

**Checkpoint**: Foundation ready — read-side resolution and collision-free multi-line resource derivation both exist and are tested.

---

## Phase 3: User Story 1 - Mixed-carrier fleet primes itself (Priority: P1) 🎯 MVP

**Goal**: A `[volte].enabled + bridge_inbound` deployment with several different-carrier lines and no configuration primes every line automatically, concurrently, each into its own `card_id`-keyed cache — closing the gap where every line silently read one shared address.

**Independent Test**: Start the bridge with multiple auto-discovered lines on different (simulated) carriers, no overrides, no pre-existing captures; observe every line ends up registered using its own carrier's address (spec.md User Story 1).

### Implementation for User Story 1

- [X] T005 [US1] In `src/supervise/orchestrate_prime.rs`, replace `discover_priming_line` (singular) with a plural resolver that, given a set of target `card_id`s needing priming, calls `scan_for_line_resolution` once and builds a `LineResolutionEntry` for each via `resolve_single_line`/`resolve_priming_line` (T003) at a distinct pass-local index per line (research.md R4) — depends on T003
- [X] T006 [US1] In `src/supervise/orchestrate_prime.rs`, replace `prime_pcscf`/`prime_with_line` with a pass-scoped `prime_pass(runner, bin, config_path, config, lines: &[(LineResolutionEntry, PathBuf)], started, real_shutting_down) -> Vec<(String, Result<(), String>)>`: render one shared charon's assets listing every line's connection name up front (mirroring `start_vowifi_subsystem`'s `conn_names`/`render_pcscf_plugin_conf` step, research.md R3), start one shared pcscd covering every line's own `vpcd_port`, run each line's `prepare_vowifi_line`/`establish_line_tunnel` on its own thread so one line's failure never blocks another (FR-005), and on each line's success write to that line's own path from T005 (not the global `config.volte.pcscf_source_path`) — depends on T005
- [X] T007 [US1] In `src/supervise/orchestrate_prime.rs`, generalize `tear_down` to the whole pass's `StartedState` (same four fields — `pcscd`, `vowifi_child_handles`, `started_netns`, `vowifi_lines` — now populated by every line in the pass, research.md R6) instead of one line's; call once after every line's attempt has concluded (success or failure) — depends on T006
- [X] T008 [US1] Update the `#[cfg(test)] mod tests` in `src/supervise/orchestrate_prime.rs`: adapt `refuses_the_swu_engine_before_touching_anything`, `establish_failure_still_tears_down_whatever_partially_started`, `tear_down_never_touches_resources_it_did_not_itself_create`, and `a_real_shutdown_mid_establish_abandons_the_attempt_and_still_tears_down` to the new pass-scoped signature, and add two new cases: (a) two lines in one pass, one fails (forced "born dead") and one succeeds — the failure must not prevent the other line's cache file from being written; (b) two lines primed together must never write to each other's `per_line_cache_path` — depends on T006, T007
- [X] T009 [US1] In `src/supervise/orchestrate_volte.rs::start_multiline`, replace the single pre-flight block (today: one `ensure_pcscf_primed` call gated on `manifest.lines.first()`'s override) with: for every manifest line, check the three-tier availability from T004 using that line's own `card_id`; if any lines are missing a usable address, call `prime_pass` (T006) once covering exactly those lines, before the existing per-line spawn loop — depends on T004, T006
- [X] T010 [US1] In `src/supervise/orchestrate_volte.rs::start_multiline`'s existing per-manifest-line `std::thread::spawn` loop (the one that already supervises each line's `volte-carrier-agent` restarts), add a step at its top, before the first spawn attempt: resolve this line's own address via the three-tier check (T004); if unavailable, `runner.sleep` on the existing 15s cadence and retry, exactly like every other retry loop in this file, rather than spawning — depends on T004, T009
- [X] T011 [P] [US1] Generalize `ensure_pcscf_primed`/`ensure_pcscf_primed_with` in `src/supervise/orchestrate_volte.rs` to take a cache path and per-line override rather than assuming the single global config path, with unit tests in this file's existing test module for the per-line decision logic (skip vs. attempt-and-report-false), mirroring the existing tests' style — depends on T004
- [X] T012 [US1] ~~New integration test file~~ **Deviation from the original plan, recorded here rather than silently**: driving `start_multiline` end-to-end through `MockCommandRunner` would require real background-thread timing (it spawns and never joins), making any such test either sleep-and-hope or flaky — confirmed live while implementing T008's sibling test (`orchestrate_prime::tests::one_lines_failure_does_not_prevent_or_corrupt_another_lines_outcome`): a genuine *successful* establish cannot be driven in this mock at all, since `SharedCharon::spawn_locked` unconditionally truncates the charon log the moment it spawns the (mocked) charon daemon, so a pre-seeded "established" marker can never survive to be read. Instead, extracted `lines_needing_priming` (the coordinator's actual decision logic — which `card_id`s belong in the next pass) out of the thread closure in `src/supervise/orchestrate_volte.rs`, mirroring the same rationale `ensure_pcscf_primed_with` was already extracted for, and unit-tested it directly (`every_unconfigured_line_needs_priming`, `already_available_lines_are_excluded_mixed_carrier_fleet`, `adding_a_new_line_does_not_disturb_an_already_primed_ones_status`) alongside T008's multi-line pass-isolation coverage in `orchestrate_prime.rs` and T004's `different_lines_never_read_each_others_per_line_cache`. Together these deterministically cover every observable claim User Story 1 makes, without the flakiness a background-thread integration test would introduce

**Checkpoint**: User Story 1 fully functional — a mixed-carrier, zero-config fleet primes and registers every line correctly. This is the MVP; stop and validate here before continuing.

---

## Phase 4: User Story 2 - Cache lost after a redeploy, mixed carriers (Priority: P2)

**Goal**: Prove that a wiped multi-line cache recovers automatically, per line, using the exact same mechanism US1 already built — no new production code, per research.md R2/R7 ("missing" and "never primed" are the same case to the three-tier check).

**Independent Test**: Prime a multi-line, mixed-carrier deployment successfully, delete every line's cache, restart, confirm every line re-captures independently; delete only some lines' caches and confirm only those re-prime (spec.md User Story 2).

### Implementation for User Story 2

- [X] T013 [US2] Same deviation as T012 (see its note). Added `every_line_needs_repriming_after_its_cache_is_wiped` to `src/supervise/orchestrate_volte.rs`'s test module: two lines primed, both caches deleted (simulating a redeploy), asserts `lines_needing_priming` puts both back in the needed set. The partial-wipe scenario (only one line's cache missing) is already covered by `adding_a_new_line_does_not_disturb_an_already_primed_ones_status`. No new production code was needed — confirms research.md R2/R7's prediction that this falls out of the three-tier check for free

**Checkpoint**: User Stories 1 and 2 both verified working.

---

## Phase 5: User Story 3 - Some lines already pinned, others not (Priority: P3)

**Goal**: Prove a pinned line is completely untouched by another line's priming need — the three-tier check's tier 1 (explicit override) always wins, independently per line, per research.md R2.

**Independent Test**: One line with an explicit `[[volte.line]].pcscf` override (or a valid pre-existing own-line cache) alongside a second, unconfigured line; confirm the first shows zero priming activity while the second is primed automatically (spec.md User Story 3).

### Implementation for User Story 3

- [X] T014 [US3] Same deviation as T012 (see its note). Already fully covered by `already_available_lines_are_excluded_mixed_carrier_fleet` (`src/supervise/orchestrate_volte.rs`): one line pinned via explicit override, one line with a valid pre-existing per-line cache, one line unconfigured — asserts only the unconfigured line appears in the needed set. No new production code was needed

**Checkpoint**: All three user stories independently verified.

---

## Phase 6: Polish & Cross-Cutting Concerns

- [X] T015 [P] Update `docs/operations.md`'s "It's automatic now (specs/080-volte-pcscf-auto-prime)" section to describe the per-line, `card_id`-keyed cache files this feature adds (cross-reference specs/081), and note the legacy shared-file fallback still applies for single-line deployments
- [X] T016 [P] Re-read `specs/081-multi-carrier-pcscf/quickstart.md` against the actual implemented behavior (function/flag names, log message wording) and correct any drift
- [X] T017 Full workspace gate: `make format && make lint && make test` — must be clean before any commit per the project's Pre-commit Checklist (CLAUDE.md)
- [X] T018 Ran what this session can safely exercise without root/CAP_NET_ADMIN or touching real carrier hardware (project memory: sandbox blocks root network testing; launching the privileged `test/` docker deployment against the host's real modems is a live-hardware action left for the user to trigger deliberately, not taken autonomously): the full workspace build and `make test` (T017) already exercise every new function's unit/mock-driven coverage, including config parsing for the unchanged `[volte]`/`[vowifi]` schema. Steps 2–9 of quickstart.md's "Trying it" (real `ip netns`, real charon/EAP-AKA against a live ePDG, real multi-carrier SIMs) remain deferred to a live rig pass, exactly as quickstart.md's own "Real-hardware validation still needed" section already states — this task does not change that; it confirms there is nothing further this sandbox can validate

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies
- **Foundational (Phase 2)**: Depends on Setup — BLOCKS all user stories
- **User Story 1 (Phase 3)**: Depends on Foundational — this is the entire mechanism; nothing else can start before it
- **User Story 2 (Phase 4)**: Depends on User Story 1 being complete (T012) — not independent of US1 the way a typical CRUD story would be, because US2 is a verification of US1's own design (research.md R2/R7), not new functionality
- **User Story 3 (Phase 5)**: Depends on User Story 1 being complete (T012) — same reasoning as US2; US2 and US3 can run in parallel with each other once US1 is done
- **Polish (Phase 6)**: Depends on US1–US3 all complete

### Within Foundational

- T002 and T003 are independent (different files, different concerns) — parallel
- T004 depends on T002 (needs `per_line_cache_path`) but not on T003

### Within User Story 1

- Strict chain: T005 → T006 → T007 → T008, and T009 depends on T006, T010 depends on T009
- T011 depends only on T004, so it can run alongside T005–T008
- T012 depends on T010 and T011 (needs the full wired path to drive end to end)

---

## Parallel Example: Foundational

```bash
# T002 and T003 touch different files and have no dependency on each other:
Task: "Add per_line_cache_path to src/volte/pcscf.rs"
Task: "Generalize resolve_single_line to take a pass-local index in src/vowifi/discovery.rs"
```

## Parallel Example: User Story 1

```bash
# T011 depends only on T004 (already done in Foundational), not on T005-T009:
Task: "Generalize ensure_pcscf_primed to be per-line in src/supervise/orchestrate_volte.rs"
# ...can run while T005-T008's orchestrate_prime.rs chain is in progress.
```

---

## Implementation Strategy

### MVP First (User Story 1 only)

1. Phase 1: Setup (T001)
2. Phase 2: Foundational (T002–T004) — CRITICAL, blocks everything
3. Phase 3: User Story 1 (T005–T012)
4. **STOP and VALIDATE**: run T012's integration test plus quickstart.md's
   steps 1–8 as far as the sandbox allows; this alone closes the gap the
   whole feature exists for
5. Continue to US2/US3 (verification-only phases) and Polish

### Incremental Delivery

1. Setup + Foundational → foundation ready, nothing user-visible yet
2. User Story 1 → the mixed-carrier gap is closed; this is the deliverable
3. User Story 2 → proves redeploy recovery needs no extra work (or fixes a
   gap T013 finds)
4. User Story 3 → proves pinned-line isolation needs no extra work (or
   fixes a gap T014 finds)
5. Polish → docs, full-workspace gate, quickstart re-validation

---

## Notes

- [P] tasks = different files, no dependency on an incomplete task
- Unit tests are colocated in the same `.rs` files as the code under test
  (existing convention); no new top-level `tests/` file was added — see
  T012's note for why
- Commit after each task or logical group, `make format && make lint &&
  make test` green before every commit (CLAUDE.md Pre-commit Checklist,
  constitution Principle II)
- No real phone numbers/IMSIs/ICCIDs in any new test fixture — use
  `card_id` values in the existing synthetic style (`ec20-AAAAAA`,
  `ec20-BBBBBB`, ...) already used throughout `discovery.rs`'s tests
