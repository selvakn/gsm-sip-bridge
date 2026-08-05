---

description: "Task list for 026-disable-circuit-switched"
---

# Tasks: Disable Circuit-Switched Handling

**Input**: Design documents from `specs/026-disable-circuit-switched/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/

**Tests**: Test tasks ARE included. The project constitution makes Integration-First Testing non-negotiable (Principle I) and TDD the default practice (Development Workflow). Every test task below exercises real components — no new mocks are introduced.

**Organization**: Grouped by user story so each is independently implementable and testable.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to (US1–US4)
- Exact file paths included in every task

## Path Conventions

Single Rust workspace. Source under `gsm-sip-bridge/src/`, integration tests under `gsm-sip-bridge/tests/`, docs at repo root under `docs/`.

---

## Phase 1: Setup

**Purpose**: Establish the baseline this feature must not regress

- [X] T001 Confirm baseline is green: run `make format`, `make lint`, `make test` from repo root and record that all three pass before any change
- [X] T002 [P] Capture the current circuit-switched metric series as a before-state fixture in `specs/026-disable-circuit-switched/contracts/metrics-baseline.txt` by scraping a running daemon (`curl -s localhost:9091/metrics | grep -E 'modules_|scheduled_restart'`), for the FR-021c "unchanged when enabled" comparison — no live daemon was available; derived instead from the `Lazy<...>` registrations in `metrics/mod.rs`, noted in the fixture

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: The `[cs].enabled` config value itself. Every user story reads it, so nothing else can start until this lands.

**⚠️ CRITICAL**: No user story work can begin until this phase is complete.

**⚠️ HIGHEST-RISK TASK IN THE FEATURE**: T004. The `section!` macro applies `#[serde(default)]`, so an absent `[cs]` section falls back to `Default`. Every neighbouring section is opt-in and correctly derives `false`. Deriving `Default` here would silently disable circuit switching for every existing deployment on upgrade. Write T003 first and watch it fail.

- [X] T003 Write failing config tests in `gsm-sip-bridge/tests/test_config.rs`: (a) a config with no `[cs]` section parses to `cs.enabled == true`; (b) `[cs]` present with `enabled` absent parses to `true`; (c) `[cs].enabled = false` parses to `false`; (d) `[cs].bogus = 1` is rejected naming `cs.bogus`; (e) all eight combinations of `[cs]`/`[vowifi]`/`[volte]` enabled flags load without error (FR-003). Confirm they FAIL.
- [X] T004 Add the `RawCs` section to `gsm-sip-bridge/src/config/raw.rs` using the `section!` macro with a single `pub enabled: bool`, and a **hand-written** `impl Default for RawCs` returning `enabled: true` — NOT `#[derive(Default)]`. Include a comment at the impl explaining why, per data-model.md.
- [X] T005 Add `pub cs: RawCs` to the `RawConfig` struct in `gsm-sip-bridge/src/config/raw.rs`
- [X] T006 Add `("cs", RawCs::KEYS)` to `section_key_lists()` in `gsm-sip-bridge/src/config/raw.rs` so `collect_unknown_keys` accepts the section (FR-002a)
- [X] T007 [P] Add `CsConfig { pub enabled: bool }` and the `pub cs: CsConfig` field on `AppConfig` in `gsm-sip-bridge/src/config/mod.rs`
- [X] T008 Add `build_cs` to `gsm-sip-bridge/src/config/build.rs` and wire `cs: build_cs(raw.cs)?` into `build()` (depends on T004, T007)
- [X] T009 [P] Document the section in `docs/configuration.md`: a `### \`[cs]\`` heading with an `` | `enabled` `` table row, plus a cross-reference in both directions between `[cs]` and `[modules]` (FR-004a, FR-027). Required — `tests/test_config_docs.rs` fails without it.
- [X] T010 [P] Add a `[cs]` block with `enabled = true` and an explanatory comment to `config.toml.example` (required by `tests/test_config_docs.rs`)
- [X] T011 Run `make test` and confirm T003's tests now pass and `test_config_docs.rs` is green

**Checkpoint**: `[cs].enabled` parses correctly and defaults to enabled. Nothing reads it yet — behaviour is unchanged.

