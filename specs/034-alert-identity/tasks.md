# Tasks: Card Phone Number and Instance Identity in Alerts

**Feature**: `034-alert-identity` | **Plan**: [plan.md](./plan.md) | **Spec**: [spec.md](./spec.md)

Tests are included because the project constitution makes integration testing
NON-NEGOTIABLE. The alerts suite already uses `wiremock` (HTTP capture) and
`insta` (payload snapshots) — reuse them; add no new internal mocks.

All paths are under `gsm-sip-bridge/` unless noted.

## Phase 1: Setup

- [x] T001 Verify a clean baseline: run `make build && make test` from repo root and confirm the existing alert tests pass before changes.

## Phase 2: Foundational (blocking — must complete before any user story)

These make the codebase compile with the new identity plumbing (fields default to
`None`/wired instance) so US1 and US2 can each land independently on top.

- [x] T002 Add `instance_name: Option<String>` to `AlertsConfig` and `msisdn: Option<String>` to `VowifiLineOverride` in src/config/mod.rs
- [x] T003 Parse `[alerts].instance_name` and `[[vowifi.line]].msisdn` in src/config/raw.rs
- [x] T004 Wire raw → resolved for both new keys in src/config/build.rs
- [x] T005 Add `phone_number: Option<String>` to `CriticalEvent` in src/alerts/mod.rs
- [x] T006 Add `system_hostname() -> String` (via `libc::gethostname`, `"unknown"` fallback) and `instance_label(&AlertsConfig) -> String` (config value if non-empty, else hostname) in src/alerts/mod.rs
- [x] T007 Add `instance: String` field to `DiscordClient` and an `instance` parameter to `DiscordClient::new` in src/alerts/discord.rs
- [x] T008 Add a `phone_number: Option<&str>` parameter to `DiscordClient::forward_sms` (src/alerts/discord.rs) and to `sms::record_and_forward` (src/sms/mod.rs)
- [x] T009 Update every `DiscordClient::new` call site to pass `alerts::instance_label(&config.alerts)` in src/commands/daemon.rs, src/modules/pool/mod.rs (two sites), src/supervise/orchestrate.rs, src/vowifi/mod.rs
- [x] T010 Update every `CriticalEvent { … }` construction and `record_and_forward`/`forward_sms` call so the crate compiles with the new fields (default `phone_number` to `None`) across src/modules/worker.rs, src/modules/pool/mod.rs, src/modules/pool/dispatch.rs, src/metrics/ingest.rs, src/supervise/orchestrate.rs, src/vowifi/mod.rs

**Checkpoint**: `make build` compiles; `make test` still green (behavior unchanged).

## Phase 3: User Story 2 — Instance name on every alert (Priority: P1)

**Goal**: Every SMS and critical alert footer shows the instance name (config, or
system hostname when unset).

**Independent test**: Set `[alerts].instance_name`, fire any alert → footer shows
it; unset it → footer shows the hostname.

- [x] T011 [US2] Render `footer.text = format!("gsm-sip-bridge · {}", self.instance)` in both `forward_sms` and `send_alert` in src/alerts/discord.rs
- [x] T012 [P] [US2] Unit test `instance_label`: returns configured value when set, non-empty hostname when unset, in src/alerts/mod.rs `#[cfg(test)]`
- [x] T013 [US2] Integration test (wiremock capture) asserting the footer instance appears on both an SMS embed and a critical embed, in src/alerts/discord.rs `#[cfg(test)]`

**Checkpoint**: US2 independently verifiable and green.

## Phase 4: User Story 1 — Card phone number on every alert (Priority: P1)

**Goal**: Every alert shows a `Phone` field — the resolved number, or the literal
`unknown` when none is determinable.

**Independent test**: Configure a line `msisdn`, fire SMS + critical events on it →
`Phone` shows the number; on a card with no resolvable number → `Phone` = `unknown`.

- [x] T014 [US1] Render a `Phone` field (value if `Some`/non-empty, else literal `unknown`) in both `forward_sms` and `send_alert` in src/alerts/discord.rs
- [x] T015 [US1] Build a `unit_id → msisdn` map from resolved VoLTE+VoWiFi lines at `init_alerts` and look it up in `dispatch_transition` (RegistrationLoss/TunnelFailure/GmConnectionLost) in src/metrics/ingest.rs
- [x] T016 [US1] Populate `phone_number` from `SlotState.phone_number` for CS SMS (src/modules/pool/dispatch.rs) and CS ModuleLifecycle `dispatch_alert` (src/modules/pool/mod.rs)
- [x] T017 [US1] Cache the `AT+CNUM` value at worker open and set `phone_number` on the worker's ModuleLifecycle events in src/modules/worker.rs
- [x] T018 [US1] Pass the configured line `msisdn` for VoWiFi SMS in src/vowifi/mod.rs
- [x] T019 [US1] Resolve `msisdn` from `resolution.lines` for the LineDiscoveryFailed events in src/supervise/orchestrate.rs
- [x] T020 [P] [US1] Unit test the `unit_id → msisdn` resolver: configured line hits, unknown id → `None`, in src/metrics/ingest.rs `#[cfg(test)]`
- [x] T021 [US1] Integration test (wiremock capture): `Phone` shows the number when resolvable and `unknown` when not, on both SMS and critical embeds, in src/alerts/discord.rs `#[cfg(test)]`

**Checkpoint**: US1 independently verifiable and green.

## Phase 5: User Story 3 — Configure identity per deployment (Priority: P2)

**Goal**: Operator can set both values in `config.toml`; documented and parsed.

**Independent test**: Add both keys to a config, load it, confirm resolution.

- [x] T022 [US3] Document `[alerts].instance_name` and `[[vowifi.line]].msisdn` with synthetic placeholders (`+919000000000`) in config.toml.example
- [x] T023 [P] [US3] Config parse/resolve tests for `[alerts].instance_name` and `[[vowifi.line]].msisdn` in src/config `#[cfg(test)]`

## Phase 6: Polish & Cross-Cutting

- [x] T024 Review/accept any changed `insta` snapshots for alert embeds (`cargo insta` accept) under gsm-sip-bridge/
- [x] T025 Run `make format && make lint && make test` and confirm all green (whole-workspace lint incl. test targets)

## Dependencies

- Phase 1 → Phase 2 → (Phase 3 US2 ∥ Phase 4 US1) → Phase 5 US3 → Phase 6.
- US1 and US2 are independent of each other (Phone field vs footer) once Phase 2
  lands; either can ship first.
- US3 depends only on the Phase 2 config tasks (T002–T004).

## Parallel execution examples

- Within Phase 2: T002/T003/T004 are sequential (same config concern); T005 and
  T006 touch `alerts/mod.rs` and can pair with T007/T008 in `discord.rs`/`sms`.
- Test tasks marked [P] (T012, T020, T023) touch isolated `#[cfg(test)]` modules
  and can be written alongside their story's implementation.

## Implementation strategy

- **MVP**: Phase 2 + Phase 3 (US2) delivers host attribution on every alert.
- **Increment 2**: Phase 4 (US1) adds per-card phone attribution — the primary
  ask.
- **Increment 3**: Phase 5 (US3) documentation + config tests.
- Commit per phase (atomic, green) per the constitution; run the pre-commit
  checklist (`make format && make lint && make test`) before each commit.
