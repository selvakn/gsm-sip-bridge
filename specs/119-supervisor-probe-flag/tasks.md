# Tasks: Supervisor P-CSCF Probe Behind a Flag, Off by Default

**Input**: `specs/119-supervisor-probe-flag/` (spec.md, plan.md, research.md, data-model.md, contracts/, quickstart.md)
**Tests**: included. The constitution requires them and SC-007 names one.

Before EVERY commit: `make format && make lint && make test` (CLAUDE.md). One commit per phase checkpoint. Use only synthetic addresses (documentation ranges such as `192.0.2.x`, `2001:db8::`) in tests and docs.

Format: `[ID] [P?] [Story] Description`. `[P]` = different files, no dependency on an unfinished task.

## Phase 1: Setup

- [X] T001 Re-read the probe block in `gsm-sip-bridge/src/supervise/line_supervisor.rs` (`tick_steady_state_underlay`, from `let selection_reachable = engine.pcscf_reachable(...)` to the final `SteadyOutcome::StillUp`) and the three read sites of `pcscf_via_sibling` (`gsm-sip-bridge/src/supervise/orchestrate.rs`, `report_pcscf_sibling` and the two `via_sibling_before` loads). Confirm research R1, R3 and R4 hold (no other production caller of `pcscf_reachable`; nothing but a log line consumes the sibling). If either claim is false, stop and revise `research.md` and `plan.md` before coding.

## Phase 2: Foundational (blocks all stories)

- [X] T002 Add `supervisor_pcscf_probe: bool` (default `false`) to `VowifiConfig` in `gsm-sip-bridge/src/config/mod.rs` with a doc comment pointing at `specs/119-supervisor-probe-flag`, its `Default` impl (~line 1204 beside `idle_keepalive`), the raw TOML mirror in `gsm-sip-bridge/src/config/raw.rs` (struct field and its `Default`), and the copy in `gsm-sip-bridge/src/config/build.rs` (beside `idle_keepalive: raw.idle_keepalive`).
- [X] T003 [P] Config tests in `gsm-sip-bridge/src/config/mod.rs` beside `vowifi_idle_keepalive_is_off_by_default`: the probe is off in a minimal config, `supervisor_pcscf_probe = true` turns it on, and `supervisor_pcscf_probe = "yes"` fails loading with an error naming `supervisor_pcscf_probe` (FR-012).
- [X] T004 Add `probe_disabled: bool` to `SteadyTracking` in `gsm-sip-bridge/src/supervise/line_supervisor.rs` (derived `Default` stays `false`, data-model.md) with a doc comment saying it is the inverse of `[vowifi].supervisor_pcscf_probe` and why it is stored inverted (research R2). Fix any struct literal the compiler flags.

**Checkpoint**: `make format && make lint && make test` green; no behaviour change yet. Commit: `refactor(config): carry [vowifi].supervisor_pcscf_probe toward the line supervisor (#120)`.

## Phase 3: User Story 1 - A registered line stops making handset-unlike connections (P1) 🎯 MVP

**Goal**: with the default config the supervisor makes zero connects to a P-CSCF's :5060.
**Independent test**: tick the supervisor with `probe_disabled = true`; the runner records no connect.

- [X] T005 [US1] In `tick_steady_state_underlay` (`gsm-sip-bridge/src/supervise/line_supervisor.rs`) gate the probe block on `tracking.probe_disabled`: when set, assign `tracking.pcscf_via_sibling = None`, `tracking.pcscf_unreachable.reset()` and skip to the final `SteadyOutcome::StillUp`; when clear, leave the block exactly as it is (FR-004, research R3). Expand the comment above the block to say the probe is optional and what covers each of its former jobs.
- [X] T006 [US1] Set `probe_disabled: !ctx.config.vowifi.supervisor_pcscf_probe` where the strongSwan loop builds its `SteadyTracking` (`gsm-sip-bridge/src/supervise/orchestrate.rs`, ~line 1882) and where the SWu loop builds its own (~line 3350).
- [X] T007 [US1] Add the once-per-run startup line from `contracts/logs.md` beside the idle-keepalive and DPD lines in `gsm-sip-bridge/src/supervise/orchestrate.rs` (~line 220-245), via a `supervisor_probe_log_line(&VowifiConfig) -> String` helper like `idle_keepalive_log_line`. Make `idle_keepalive_log_line`'s off-state text name the new flag's current state instead of claiming the supervisor probes (research R6).
- [X] T008 [P] [US1] Tests in `gsm-sip-bridge/src/supervise/line_supervisor.rs` (use the existing `MockEngine`/runner in that module, whose `pcscf_reachable` is observable): with `probe_disabled = true` and the mock's reachability `false`, ten consecutive ticks return `StillUp` and the engine's reachability was never asked; `tracking.pcscf_unreachable` stays at its default; `pcscf_via_sibling` stays `None`, including when the tracking started with a dirty streak and a `Some` sibling (FR-003, FR-008). Also assert the same through the real `StrongswanEngine` and the command-runner fake in `gsm-sip-bridge/src/supervise/engines.rs`: no `tcp_connect_ok_in_netns` call is recorded across ticks (the strongSwan half of SC-007). For the SWu half, assert the flag-on/off call count against `SwuEngine` the same way.
- [X] T009 [P] [US1] Unit tests for the two log-line helpers in `gsm-sip-bridge/src/supervise/orchestrate.rs` (next to the existing `idle_keepalive_log_line` tests): off text names `supervisor_pcscf_probe = false`; on text names the port and `true`; the idle-keepalive off line no longer says the supervisor probes when the flag is off.