---

## Phase 3: User Story 1 - Silence background modem probing (Priority: P1) 🎯 MVP

**Goal**: With `[cs].enabled = false`, no modem discovery, no periodic rescan, no AT traffic, no circuit-switched calls — while VoWiFi keeps working.

**Independent Test**: Start with the flag off and VoWiFi on; observe across several `[modules].retry_interval_sec` intervals that no discovery or modem-port access occurs, while an inbound and an outbound VoWiFi call complete normally.

### Tests for User Story 1

> Write these FIRST and confirm they FAIL.

- [X] T012 [P] [US1] Create `gsm-sip-bridge/tests/test_cs_disabled.rs` with a failing test asserting that with `cs.enabled = false` the daemon wiring does not construct or spawn a `CardPool`, and no modem discovery is invoked (FR-005, FR-006, FR-007, FR-008)
- [X] T013 [P] [US1] Add a failing test in `gsm-sip-bridge/tests/test_cs_disabled.rs` asserting the startup log reports the effective `[cs].enabled` value at a level visible without debug logging (FR-004)
- [X] T014 [P] [US1] Add a failing test in `gsm-sip-bridge/tests/test_card_pool.rs` asserting that with the flag ON the pool is constructed and discovery runs exactly as today (the no-regression half of FR-005–FR-008)

**Implementation note (T012–T014)**: `commands::daemon::run` blocks on a real OS signal and can't be driven from a test without hardware or signaling the whole test process, so `CardPool::run` itself was decomposed into a testable pure decision — `StartupPlan`/`plan_startup`/`log_startup_plan`/`apply_startup_metrics` in `commands/daemon.rs`, mirroring the seam `commands::healthcheck::evaluate` already uses for the identical reason. T012–T014 all landed in `tests/test_cs_disabled.rs` against this seam (T014's "flag ON" case included as `plan_startup_gates_circuit_switched_on_cs_enabled`) rather than one of them going into `test_card_pool.rs`, whose existing scope is `CardInstance`/`CardState` struct tests, not daemon-level wiring — a StartupPlan test there would be a non-sequitur. `run`'s own one-line `if plan.circuit_switched` is verified by type-checking and the manual hardware check in quickstart.md (T048), not by an automated test.

### Implementation for User Story 1

- [X] T015 [US1] Gate `CardPool::new` + `card_pool.run(...)` on `config.cs.enabled` in `gsm-sip-bridge/src/commands/daemon.rs`, leaving the metrics server, control server, and store initialisation untouched (FR-005–FR-009, FR-014, FR-016)
- [X] T016 [US1] Log the effective `[cs].enabled` value at startup in `gsm-sip-bridge/src/commands/daemon.rs`, alongside the existing "configuration loaded" line (FR-004)
- [X] T017 [US1] Run `make test`; confirm T012–T014 pass and no existing test regressed

**Checkpoint**: The flag silences the circuit-switched path. VoWiFi and VoLTE are unaffected.

**Known-gap note superseded**: the original plan deferred the control-channel responder to Phase 5 and expected card commands to *hang* in the interim. Building `daemon.rs`'s gate surfaced that this was never quite true — `control::server::handle_connection` already replies fast with a generic `"daemon shutting down"`/`"no response from daemon"` `Err` the moment the receiver is dropped or closed (`cmd_tx.send(...).await.is_err()`), not a hang. Since the disabled-command responder (`control::disabled`, originally T024/T025) is the same `daemon.rs` branch as this gate, it was implemented in this same pass rather than left as a real gap — see Phase 5 below, whose tasks are marked done here.

---

## Phase 4: User Story 2 - Existing deployments upgrade unchanged (Priority: P1)

**Goal**: A configuration written before this feature behaves identically after upgrade.

**Independent Test**: Run existing production configurations verbatim against the new build and confirm identical circuit-switched discovery, call bridging, and message forwarding.

### Tests for User Story 2

