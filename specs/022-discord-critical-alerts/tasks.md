---

description: "Task list for Discord Alerts for Critical Events"
---

# Tasks: Discord Alerts for Critical Events

**Input**: Design documents from `/specs/022-discord-critical-alerts/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: Included as first-class tasks per each story. The project constitution
(`.specify/memory/constitution.md`, Principle II "Green-on-Commit" and the
Development Workflow section) makes TDD the default practice and requires
`cargo test --workspace` green before every commit — tests are not optional here.

**Organization**: Tasks are grouped by user story (spec.md priorities P1/P1/P2/P2/P3)
to enable independent implementation and testing of each.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies on incomplete tasks)
- **[Story]**: Maps to spec.md's US1–US5
- Every commit must pass `cargo fmt --all && make lint && cargo test --workspace`
  (CLAUDE.md's mandatory pre-commit checklist) before it lands.

## Path Conventions

Single Rust crate/workspace (`gsm-sip-bridge/`) — see plan.md's Project
Structure for the full file list. No frontend/backend split.

---

## Phase 1: Setup

**Purpose**: Scaffold the new `alerts` module and its metrics so every later
task has somewhere to plug in.

- [X] T001 Add `pub mod alerts;` to `gsm-sip-bridge/src/lib.rs` and create
      `gsm-sip-bridge/src/alerts/mod.rs` + `gsm-sip-bridge/src/alerts/discord.rs`
      as empty modules with a module-level doc comment describing their role
      (per plan.md's Project Structure).
- [X] T002 [P] Add `AlertCategory` enum (`Sms`, `ModuleLifecycle`,
      `RegistrationLoss`, `TunnelFailure`, `MissedCall`) with `as_str()` and
      `CriticalEvent`/`CriticalEventKind`/`AlertOutcome` types to
      `gsm-sip-bridge/src/alerts/mod.rs`, per data-model.md.
- [X] T003 [P] Add `CRITICAL_ALERTS_TOTAL` (`CounterVec`, labels
      `category, outcome`) and `CRITICAL_EVENT_ACTIVE` (`GaugeVec`, labels
      `category, module`) to `gsm-sip-bridge/src/metrics/mod.rs`, following the
      existing `SMS_FORWARDED_TOTAL`/`AGENT_UP` `Lazy` construction pattern.
      See contracts/metrics.md.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Config schema, the generic Discord sender, and the shared
dispatch decision function every user story calls into.

**⚠️ CRITICAL**: No user story task may start until this phase is complete.

- [X] T004 Add `AlertsConfig`, `CategoryAlertConfig`,
      `ModuleLifecycleThresholds`, `TunnelFailureThresholds`,
      `RegistrationLossThresholds` structs to `gsm-sip-bridge/src/config/mod.rs`,
      per data-model.md.
- [X] T005 Implement `parse_alerts()` in `gsm-sip-bridge/src/config/mod.rs`:
      add `"alerts"` to `TOP_LEVEL_SECTIONS`, add `ALERTS_KEYS`/
      `ALERTS_CATEGORY_KEYS` consts, parse `[alerts]` and each
      `[alerts.<category>]` sub-table (defaults: `sms.enabled = true`, the
      other four `enabled = false`; thresholds default 60s/300s/300s per
      contracts/config-schema.md), seed `alerts.sms` from legacy `[sms]` keys
      when `[alerts.sms]` is absent, validate threshold ranges via the
      existing `as_u64_range` helper, and call `warn_unknown_keys_in` for
      each table.
- [X] T006 [P] Unit tests for `parse_alerts()` in
      `gsm-sip-bridge/tests/test_config.rs`: defaults when `[alerts]` is
      absent entirely; explicit per-category enable/disable; per-category
      webhook override resolution; `[sms]`-only backward-compat seeding;
      out-of-range threshold falls back to default with a warning, not a
      fatal error. **Write these first; confirm they fail before T004/T005
      land.**
- [ ] T007 Generalize `gsm-sip-bridge/src/sms/discord.rs`'s embed-building
      into `gsm-sip-bridge/src/alerts/discord.rs` as a `DiscordClient::
      send_alert(&self, event: &CriticalEvent) -> Result<u16, String>`
      method (reusing the existing retry/backoff/timeout logic verbatim);
      keep `forward_sms` as a thin wrapper that builds a `CriticalEvent` for
      the `Sms` category and calls `send_alert`, so behavior is byte-for-byte
      unchanged for existing SMS forwarding (FR-001).
- [ ] T008 Implement `alerts::dispatch::resolve(category_config: &
      CategoryAlertConfig, default_webhook: &Secret<String>) -> Option<&str>`
      (webhook resolution: override wins, else default, else `None`) and
      `alerts::dispatch::decide_outcome(...)` (enabled/disabled →
      `Skipped`; no webhook → `Skipped`; otherwise proceed to send) in
      `gsm-sip-bridge/src/alerts/mod.rs` — the pure decision core shared by
      every category, independent of how the actual send is invoked
      (async `Handle` vs. dedicated `Runtime`, research.md R3).
- [ ] T009 [P] Unit tests for `alerts::dispatch::resolve`/`decide_outcome` in
      `gsm-sip-bridge/src/alerts/mod.rs`'s `#[cfg(test)]` block: default vs.
      override webhook resolution; disabled category; missing webhook
      entirely. **Write first.**

