---

description: "Task list for Discovery Retry & Missing-Line Health Reporting"

---

# Tasks: Discovery Retry & Missing-Line Health Reporting

**Input**: Design documents from `/specs/027-discover-retry-health/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md (all present)

**Tests**: Included as first-class tasks (not optional) — the project constitution (`.specify/memory/constitution.md`, Principles I and the Development Workflow section) makes Integration-First Testing and TDD the default practice, and `CLAUDE.md`'s pre-commit checklist requires `make test` green before every commit.

**Organization**: Tasks are grouped by user story (spec.md priorities P1/P2/P3) so each is independently implementable, testable, and deliverable.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependency on an incomplete task)
- **[Story]**: Which user story this task belongs to (US1/US2/US3) — omitted for Setup, Foundational, and Polish tasks
- File paths are exact and relative to the repository root

---

## Phase 1: Setup

- [X] T001 Confirm `make test` passes cleanly on branch `027-discover-retry-health` before any change lands — establishes the Green-on-Commit baseline (Constitution Principle II) this feature's tasks build on.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: A "configured VoWiFi/VoLTE line with no matching probed hardware" must be detectable and recorded as a `FailedLine` before any of US1 (retry it), US2 (show it), or US3 (alert on it) has anything to build on.

**⚠️ CRITICAL**: No user story task below can start until this phase is complete.

- [X] T002 [P] Add failing tests in `gsm-sip-bridge/tests/test_discovery.rs` asserting: a configured `modem_port` override absent from the full probed-modem list (i.e. not just `RoleAssignment.vowifi`, per research.md R1) produces a `FailedLine{card_id: <port>, reason: "not_found"}`; a `modem_serial` override behaves the same by serial; a `pcsc_reader` override absent produces one identified by its existing synthetic `pcscN` id (data-model.md); an override that *is* matched produces nothing; an unpinned/auto-discovered absence produces nothing (only explicitly configured overrides count, per FR-001). These must fail to compile/pass until T003-T005 land.
- [X] T003 [P] Add `Rejection::NotFound` variant and `"not_found"` reason string to the `Rejection` enum in `gsm-sip-bridge/src/line/mod.rs` (alongside `SimAbsent`/`SimLocked`/`SimUnreadable`/`NoAtPort`/`MaxLinesExceeded`), with a unit test for the reason string.
- [X] T004 [US: shared] In `gsm-sip-bridge/src/vowifi/discovery.rs`, add a function that, given `base.line_overrides` and the full probed-modem list (every `ProbedModem` `scan_all_preferring` returned, including AT-failed ones — not the post-`RoleAssignment::from_probed` filtered subset), returns a `Vec<FailedLine>` using `Rejection::NotFound` for every configured override matched by neither `modem_port` nor `modem_serial` to any probed device, and for every `pcsc_reader` override that's unconditionally "configured" (a pcsc line has no USB match to check, so treat it as a placeholder for T005/orchestrate-level detection — see task notes). Reuse the existing serial/port matching logic `is_overridden_to_vowifi`/`override_for` already implement in this file rather than duplicating it.
- [X] T005 In `gsm-sip-bridge/src/commands/discover.rs`'s `handle_discover_command`, thread the full probed-modem list (already available from `scan_all_preferring`'s return value, before `RoleAssignment::from_probed` filters it) into T004's function, and merge its output into the `LineTableResult`/`LineResolution` `failed` list that gets written to disk — so every `discover` invocation (not just a future retry) reports a missing configured override immediately. Confirm T002's tests now pass.

**Checkpoint**: `gsm-sip-bridge discover` now detects and records (but does not yet retry or surface downstream) a configured line with no matching hardware.

---

## Phase 3: User Story 1 - A slow-enumerating configured modem still comes up on its own (Priority: P1) 🎯 MVP

**Goal**: A configured line whose hardware wasn't visible on the first `discover` pass resolves and starts automatically if it becomes available within a bounded window — no restart needed — without delaying the circuit-switched daemon or any already-successful line.

**Independent Test**: Per spec.md — configure a line pinned to a modem, start the system while that modem is deliberately made to enumerate a short time after container start, and confirm the line comes up and starts registering without any manual intervention.

### Tests for User Story 1

- [X] T006 [P] [US1] Add failing tests (`gsm-sip-bridge/tests/test_discovery.rs` or a new `test_discover_retry.rs`) for the retry *decision* logic in isolation: given a first probe result missing a configured override and a second probe result including it, the override resolves on the second attempt into a `ResolvedLine` with the correct identity; a sibling line already resolved on the first attempt keeps the same index/identity across both attempts (not disturbed by the retry).

### Implementation for User Story 1

- [X] T007 [US1] Add `DISCOVER_RETRY_WINDOW` (`Duration::from_secs(180)`) and `DISCOVER_RETRY_POLL_INTERVAL` (`Duration::from_secs(10)`) consts in `gsm-sip-bridge/src/supervise/orchestrate.rs`, alongside the file's existing `ESTABLISH_POLL_INTERVAL`/`STEADY_STATE_POLL_INTERVAL` consts. (Window/interval values per research.md's "on-the-order-of-minutes" assumption — 3 minutes bounds ordinary USB enumeration delay without leaving a genuinely-missing device retrying for long.)
- [X] T008 [US1] Extract the per-line VoWiFi startup logic (today's initial loop over `vowifi_lines` in `orchestrate.rs` section 3, ~line 274 onward) into a function taking one `LineResolutionEntry` at a time, callable both from that initial loop and, later, from the retry loop's success path (T010) — so a late-resolved line starts without restarting anything else (FR-004).
- [X] T009 [US1] In `orchestrate.rs`, after the first `discover` pass and after sections 2/3 (circuit-switched daemon + every initially-resolved line) have already started — unchanged timing, so SC-005 holds — spawn a background thread that tracks each still-missing configured override from the first pass's `failed` list (T005) with its own start time.
- [X] T010 [US1] In that background thread, on `DISCOVER_RETRY_POLL_INTERVAL`, re-probe only the still-missing overrides — reusing `modules::discovery::scan_all_inner`'s existing `skip_card_ids` exclusion (today only exercised by `scan_modules`'s ongoing rescans) seeded with every already-resolved line's `card_id`, so an already-running line's serial port is never reopened or re-probed (FR-005). For any override that now resolves: update the on-disk resolution file (add it to `lines`, remove its `failed` entry) and call T008's per-line startup function for it.
- [X] T011 [US1] Bound the retry loop by `DISCOVER_RETRY_WINDOW` per override: once elapsed without success, stop retrying that one, write/confirm its terminal `FailedLine{reason: "not_found", ..}` in the resolution file, and leave it there for the rest of this process's life (no further retries — startup-only scope per spec.md's Clarifications).
- [X] T012 [P] [US1] Add an integration test (`gsm-sip-bridge/tests/test_discovery.rs` or a dedicated file) covering spec.md User Story 1's three acceptance scenarios end-to-end: (1) a late-appearing configured modem resolves without a restart, (2) a first-pass-successful modem's resolution is byte-identical to today (no added delay), (3) with two configured lines where only one is late, the immediately-available one starts and operates without waiting on the late one.

**Checkpoint**: User Story 1 is fully functional and independently testable/demoable — run `make test` and the relevant scenario from `quickstart.md` step "Verifying the fix once implemented" #1-2.

---

## Phase 4: User Story 2 - Operators can see, at a glance, which configured lines aren't actually running (Priority: P2)

**Goal**: `vowifi-status` and the container `healthcheck` both surface a configured line that's still failed after its retry window, distinct from "not configured" and from "healthy" — without flagging a line still inside its retry window as broken.

**Independent Test**: Per spec.md — configure a line whose hardware is deliberately never made available, start the system, and confirm both `vowifi-status` and `healthcheck` report that specific configured line as failed/missing.

### Tests for User Story 2

- [X] T013 [P] [US2] Add failing tests in `gsm-sip-bridge/tests/test_cli.rs` for `vowifi-status`'s new output section per `contracts/vowifi-status-output.md`: a resolution with a terminal `not_found` entry prints `Configured line <id> (from config.toml): NOT RUNNING` / `reason: not_found`; a `sim_absent`/`sim_locked`/`sim_unreadable`/`no_at_port` entry is worded distinctly per its own reason (FR-007); a `max_lines_exceeded` entry (an unpinned candidate losing out on a scarce slot, not a configured line failing) is excluded from this section; a resolution with no `failed` configured-line entries prints byte-identical output to today.
- [X] T014 [P] [US2] Add failing tests for `healthcheck::evaluate` (in `gsm-sip-bridge/src/commands/healthcheck.rs`'s existing `#[cfg(test)]` module) per `contracts/healthcheck-contract.md`: one healthy resolved line plus a terminal configured-line `not_found` failure → unhealthy (not `Health::Healthy`); zero resolved lines but `[cs].enabled = true` and no configured-line failure → unchanged `Health::Healthy`; existing `MetricsEndpointDown`/`LinesUnhealthy`/`CircuitSwitchedDisabled` cases unchanged.