- [X] T018 [P] [US2] Add a regression test in `gsm-sip-bridge/tests/test_config.rs` that loads every file in `sample_configs/` and `config.toml.example` and asserts each yields `cs.enabled == true` unless it explicitly sets the flag (FR-002, User Story 2) — `every_shipped_sample_config_defaults_circuit_switching_to_enabled` iterates the real `sample_configs/*.toml` files at test time (not fabricated fixtures); `config.toml.example` itself already has standing coverage via `test_config_docs.rs`/`test_the_shipped_example_config_still_loads`
- [X] T019 [P] [US2] Add a test in `gsm-sip-bridge/tests/test_cs_disabled.rs` asserting that with the flag on (explicitly, and by omission) and VoWiFi also on, both subsystems initialise together exactly as they do today — `plan_startup_runs_circuit_switched_alongside_vowifi_when_both_enabled`

### Implementation for User Story 2

- [X] T020 [US2] Verify no behaviour change with the flag defaulted: run `make test` and confirm the full existing suite passes with no edits to pre-existing test expectations. Any pre-existing test that needs modification is a signal the default is wrong — stop and re-check T004. No pre-existing test's *expected value* was changed anywhere in this feature; the only edits to existing tests were mechanical call-site updates forced by `evaluate()` gaining a new parameter (`healthcheck.rs`), which is confirmed safe since every such call still passes `true` for the new `cs_enabled` argument, reproducing prior behaviour exactly.

**Checkpoint**: Upgrade safety proven. Combined with US1, the flag is functional and backward compatible.

---

## Phase 5: User Story 3 - Coherent health, metrics, and commands (Priority: P2)

**Goal**: With the path off, health checks, metrics, and card commands say "disabled" clearly instead of erroring, hanging, or looking broken.

**Independent Test**: With the flag off, run the health check and each card command and scrape metrics; confirm each communicates "disabled" unambiguously and none reports a fault.

### Tests for User Story 3

- [X] T021 [P] [US3] Add failing tests in `gsm-sip-bridge/tests/test_cs_disabled.rs` for control-socket behaviour with the path off: `ListSlots`, `CardRestart`, `SetMode`, and `GetMode` each return `ControlResp::Err` whose message names `[cs].enabled`. Assert on the returned reply, not on absence of a hang (contracts/control-protocol.md)
- [X] T022 [P] [US3] Add a failing test in `gsm-sip-bridge/tests/test_cs_disabled.rs` asserting an `Observe` report still reaches `metrics::ingest` with the path off, so VoWiFi/VoLTE metrics keep flowing (FR-014, FR-015) — exercised end to end through a real control socket and the real `disabled::run` responder (`observe_reports_still_reach_metrics_ingest_with_the_responder_running`), not just `apply_report` called directly
- [X] T023 [P] [US3] Add failing tests in `gsm-sip-bridge/tests/test_metrics_endpoint.rs`: with the path off, a scrape contains `gsm_sip_bridge_cs_enabled 0` and contains **none** of `modules_active`, `modules_failed`, `module_init_total`, `module_retries_total`, `scheduled_restart_total`; with the path on, it contains `cs_enabled 1` and all of them (FR-021a, FR-021b, FR-021c). Landed as one combined test in `test_cs_disabled.rs` (`apply_startup_metrics_sets_the_gauge_and_leaves_cs_series_unregistered`) rather than two separate tests in `test_metrics_endpoint.rs` — `CS_ENABLED` is process-global state and two independent set-then-scrape tests would race under parallel execution; one sequential function has no such window. `test_metrics_endpoint.rs`'s own `test_all_metrics_registered` was extended with `gsm_sip_bridge_cs_enabled` for the FR-021c "unchanged when enabled" side.

### Implementation for User Story 3