**Checkpoint**: Foundation ready — every user story below only adds a call
site into `alerts::dispatch`/`DiscordClient::send_alert` plus its own
detection logic.

---

## Phase 3: User Story 1 - Module/Modem Lifecycle Failure Alerts (Priority: P1) 🎯 MVP

**Goal**: Alert when a module's SIM is absent/unreadable (GSM or VoWiFi/VoLTE
path), discovery/initialization fails, or a module's AT command worker goes
unresponsive for 60s — but only once each condition's own built-in recovery
is exhausted (spec Clarifications Q7/Q8 for the VoWiFi SIM path; the GSM
path and AT-worker path have no existing auto-recovery loop, so they fire
directly).

**Independent Test**: Pull a SIM from a running GSM module (or block a
VoWiFi line's SIM access) and confirm a Discord message identifying the
module/line and condition, per quickstart.md US1.

### Tests for User Story 1

- [ ] T010 [P] [US1] Unit test for a new pure helper
      `modules::at_worker::is_unresponsive(last_success: Instant, now:
      Instant, threshold: Duration) -> bool` in
      `gsm-sip-bridge/src/modules/mod.rs` — construct `last_success` as
      `Instant::now() - Duration::from_secs(61)` against a 60s threshold
      (no sleeping). **Write first.**
- [ ] T011 [P] [US1] Extend `gsm-sip-bridge/src/supervise/sim_recovery.rs`'s
      existing table-driven tests with a case asserting that observing
      `Action::GiveUpForThisIncident` is distinguishable by the caller from
      `Action::ResetSim`/no-action (the type already supports this — this
      test locks in the call site's obligation to handle it, addressing the
      research.md R2 gap). **Write first; must fail against today's
      `orchestrate.rs`, which currently drops this case silently.**
- [ ] T012 [P] [US1] Integration test in new
      `gsm-sip-bridge/tests/test_alerts_discord.rs` using `wiremock` (the
      existing external-service mock convention from
      `tests/test_sms_discord.rs`): dispatching a `ModuleLifecycle`
      `CriticalEvent` posts an embed containing the module id and
      description to the mocked webhook. **Write first.**

### Implementation for User Story 1

- [ ] T013 [US1] Add a per-module `last_at_success: Instant` field (reset on
      every successful AT command) to the module/card state in
      `gsm-sip-bridge/src/modules/mod.rs`; on each poll tick, call
      `at_worker::is_unresponsive` (T010) and dispatch a `ModuleLifecycle`
      `Failure` event via `Handle::current().spawn(...)` the first time it
      flips true, and a `Recovered` event the next time an AT command
      succeeds after having been unresponsive.
- [ ] T014 [US1] At the existing `SimStatus::Absent`/`SimStatus::Unreadable`
      handling site in `gsm-sip-bridge/src/modules/discovery.rs` (GSM path)
      and at module discovery/initialization failure in the same file,
      dispatch a `ModuleLifecycle` `Failure` event via
      `Handle::current().spawn(...)` — this path has no existing
      auto-recovery loop, so it fires directly (no exhaustion wait, unlike
      the VoWiFi SIM path below).
- [ ] T015 [US1] In `gsm-sip-bridge/src/supervise/orchestrate.rs`'s
      per-line `vowifi-ims-agent` supervision loop (~line 868), add the
      missing `match` arm for `sim_recovery::Action::GiveUpForThisIncident`:
      log via `tracing::error!` and dispatch a `ModuleLifecycle` `Failure`
      event for that line (research.md R2).
- [ ] T016 [US1] In `supervise::orchestrate::run` (`gsm-sip-bridge/src/
      supervise/orchestrate.rs`), after `config::load_config`, build one
      dedicated `tokio::runtime::Runtime` (research.md R3) and load
      `AlertsConfig`; wrap both in `Arc` and thread them into the per-line
      spawn closures alongside the existing `runner`/`started`/
      `shutting_down` state, so T015's dispatch call has a `Handle` to
      `.spawn()` onto.

**Checkpoint**: User Story 1 is independently functional — enabling
`[alerts.module_lifecycle]` and pulling a SIM (GSM or VoWiFi) produces one
Discord alert, not a stream, and a transient blip that self-recovers
produces none.

---

## Phase 4: User Story 2 - IMS/SIP Registration Loss Alerts (Priority: P1)

**Goal**: Alert when a VoLTE/VoWiFi line's SIP registration is lost and
stays lost for 5 continuous minutes (surviving the agent's own 5s
crash-restart loop), with a recovery notice on re-registration; no alert on
a deliberate/clean unregister.

**Independent Test**: Block a line's PBX reachability, wait past 5 minutes,
confirm one Discord alert then one recovery notice on unblock — per
quickstart.md US2.

### Tests for User Story 2

- [ ] T017 [P] [US2] Unit tests in new
      `gsm-sip-bridge/tests/test_ingest_critical_alerts.rs` (or
      `metrics::ingest`'s own `#[cfg(test)]` block) for the new
      `registered_unhealthy_since` transition logic: `AgentState.registered
      = Some(false)` sets it; a second `false` report doesn't reset it;
      `Some(true)` after the field was set (constructed via `Instant::now()
      - Duration::from_secs(301)`) yields a `Recovered` event and clears
      the field; `Some(true)` before the threshold elapsed clears it with
      **no** event. **Write first.**
- [ ] T018 [P] [US2] Integration test in `tests/test_alerts_discord.rs`:
      `evaluate_critical_alerts` output for a stale-`registered=false`
      record posts a `RegistrationLoss` embed naming the line id.

### Implementation for User Story 2

- [ ] T019 [US2] Extend the per-`(AgentKind, module_id)` liveness record in
      `gsm-sip-bridge/src/metrics/ingest.rs` with
      `registered_unhealthy_since: Option<Instant>`; update it inside
      `apply_report` per the transition rule in data-model.md.
- [ ] T020 [US2] Implement `evaluate_critical_alerts(thresholds:
      &CriticalAlertThresholds) -> Vec<CriticalEvent>` in
      `gsm-sip-bridge/src/metrics/ingest.rs` (registration-loss portion for
      now), mirroring `evaluate_liveness`'s "evaluated on every scrape, not
      a timer" shape (research.md R4/R1).
- [ ] T021 [US2] Call `evaluate_critical_alerts` from the same site
      `metrics::server::refresh_agent_liveness` already runs on each scrape
      (`gsm-sip-bridge/src/metrics/server.rs`), and dispatch each returned
      event via `Handle::current().spawn(...)` into
      `DiscordClient::send_alert` (T007) — this stays entirely in the
      existing async/tokio world, no new runtime needed.
- [ ] T022 [US2] Verify (and adjust if needed) that a deliberate shutdown's
      `ims::unregister` path stops the agent reporting rather than reporting
      `registered = Some(false)`, so `registered_unhealthy_since` is never
      set for an expected teardown (FR-009); add a regression test in
      T017's file for this case.

**Checkpoint**: User Stories 1 and 2 both independently functional.

---

## Phase 5: User Story 3 - VoWiFi Tunnel Failure Alerts (Priority: P2)

**Goal**: Alert when a VoWiFi line's tunnel stays non-established for 5
continuous minutes, distinct from registration loss, with a recovery notice.

**Independent Test**: Block the ePDG endpoint, wait past 5 minutes, confirm
a distinct tunnel-failure alert (not a registration-loss one) then a
recovery notice — per quickstart.md US3.

> **Shared-file note**: T023/T024 extend the same `metrics::ingest.rs`
> record and evaluator that Phase 4 (T019/T020) introduced. Land Phase 4
> first, or coordinate closely if working in parallel — these are not
> `[P]` against Phase 4's equivalents despite being an independent story.

### Tests for User Story 3

- [ ] T023 [US3] Unit tests for `tunnel_unhealthy_since` transition logic in
      the same test file as T017, mirroring its cases exactly but for
      `AgentState.tunnel_up`. **Write first.**
- [ ] T024 [US3] Integration test in `tests/test_alerts_discord.rs`: a
      stale-`tunnel_up=false` record posts a `TunnelFailure` embed distinct
      from a `RegistrationLoss` one for the same line.

### Implementation for User Story 3

- [ ] T025 [US3] Extend the liveness record in `metrics::ingest.rs` with
      `tunnel_unhealthy_since: Option<Instant>`, same transition rule as
      T019 but keyed on `tunnel_up`.
- [ ] T026 [US3] Extend `evaluate_critical_alerts` (T020) to also emit
      `TunnelFailure` events; no new call site needed — T021's dispatch loop
      already consumes whatever the evaluator returns.

**Checkpoint**: User Stories 1–3 independently functional.

---

## Phase 6: User Story 4 - Missed Call Alerts (Priority: P2)

**Goal**: Alert when an inbound call is recorded with `CallStatus::Missed`
(never bridged) — explicitly not for `CallStatus::Failed` (bridged but
broken audio), per spec Clarifications Q4.

**Independent Test**: Call a line and let it ring out; confirm one Discord
alert with caller number, line, timestamp; answering normally or bridging
with broken audio produces none — per quickstart.md US4.

> **Shared-file note**: T027/T029 touch `gsm-sip-bridge/src/modules/mod.rs`,
> the same file Phase 3's T013/T014 modify. Land Phase 3 first, or
> coordinate — not `[P]` against Phase 3's tasks.

### Tests for User Story 4

- [ ] T027 [US4] Unit test for `record_call_end` in
      `gsm-sip-bridge/src/modules/mod.rs`'s `#[cfg(test)]` block: the
      `"missed"` status path dispatches exactly one `MissedCall` event with
      the correct caller id/module id/timestamp; the `"answered"` and
      `"failed"` paths dispatch none. **Write first.**
- [ ] T028 [P] [US4] Integration test in `tests/test_alerts_discord.rs`: a
      `MissedCall` event posts an embed with caller number, line, and
      timestamp.

### Implementation for User Story 4

- [ ] T029 [US4] In `record_call_end`'s `"missed"` branch
      (`gsm-sip-bridge/src/modules/mod.rs`), dispatch a `MissedCall`
      `Failure` event (one-shot, always `Failure` per data-model.md — no
      recovery notice for this category) via `Handle::current().spawn(...)`.

**Checkpoint**: User Stories 1–4 independently functional.

---

## Phase 7: User Story 5 - Per-Category Alert Configuration (Priority: P3)

**Goal**: Confirm the config surface built in Phase 2 (T004–T009) actually
gates every category end-to-end, and that per-category webhook overrides
route correctly — this story is mostly a verification/integration pass over
work already landed, not new detection logic.

**Independent Test**: Disable one category in `config.toml`, trigger it,
confirm no Discord call but a `skipped` metric/log entry; override one
category's webhook and confirm only that category uses it — per
quickstart.md US5.

### Tests for User Story 5

- [ ] T030 [P] [US5] End-to-end test in `tests/test_alerts_discord.rs`:
      with `[alerts.missed_call].enabled = false`, a `MissedCall` event
      results in zero HTTP calls to the wiremock server and
      `CRITICAL_ALERTS_TOTAL{category="missed_call",outcome="skipped"}`
      increments.
- [ ] T031 [P] [US5] End-to-end test in `tests/test_alerts_discord.rs`: with
      a category-specific `discord_webhook_url` override set, two wiremock
      servers (default + override) confirm the event lands only on the
      override.
- [ ] T032 [P] [US5] End-to-end test confirming a fresh config with no
      `[alerts]` section at all preserves today's SMS-forwarding behavior
      unchanged (FR-001 regression guard).

### Implementation for User Story 5

- [ ] T033 [US5] Fix any gaps T030–T032 surface in `alerts::dispatch`
      (T008) or `parse_alerts` (T005) — by design this phase should mostly
      confirm Phase 2's work rather than add new logic; if it does surface
      a gap, this is where it's closed.

**Checkpoint**: All five user stories independently functional and gated by
config exactly as specified.

---

## Phase 8: Polish & Cross-Cutting Concerns

- [ ] T034 [P] Update `config.multi.toml` (or the project's documented
      config reference) with a `[alerts]` section example, per
      contracts/config-schema.md.
- [ ] T035 Run `cargo fmt --all && make lint && cargo test --workspace`
      across the whole feature as a final gate.
- [ ] T036 Walk through quickstart.md end-to-end against real hardware
      (`sugam-direct`) or the container, for all five user stories.
- [ ] T037 [P] Add the two new metrics (contracts/metrics.md) to any
      existing Grafana dashboard JSON under `docker/`/`docs/` alongside the
      existing SMS/agent-liveness panels, if one is tracked in this repo.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies.
- **Foundational (Phase 2)**: Depends on Phase 1 — blocks every user story.
- **User Stories (Phase 3–7)**: All depend on Phase 2. Priority order is
  US1 = US2 (both P1) → US3 = US4 (both P2) → US5 (P3), but see the
  shared-file notes below before treating any pair as parallelizable.
- **Polish (Phase 8)**: Depends on all desired user stories being complete.

### Shared-file sequencing (read before parallelizing)

- **US2 (T019–T020) and US3 (T025–T026)** both edit
  `metrics/ingest.rs`'s liveness record and `evaluate_critical_alerts`.
  Land US2 first; US3 then only adds a field and a match arm.
- **US1 (T013–T014) and US4 (T029)** both edit `modules/mod.rs`. Land US1
  first, or coordinate — US4's change is a single small addition to
  `record_call_end` and is unlikely to conflict, but the file is shared.
- Everything else (Setup, Foundational, US1's `discovery.rs`/
  `orchestrate.rs` changes, US5) touches files no other story touches.

### Within Each User Story

- Tests are written first and must fail before the corresponding
  implementation task lands (Constitution II/Development Workflow's TDD
  default).
- Detection/tracking logic before dispatch wiring.
- Story complete and independently verified (quickstart.md) before moving
  to the next priority tier.

### Parallel Opportunities

- T002/T003 (Setup) — different files, parallel.
- T006 and T009 (Foundational tests) — different files, parallel; both can
  run alongside T007 (discord.rs generalization) since none share a file.
- Within US1: T010/T011/T012 (tests, three different files) in parallel.
- Within US2: T017/T018 in parallel (different files).
- Within US3: T023/T024 in parallel.
- Within US4: T028 can run parallel to T027 (different files).
- Within US5: T030/T031/T032 all in parallel (same file, but read-only
  additive test cases with no shared mutable state).
- Across stories: once Phase 2 lands, US1 and US2 can be staffed in
  parallel (no shared files); US3 should follow US2; US4 should follow US1;
  US5 follows whichever of US1–US4 it's verifying.

---

## Parallel Example: User Story 1

```bash
# Launch all three US1 tests together:
Task: "Unit test at_worker::is_unresponsive in gsm-sip-bridge/src/modules/mod.rs"
Task: "sim_recovery GiveUpForThisIncident test in gsm-sip-bridge/src/supervise/sim_recovery.rs"
Task: "wiremock ModuleLifecycle alert test in gsm-sip-bridge/tests/test_alerts_discord.rs"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Phase 1: Setup
2. Phase 2: Foundational (blocks everything)
3. Phase 3: User Story 1 (module/modem lifecycle failure — the highest-value
   category per spec.md's own "Why this priority")
4. **STOP and VALIDATE**: run quickstart.md's US1 section against a real or
   simulated SIM failure
5. Ship — this alone already surfaces the "dead GSM worker slot" class of
   failure the project has hit before.

### Incremental Delivery

1. Setup + Foundational → alerts module + config exist, nothing fires yet.
2. + US1 → module/modem lifecycle alerts live (MVP).
3. + US2 → registration-loss alerts live.
4. + US3 → tunnel-failure alerts live (builds on US2's ingest work).
5. + US4 → missed-call alerts live.
6. + US5 → per-category config verified end-to-end; ship the full feature.

### Notes

- Every task's file path is exact and taken from plan.md's Project
  Structure — no task should require guessing where code goes.
- No task introduces a new crate, a new concurrency model, or a new SQLite
  table (research.md R1–R7) — if an implementer finds themselves reaching
  for one, re-check the plan before proceeding.
- Commit after each task or tightly related group, per Constitution III.