### Implementation for User Story 2

- [X] T015 [US2] Implement the new section in `gsm-sip-bridge/src/vowifi/mod.rs`'s `print_status`: after the existing per-resolved-line loop, read the resolution file's `failed` list and print each configured-line entry per `contracts/vowifi-status-output.md`, filtering out `max_lines_exceeded` (not a configured-line failure). Confirm T013 passes.
- [X] T016 [US2] Implement the change in `gsm-sip-bridge/src/commands/healthcheck.rs`'s `evaluate()`: inspect `resolution.failed` for configured-line `not_found` entries and fold them into the unhealthy result (extending the existing `Health::LinesUnhealthy` fault list, per `contracts/healthcheck-contract.md`) alongside the existing per-resolved-line fault check at line ~189-193. Confirm T014 passes.

**Checkpoint**: User Story 2 is fully functional and independently testable/demoable — run `make test` and `quickstart.md`'s "Verifying the fix" #3 (vowifi-status / docker ps / healthcheck bullets).

---

## Phase 5: User Story 3 - The existing alert channel fires for a missing configured line (Priority: P3)

**Goal**: A Discord notification (and a matching Prometheus metric) fires once when a configured line's retry window elapses, and a paired recovery notification fires if it later self-heals — mirroring the existing `registration_loss`/`tunnel_failure` failure/recovered pattern.