**Checkpoint**: `make format && make lint && make test`. Commit: `feat(supervise): gate the supervisor's P-CSCF probe behind [vowifi].supervisor_pcscf_probe, off by default (#120)`.

## Phase 4: User Story 2 - Recovery keeps working without the probe (P1)

**Goal**: route repair, the pool-exhaustion rebuild and the dead-peer behaviour are unchanged with the probe off.
**Independent test**: each fault injected against a `probe_disabled = true` tracking.

- [X] T010 [US2] Tests in `gsm-sip-bridge/src/supervise/line_supervisor.rs` with `probe_disabled = true` (reuse the fixtures of the existing route-repair and pool-exhaustion tests, copying their setup): (a) a missing default route is repaired on the first tick, for an IPv4 selection and an IPv6 selection, returns the repaired outcome and issues no rebuild (FR-005); (b) an agent pool with every member marked returns `Recovered { reason: PcscfPoolExhausted }` on one tick, and is deferred while a call is active (FR-006); (c) a single-P-CSCF selection with the mock unreachable never produces `PcscfUnreachable`, even over 100 ticks of simulated time.
- [X] T011 [P] [US2] (covered by construction: `render_epdg` takes no probe input, so there is nothing to vary; no test added) Confirm FR-007 by test: `render_epdg` output in `gsm-sip-bridge/src/commands/discover.rs` is byte-identical with the new option on and off (the option is not an input to rendering), next to the existing `dpd_delay` assertions.
- [X] T012 [US2] Add a test that the underlay guard path is unaffected: with `probe_disabled = true` and the underlay view holding, a pool-exhausted tick still returns `Held` exactly as the existing hold tests assert (`gsm-sip-bridge/src/supervise/line_supervisor.rs`, copy the nearest `Held` test).

**Checkpoint**: `make format && make lint && make test`. Commit: `test(supervise): recovery paths keep working with the P-CSCF probe off (#120)`.

## Phase 5: User Story 3 - An operator can switch the probe back on (P2)

**Goal**: flag on is today's behaviour, unchanged.
**Independent test**: the existing probe tests all still pass untouched, plus one flag-on test through the config path.

- [X] T013 [US3] Verify, without editing them, that every pre-existing probe test in `gsm-sip-bridge/src/supervise/line_supervisor.rs` (strikes, window, sibling selection, `PcscfUnreachable` rebuild, held rebuild) passes with `SteadyTracking::default()`; `git diff` of those tests must be empty (FR-004).
- [X] T014 [US3] (covered by `probe_disabled_is_the_inverse_of_the_config_flag` plus the engine flag-on/off tests) Add one test that builds `SteadyTracking` the way `orchestrate.rs` does from a loaded config with `supervisor_pcscf_probe = true` (extract the one-line `!config.vowifi.supervisor_pcscf_probe` into a small `probe_disabled(&VowifiConfig) -> bool` helper used by both loops if needed for testability) and ticks it with the mock unreachable: the probe is consulted and the streak counts (`gsm-sip-bridge/src/supervise/orchestrate.rs` tests).
- [X] T015 [P] [US3] Export contract: add `SUPERVISOR_PCSCF_PROBE` after `DPD_DELAY_SEC` in `render_vowifi_shell_env` (`gsm-sip-bridge/src/commands/config.rs`), and a test in `gsm-sip-bridge/tests/test_shell_env_contracts.rs` beside the idle-keepalive one asserting `DPD_DELAY_SEC='120'\nSUPERVISOR_PCSCF_PROBE='false'\n` by default and `'true'` when set. Check the existing DPD test's adjacency assertions still hold.