- [X] T024 [US3] Create `gsm-sip-bridge/src/control/disabled.rs` with a responder task that drains the control-command receiver and answers `CardRestart`, `SetMode`, `GetMode`, and `ListSlots` with `ControlResp::Err` naming `[cs].enabled`; register the module in `gsm-sip-bridge/src/control/mod.rs` (FR-019, FR-020)
- [X] T025 [US3] Spawn the disabled responder from the else-branch of the gate in `gsm-sip-bridge/src/commands/daemon.rs`, so the control receiver always has exactly one consumer (depends on T015, T024) — implemented as `pool_handle`'s two branches both yielding a `JoinHandle<()>` rather than an `Option`, since either branch always produces one
- [X] T026 [P] [US3] Add the `CS_ENABLED` gauge (`gsm_sip_bridge_cs_enabled`) to `gsm-sip-bridge/src/metrics/mod.rs` per contracts/metrics-contract.md
- [X] T027 [US3] Set `CS_ENABLED` unconditionally in `gsm-sip-bridge/src/commands/daemon.rs` before the gate, so it is present in both states (FR-021b; depends on T026) — via `apply_startup_metrics`, part of the same `StartupPlan` seam as T015/T016
- [X] T028 [P] [US3] Report the circuit-switched path as intentionally disabled — not unhealthy or degraded — in `gsm-sip-bridge/src/commands/healthcheck.rs`, threading `config.cs.enabled` into `evaluate` (FR-018) — added `Health::CircuitSwitchedDisabled` (exits `SUCCESS`, `is_healthy() == true`), returned only when there is nothing else to check (mirrors the existing `!vowifi_enabled || resolution.lines.is_empty()` branch); real VoWiFi line checks are unaffected and still run when VoWiFi is actually carrying traffic
- [X] T029 [US3] Run `make test`; confirm T021–T023 pass

**Checkpoint**: The operational surface is coherent. **US1 + US2 + US3 is the minimum production-shippable set.**

---

## Phase 6: User Story 4 - Reuse circuit-switched hardware for VoWiFi (Priority: P3)

**Goal**: With the path off, voice-capable modems stop being reserved for it and become VoWiFi candidates.

**Independent Test**: On a system whose only modem is voice-capable with no explicit override, turn the flag off, restart, and confirm the modem resolves as a VoWiFi line and carries a call.

### Tests for User Story 4

- [X] T030 [P] [US4] Add failing tests in `gsm-sip-bridge/tests/test_discovery.rs`: with `cs_enabled = false`, `RoleAssignment::from_probed` puts a voice-capable modem with no override into `vowifi` and leaves `circuit_switched` empty; with `cs_enabled = true`, the existing partition is unchanged (FR-010a, FR-010c) — landed in `vowifi/discovery.rs`'s own `#[cfg(test)]` module (`role_assignment_offers_every_modem_to_vowifi_when_cs_is_disabled`, `role_assignment_default_splits_by_audio_when_cs_enabled`), alongside every other `RoleAssignment`/`from_probed` test, rather than `test_discovery.rs` (which only covers `derive_module_id` and has no access to the module-private `ready_modem`/`unusable_modem` fixtures `from_probed`'s existing tests already share)
- [X] T031 [P] [US4] Add a failing test in `gsm-sip-bridge/tests/test_vowifi_lines.rs` asserting the readiness filter and `max_lines` bound still apply to the newly freed candidates, and that excess candidates are dropped without error (FR-010b, and the "More candidates than lines allowed" edge case) — this file existed only as an unpopulated header comment; gave it real content chaining the real `RoleAssignment::from_probed` into the real `resolve_lines` through the public API, plus a second test for the "freed but unusable" edge case

### Implementation for User Story 4

- [X] T032 [US4] Add a `cs_enabled: bool` parameter to `RoleAssignment::from_probed` in `gsm-sip-bridge/src/vowifi/discovery.rs`; when false, every successfully probed modem goes to `vowifi` (FR-010a)
- [X] T033 [US4] Pass `config.cs.enabled` at the `from_probed` call site in `gsm-sip-bridge/src/commands/discover.rs` (depends on T032)
- [X] T034 [US4] Update the existing `from_probed` unit tests in `gsm-sip-bridge/src/vowifi/discovery.rs` to pass `true`, preserving their current expectations (FR-010c)
- [X] T035 [US4] Run `make test`; confirm T030–T031 pass and no VoWiFi line-resolution test regressed

**Checkpoint**: All four user stories functional.

---

## Phase 7: Cross-Cutting Requirements

**Purpose**: Requirements that belong to no single story but are part of the feature's contract. Each is independently testable.