**Independent Test**: Per spec.md — configure a line whose hardware never becomes discoverable, start the system, and confirm an alert notification is sent once the failure is confirmed (after retries are exhausted), without needing anyone to check status manually.

### Tests for User Story 3

- [X] T017 [P] [US3] Add failing tests in `gsm-sip-bridge/tests/test_ingest_critical_alerts.rs` (or a new sibling file mirroring its structure) per `contracts/metrics-and-alerts-contract.md`: retry-window-elapsed dispatches exactly one `Failure` notification for the `line_discovery_failed` category (SC-004); a line that resolves *during* the window never triggers one (FR-010); a line that resolves *after* a failure notification already fired triggers exactly one `Recovered` notification (FR-011); the category disabled in config sends nothing.
- [X] T018 [P] [US3] Add a failing test in `gsm-sip-bridge/tests/test_metrics_endpoint.rs` for `gsm_sip_bridge_vowifi_line_discovery_failed`'s presence, labels (`module`), and value transitions (absent → `1` on terminal failure → cleared/`0` on recovery) per `contracts/metrics-and-alerts-contract.md`.

### Implementation for User Story 3

- [X] T019 [P] [US3] Add `AlertCategory::LineDiscoveryFailed` (stable string `"line_discovery_failed"`) in `gsm-sip-bridge/src/alerts/mod.rs`, following the existing `RegistrationLoss`/`TunnelFailure` variants' pattern exactly, including any match arms that resolve a category to its `CategoryAlertConfig`.
- [X] T020 [P] [US3] Add `AlertsConfig.line_discovery_failed: CategoryAlertConfig` in `gsm-sip-bridge/src/config/mod.rs`, defaulting to `CategoryAlertConfig::disabled()` per the existing `Default for AlertsConfig` pattern; extend config-loading/doc tests (`gsm-sip-bridge/tests/test_config.rs`/`test_config_docs.rs`) the same way `registration_loss`/`tunnel_failure` are already covered there.
- [X] T021 [P] [US3] Add `VOWIFI_LINE_DISCOVERY_FAILED: Lazy<GaugeVec>` in `gsm-sip-bridge/src/metrics/mod.rs` (name `gsm_sip_bridge_vowifi_line_discovery_failed`, label `["module"]`), alongside `VOWIFI_REGISTERED`/`VOWIFI_TUNNEL_UP`.
- [X] T022 [US3] In the retry loop's terminal-failure path (US1's T011), dispatch the `Failure` alert for `AlertCategory::LineDiscoveryFailed` (via the existing `alerts::discord::DiscordClient`, respecting T020's enabled/webhook config) and set T021's metric to `1`, labeled with that override's identifier. Confirm T017/T018's failure-path assertions pass.
- [X] T023 [US3] In the retry loop's success path (US1's T010), if a `Failure` alert was already sent for that override's identifier this process lifetime, dispatch the matching `Recovered` alert and clear T021's metric for it. Confirm T017/T018's recovery-path assertions pass.