**Checkpoint**: `make format && make lint && make test`. Commit: `feat(config): export SUPERVISOR_PCSCF_PROBE and cover the probe-on path (#120)`.

## Phase 6: Documentation

- [X] T016 [P] `docs/configuration.md`: add the `supervisor_pcscf_probe` row from `contracts/config.md` after `idle_keepalive`; reword `idle_keepalive` ("the line supervisor already probes the P-CSCF every 30s" → "may probe, see `supervisor_pcscf_probe`") and `dpd_delay_sec` ("No effect while the line supervisor's P-CSCF probe runs" → "only takes effect with `supervisor_pcscf_probe` off, the default").
- [X] T017 [P] `config.toml.example`: add `# supervisor_pcscf_probe = false` with a short comment beside `idle_keepalive` (FR-013); fix its `idle_keepalive` comment which says the supervisor probes every 30s. Check `docs/vowifi-epdg-research-notes.md` line ~160 and `docs/observability.md` for the same claim and the `pcscf_unreachable` reason (reads zero by default now); update wording only where it states the probe always runs.
- [X] T018 [P] `CHANGELOG.md` under Unreleased: **Changed**, the supervisor's TCP probe to the P-CSCF's unprotected port is now off by default; `[vowifi].supervisor_pcscf_probe = true` restores it. State the visible consequences: strongSwan's own DPD now runs at `dpd_delay_sec`, and a dead single P-CSCF is detected through the agent in roughly 115s + poll instead of ~97s. Add the same note to `RELEASE_NOTES.md` in its established style (bold only the summary).

**Checkpoint**: `make format && make lint && make test`. Commit: `docs: supervisor_pcscf_probe and the default flip (#120)`.

## Phase 7: Live verification (needs the rigs)

Follow `quickstart.md`. Throwaway image tags; record every run's raw evidence (commands, log excerpts, timings) in `specs/119-supervisor-probe-flag/live-verification.md` with no real identifiers. A failed success criterion is a finding to fix or a reason to change the default, not something to paper over.

- [ ] T019 Build a throwaway image from `git archive HEAD` (not the working tree: `target-100/` is not `.dockerignored`) and deploy it to the local Vodafone rig with the default config. Check the startup line (US1 AS2) and run quickstart step 2, a one-hour capture (SC-001).
- [ ] T020 Vodafone, probe off: delete the default route (SC-003), then black-hole the sole P-CSCF (SC-004, 315s bound). Record the recovery reason and time.
- [ ] T021 Vodafone, probe on: repeat the black-hole run and confirm the `pcscf_unreachable` rebuild and timing match the pre-change build (SC-006).
- [ ] T022 Cross-build arm64 (`arm-build-200`) and deploy to the Jio Pi, recreating the container (not restarting). Repeat T019's capture and T020's route deletion; black-hole the current P-CSCF and confirm the agent's failover without a rebuild (SC-005, 193s).
- [ ] T023 4.5-hour idle soak, probe off, on both rigs: no teardown, registered throughout, zero false dead-peer losses (SC-002). Count DPD and NAT-T traffic against spec 098's soak figures.
- [ ] T024 Write up `specs/119-supervisor-probe-flag/live-verification.md` and mark the success criteria met or not. Commit: `docs(119): live verification results (#120)`.

## Phase 8: Polish

- [ ] T025 Run the full pre-commit checklist once more, run `/code-review` on the branch, address findings, and open the PR referencing #120 ("Part of", not "Closes", unless T024 shows everything in the issue is done). No reply comments to bot reviewers; fix the code.

## Dependencies

- T001 → everything. Phase 2 → Phases 3-5. T005 → T006, T008, T010, T012. T004 → T005. T006 and T014 share a helper if T014 extracts one; do T006 first.
- Phase 3 (MVP) is shippable alone; Phase 4 only adds tests and Phase 5 adds the export. Phase 6 can run beside Phases 4-5. Phase 7 needs Phases 2-6.

## Parallel opportunities

T003 with T004; T008 with T009; T011 with T010; T015 with T013-T014; T016, T017, T018 together. Live runs T019/T020 (Vodafone) and T022 (Jio) can overlap, since they use different rigs, but each rig runs one fault at a time.

## Implementation strategy

MVP = Phases 1-3: the flag exists, defaults off, and the probe is skipped. Then prove the recovery paths in tests (Phase 4), finish the export and the flag-on proof (Phase 5), docs (Phase 6), and only then spend rig time (Phase 7). If any live criterion fails, the default flips back to `true` in a one-line change and the rest stands.