- [X] T036 [P] Add failing tests in `gsm-sip-bridge/tests/test_sip_registration.rs`: with `cs.enabled = false` and no VoWiFi/VoLTE, `SipBridge::register` establishes no trunk registration and starts no registrar; with the flag on, behaviour is unchanged (FR-009a, FR-009c)
- [X] T037 Extend `owns_sip_side` in `gsm-sip-bridge/src/sip/mod.rs` with `&& config.cs.enabled`, reusing the existing early-return suppression rather than adding a second mechanism (FR-009a; depends on T036) — `register_trunk` gets the same term, since a `false` `owns_sip_side` already short-circuits before `register_trunk` is consulted but the field must still reflect the invariant on its own
- [X] T038 Extend the existing skip log in `gsm-sip-bridge/src/sip/mod.rs` to name `[cs].enabled` as the reason when that is what suppressed the telephone-facing side (FR-009b) — added a `cs_enabled` field to `SipBridgeConfig` purely so the skip log can distinguish "flag off" from "VoLTE/VoWiFi owns it" rather than always blaming the latter
- [X] T039 [P] Add a failing test then emit a prominent startup warning in `gsm-sip-bridge/src/commands/daemon.rs` when the flag is off and neither VoWiFi nor VoLTE is enabled: no call path is active, metrics and stored history only, no telephone-facing registration. Must NOT be fatal (FR-023) — delivered as part of Phase 3's `StartupPlan`/`plan_startup`/`log_startup_plan` (`warn_no_call_path`), tested in `test_cs_disabled.rs`
- [X] T040 [P] Add a failing test then emit a prominent startup warning in `gsm-sip-bridge/src/commands/daemon.rs` when the flag is off, `[sms].enabled` is true, and no VoWiFi/VoLTE line is configured (FR-024) — same seam, `warn_sms_orphaned`
- [X] T041 [P] Add a failing test then handle the single-card CLI override conflict in `gsm-sip-bridge/src/commands/daemon.rs`: `--serial`/`--audio` with the flag off must report the conflict and honour the flag, not resurrect the path (FR-026) — same seam, `warn_cli_override_ignored`; `plan_startup_warns_when_cli_override_given_with_cs_disabled` also asserts the override does not flip `circuit_switched` back on
- [X] T042 Add a test in `gsm-sip-bridge/tests/test_config.rs` asserting `[modules]`, `[resilience]`, `[scheduled_restart]`, and `[modem_audio]` still parse and validate normally with the flag off (FR-025) — delivered in Phase 2 as `cs_disabled_leaves_related_sections_valid`

---

## Phase 8: Polish & Documentation