**Checkpoint**: User Story 3 is fully functional and independently testable/demoable — run `make test` and `quickstart.md`'s "Verifying the fix" #3 (metrics/Discord bullets).

---

## Phase 6: Polish & Cross-Cutting Concerns

- [X] T024 [P] Document the new failure mode and a short runbook entry (what `NOT RUNNING`/`reason: not_found` means, what to check) in `docs/operations.md`, alongside its existing adjacent note on the XFRM/USB-enumeration startup race this feature fixes.
- [X] T025 [P] Document `[alerts.line_discovery_failed]` in `config.toml.example`/`sample_configs/`, consistent with how `registration_loss`/`tunnel_failure` are already documented there.
- [~] T026 Run `specs/027-discover-retry-health/quickstart.md`'s manual verification steps end-to-end against a real or simulated slow/absent modem, then `make format && make lint && make test` (full workspace gate, per `CLAUDE.md`'s mandatory pre-commit checklist) before any commit. **`make format`/`make lint`/`make test` done and green after every commit in this feature.** The manual quickstart.md hardware verification was **deliberately not run** during implementation: the only available modem/Docker environment is a live production container currently bridging real calls, and rebuilding/restarting it to test unreleased code is a disruptive action outside this task's authorization — left as a follow-up for the operator to run deliberately before/after merging.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies.
- **Foundational (Phase 2)**: Depends on Setup. **Blocks all user stories** — T004/T005's "detect + record `NotFound`" is the shared input every story reads or extends.
- **User Story 1 (Phase 3)**: Depends on Foundational only.
- **User Story 2 (Phase 4)**: Depends on Foundational only (reads whatever `failed` entries exist — from T005 alone, or additionally from US1's retry loop once that also exists; independently testable either way since T005 alone already produces a terminal-shaped `not_found` entry on a single pass).
- **User Story 3 (Phase 5)**: Depends on Foundational for the failure shape, and on US1's retry loop (T009-T011) existing as the trigger point for T022/T023 — the alert/metric fire *from* the retry loop's terminal/success paths, so build after US1.
- **Polish (Phase 6)**: Depends on whichever of US1/US2/US3 are in scope for the release.

### User Story Dependencies

- **US1 (P1)**: Independent after Foundational — this is the MVP.
- **US2 (P2)**: Independent after Foundational — can be built and demoed even before US1 exists (a single `discover` pass already produces a `not_found` entry per T005), though it becomes materially more useful once US1's retry exists (fewer false "NOT RUNNING" reports from transient races).
- **US3 (P3)**: Needs US1's retry loop as its trigger point (T022/T023 hook into T011/T010) — build after US1.

### Within Each User Story

- Tests before implementation (T002 before T003-T005; T006 before T007-T012; T013/T014 before T015/T016; T017/T018 before T019-T023).
- US1: consts (T007) and the extracted per-line-start function (T008) before the retry threads that use them (T009-T011).
- US3: the new `AlertCategory`/config/metric plumbing (T019-T021, parallelizable) before wiring them into the retry loop (T022-T023).

### Parallel Opportunities

- T002 and T003 (different files: a test file and `line/mod.rs`).
- T013 and T014 (different files: `test_cli.rs` and `healthcheck.rs`'s test module).
- T017 and T018 (different test files).
- T019, T020, T021 (three different files: `alerts/mod.rs`, `config/mod.rs`, `metrics/mod.rs`) — all parallelizable once Foundational and US1's retry loop exist.
- T024 and T025 (different doc files).

---

## Parallel Example: Foundational Phase

```bash
Task: "Add failing tests in gsm-sip-bridge/tests/test_discovery.rs for NotFound detection (T002)"
Task: "Add Rejection::NotFound variant in gsm-sip-bridge/src/line/mod.rs (T003)"
```

## Parallel Example: User Story 3

```bash
Task: "Add AlertCategory::LineDiscoveryFailed in gsm-sip-bridge/src/alerts/mod.rs (T019)"
Task: "Add AlertsConfig.line_discovery_failed in gsm-sip-bridge/src/config/mod.rs (T020)"
Task: "Add VOWIFI_LINE_DISCOVERY_FAILED gauge in gsm-sip-bridge/src/metrics/mod.rs (T021)"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Phase 1: Setup (T001).
2. Phase 2: Foundational (T002-T005) — **blocks everything else**.
3. Phase 3: User Story 1 (T006-T012).
4. **STOP and VALIDATE**: `make test` green; manually reproduce per `quickstart.md`'s "Verifying the fix" #1-2 against a real or simulated slow-enumerating modem.
5. This alone already fixes the production incident that motivated the feature — a slow modem self-heals without a restart.

### Incremental Delivery

1. Setup + Foundational → shared detection ready.
2. Add US1 → validate independently → this is the MVP (the concrete incident fix).
3. Add US2 → validate independently → operators stop needing to cross-reference logs against config.
4. Add US3 → validate independently → proactive Discord/metrics visibility, no manual checking needed.
5. Polish → docs, sample config, full quickstart + `make format && make lint && make test` gate.

### Parallel Team Strategy

After Foundational (Phase 2) is done:

- Developer A: US1 (T006-T012) — the retry loop.
- Developer B: US2 (T013-T016) — can start immediately (only needs T005's single-pass detection), and later re-validate once US1 lands (fewer false positives).
- Developer C: US3 (T019-T021's plumbing can start immediately in parallel; T022-T023 wait on US1's T010/T011).

---

## Notes

- [P] tasks touch different files with no dependency on an incomplete task.
- Every implementation task has a corresponding test task written first, per the constitution's TDD default and Integration-First Testing principle — no boundary in this feature is impractical to test for real (per research.md R6, retry logic is tested via `ProbedModem` fixtures across two calls, the same pattern `test_discovery.rs` already uses; no new mocks are introduced).
- Commit after each task or logical group, per Constitution Principle III (Frequent Atomic Commits) — do not batch unrelated tasks into one commit.
- `make format && make lint && make test` MUST pass before every commit (`CLAUDE.md`'s pre-commit checklist) — `make lint` covers the whole workspace including test targets.
- Total: 26 tasks — Setup 1, Foundational 4, US1 7, US2 4, US3 7, Polish 3.