- [X] T043 [P] Document the two non-obvious consequences in `docs/configuration.md`: no telephone-facing registration from the circuit-switched host (FR-009a), and voice-capable modems becoming available to VoWiFi (FR-010a) — neither is predictable from the flag's name (FR-028)
- [X] T044 [P] Document in `docs/configuration.md` (or the metrics reference) the `gsm_sip_bridge_cs_enabled` gauge and exactly which circuit-switched series disappear when the flag is off, so operators can adjust dashboards and alert rules before flipping it (FR-029) — folded into the `[cs]` section's own "Metrics" paragraph rather than a separate reference file, since none exists yet in this repo
- [X] T045 [P] Add a `[cs].enabled = false` scenario config to `sample_configs/` for a VoWiFi-only deployment — `sample_configs/vowifi-only-cs-disabled.toml`, registered in `sample_configs/README.md`'s call-path table
- [X] T046 [P] Add a `RELEASE_NOTES.md` entry flagging the trunk-registration change as the upgrade-visible behaviour — an operator who sets the flag on a client-mode box will see their PBX mark the trunk down, and that must not be a surprise
- [X] T047 Walk `specs/026-disable-circuit-switched/quickstart.md` end to end against a real build and correct anything that does not match observed behaviour — verified the `card list` error text against `control::disabled::DISABLED_REASON`/`commands/card.rs::print_resp`, the metrics grep output against the real encoder, and the `cargo test` filter against the actual test names; corrected one inaccuracy found in the process — the "every other enabled flag derives false" claim was wrong, `RawSms` already hand-writes `enabled: true` for the identical reason `RawCs` now does
- [ ] T048 Verify SC-001 on hardware: run a VoWiFi-only deployment with the flag off and confirm zero modem-probe operations across a multi-interval window — **not performed**: this sandboxed environment has no EC20/EC25 hardware and no root/CAP_NET_ADMIN for real modem or network-namespace access (see the `sandbox-blocks-root-network-testing` memory from prior sessions on this project). Left for the operator to run per `quickstart.md`'s verification steps on real hardware before shipping.
- [X] T049 Final gate: `make format`, `make lint`, `make test` all green from repo root — confirmed: `make format` made no changes, `make lint` clean (identical pre-existing `cargo-deny` license/duplicate-version warnings as the Phase 1 baseline, nothing new), `make test` 920+ passed / 0 failed across every workspace crate and every integration test binary

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: no dependencies
- **Foundational (Phase 2)**: depends on Setup — **BLOCKS every user story**
- **US1 (Phase 3)**: depends on Phase 2
- **US2 (Phase 4)**: depends on Phase 2; verification is strongest after US1
- **US3 (Phase 5)**: depends on Phase 2 and on T015 (the gate's else-branch is where the responder is spawned)
- **US4 (Phase 6)**: depends on Phase 2 only — fully independent of US1–US3
- **Cross-Cutting (Phase 7)**: depends on Phase 2; T037 independent of all stories
- **Polish (Phase 8)**: depends on everything above

### Critical Path

```text
T003 → T004 → T005/T006 → T008 → T011 → T015 → T025 → T049
       (default-true, highest risk)   (gate)  (responder)
```

### Within Each Story

- Tests written and failing before implementation
- Config plumbing before any consumer
- `make test` green before the next phase

### Parallel Opportunities

- T007, T009, T010 after T004–T006 (different files)
- T012, T013, T014 together (test authoring)
- T021, T022, T023 together
- T030, T031 together
- T036, T039, T040, T041, T042 largely independent
- All of Phase 8 except T047–T049
- **US4 (Phase 6) can be developed entirely in parallel with US1–US3** — it touches only `vowifi/discovery.rs` and `commands/discover.rs`, which no other phase modifies

---

## Parallel Example: User Story 3

```bash
# Author all three test groups together (different files, no shared state):
Task: "Control-socket disabled responses in gsm-sip-bridge/tests/test_cs_disabled.rs"
Task: "Observe still reaches metrics::ingest in gsm-sip-bridge/tests/test_cs_disabled.rs"
Task: "CS series absent + cs_enabled gauge in gsm-sip-bridge/tests/test_metrics_endpoint.rs"

# Then the two independent implementation strands:
Task: "CS_ENABLED gauge in gsm-sip-bridge/src/metrics/mod.rs"
Task: "Healthcheck disabled reporting in gsm-sip-bridge/src/commands/healthcheck.rs"
```

---

## Implementation Strategy

### MVP (User Story 1)

1. Phase 1: Setup
2. Phase 2: Foundational — **T004 is the one to get right**
3. Phase 3: User Story 1
4. **STOP and VALIDATE**: flag off, VoWiFi on, confirm zero probing and working calls

Demo-ready, but see the Phase 3 gap note before deploying.

### Minimum shippable

US1 + US2 + US3 (Phases 1–5). US2 proves nothing regressed for existing deployments; US3 closes the hanging-control-command gap. Phase 7's FR-009a work should land alongside if any target deployment runs client mode.

### Incremental delivery

1. Setup + Foundational → flag parses, nothing reads it
2. + US1 → probing stops (demo)
3. + US2 → upgrade safety proven
4. + US3 → shippable
5. + US4 → hardware reuse
6. + Phase 7 → full requirement coverage
7. + Phase 8 → documented and hardware-verified

---

## Notes

- Commit after each task or logical group; every commit must be green (Constitution II, III)
- `make lint` covers all test targets — a warning in a test file fails the build
- No new mocks: every test above drives real config parsing, real daemon wiring, a real metrics registry, or the real role-assignment function (Constitution I)
- If exactly one test fails and it is the default-value test, re-read T004's warning
