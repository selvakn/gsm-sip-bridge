---
description: "Task list for bad-port isolation"
---

# Tasks: Isolate a hanging serial port from wedging discovery

**Input**: Design documents from `/specs/030-bad-port-isolation/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/discovery-config.md

**Tests**: INCLUDED. The project constitution mandates Integration-First Testing
(NON-NEGOTIABLE) and Green-on-Commit, so each behavioral change carries a test.

**Organization**: Grouped by user story (US1=P1, US2=P2, US3=P3) so each is an
independently testable increment.

## Path Conventions

Single Rust workspace. Primary file: `gsm-sip-bridge/src/modules/discovery.rs`.
Config: `gsm-sip-bridge/src/config/{raw.rs,mod.rs}`. Docs parity:
`docs/configuration.md`, `config.toml.example`. Tests live in-module
(`#[cfg(test)]` in `discovery.rs`) or under `gsm-sip-bridge/tests/`.

---

## Phase 1: Setup (Shared Infrastructure)

- [x] T001 Confirm baseline is green before any change: run `make format && make lint && make test` and record that the pre-feature discovery tests pass (this is the FR-008 "identical behavior" baseline).

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Shared plumbing every user story builds on. No story work starts until this phase is done.

**⚠️ CRITICAL**: T002–T005 gate all of Phase 3–5.

- [x] T002 Introduce a `CandidatePort { device_path, iface_path }` type (or a `(PathBuf, PathBuf)` pair) and change `candidate_tty_ports` in `gsm-sip-bridge/src/modules/discovery.rs` to return the USB interface path alongside each `/dev/ttyUSB*` device path (the sysfs interface dir name is the topology fragment). Update `order_candidates_with_preference`, `probe_at_port`, and `scan_all_inner` call sites to carry it through. Preserve existing sort/order behavior.
- [x] T003 [P] Update the existing `candidate_tty_ports_*` unit tests in `gsm-sip-bridge/src/modules/discovery.rs` to assert the interface path is captured for each candidate (finds every interface regardless of number; empty when none; ignores non-interface entries).
- [x] T004 Add a `DiscoveryPolicy` value carrying `excluded: Vec<PortMatcher>`, `probe_timeout: Duration`, and a mutable per-port quarantine handle (see data-model.md), plus a `DiscoveryPolicy::unfiltered()` constructor (empty list, 3000ms, no quarantine). Thread `&mut DiscoveryPolicy` (or `&DiscoveryPolicy` + separate `&mut QuarantineState`) through `scan_all_inner`; keep the existing zero-arg public wrappers (`scan_all`, `scan_all_preferring`, `scan_modules`, `scan_all_preferring_with_sim_recovery`) delegating with `unfiltered()` so today's callers and tests compile unchanged.
- [x] T005 Add the `[discovery]` config section: `RawDiscovery { excluded_ports: Vec<String>, probe_timeout_ms: u64 }` via the `section!` macro in `gsm-sip-bridge/src/config/raw.rs` (defaults `[]` and `3000`), a runtime `DiscoveryConfig` + `From<RawDiscovery>` in `gsm-sip-bridge/src/config/mod.rs` that parses each `excluded_ports` string into a `PortMatcher` (device-path vs topology-prefix), and a `discovery: DiscoveryConfig` field on `AppConfig` (~`config/mod.rs:857`).

**Checkpoint**: Candidate ports carry topology paths, a policy is threaded through the scan, and `[discovery]` config parses — user stories can proceed.

---

## Phase 3: User Story 1 - Startup and rescans survive a wedged serial port (Priority: P1) 🎯 MVP

**Goal**: One port that never returns from open/read no longer wedges discovery; the scan abandons it after `probe_timeout`, keeps serving healthy modems, and quarantines it after 3 consecutive timeouts.

**Independent Test**: With a fake never-responding port alongside healthy fake ports, `scan_all_inner` completes within ~`probe_timeout` and returns the healthy modems; three passes over the bad port leave it quarantined and un-reprobed.

- [x] T006 [P] [US1] Add a test-only fake port/transport that never returns from open/read, in `gsm-sip-bridge/src/modules/discovery.rs` `#[cfg(test)]` (mirroring the existing `MockStream`/`ScriptedModem` pattern; comment justifying the fake per Constitution I).
- [x] T007 [US1] Implement the bounded-probe worker: run each individual port open/probe (both `probe_at_port`'s per-candidate open and `probe_sim_status_at`) on a `std::thread::spawn`ed worker that reports on an `mpsc` channel, and have the scan wait with `rx.recv_timeout(policy.probe_timeout)`. On `Timeout`, abandon the port and continue; the worker is deliberately leaked. File: `gsm-sip-bridge/src/modules/discovery.rs`.
- [x] T008 [US1] Emit the FR-012 abandon log: on timeout, `tracing::warn!` naming BOTH the device path and the USB interface (topology) path and stating the port was abandoned after `probe_timeout` and left unresolved. File: `gsm-sip-bridge/src/modules/discovery.rs`.
- [x] T009 [US1] Implement per-port quarantine (FR-013): increment a consecutive-timeout counter on timeout, reset it to 0 on any non-timeout result, and after 3 consecutive timeouts add the port to the in-memory quarantine set so later `scan_all_inner` passes skip it (never opened). State lives in the caller-owned `QuarantineState`; cleared on process restart, never persisted. File: `gsm-sip-bridge/src/modules/discovery.rs`.
- [x] T010 [US1] Ensure an abandoned/quarantined port is never reported as a usable modem/line (FR-011): it produces no `ProbedModem` with a usable `at_port`/`sim_status`. File: `gsm-sip-bridge/src/modules/discovery.rs`.
- [x] T011 [P] [US1] Integration test: fake never-responding port + ≥1 healthy fake port ⇒ scan completes within a bounded time and reports the healthy modem; assert the bad port is absent from usable results (SC-001, SC-002).
- [x] T012 [P] [US1] Test: a slow-but-healthy port that responds just under `probe_timeout` is resolved normally and NOT abandoned (US1 acceptance scenario 3).
- [x] T013 [P] [US1] Test: 3 consecutive timeouts on the same port ⇒ port is quarantined and not re-probed on the next pass; a single timeout followed by success resets the counter (FR-013).
- [x] T014 [US1] Wire the daemon's long-lived rescan loop (`gsm-sip-bridge/src/modules/mod.rs`, the `scan_modules_excluding_cards` call sites) to own a `QuarantineState` across rescans and pass the real `DiscoveryPolicy` (from `AppConfig.discovery`) into the scan. The one-shot `discover` CLI path (`gsm-sip-bridge/src/commands/discover.rs`) passes a fresh policy from config.

**Checkpoint**: Daemon survives a hung port on startup and across rescans with no config — MVP deliverable.

---

## Phase 4: User Story 2 - Operator excludes a known-bad port from probing (Priority: P2)

**Goal**: A port listed in `[discovery] excluded_ports` is never opened or probed, matched by exact device path or topology prefix.

**Independent Test**: With a port in `excluded_ports`, discovery logs no probe attempt for it and it is absent from results; a topology entry still matches after the device path renumbers.

- [x] T015 [US2] Implement `PortMatcher::matches(device_path, iface_path)` in `gsm-sip-bridge/src/config/mod.rs` (or a small helper module): exact device-path equality for `/dev/...` entries; equality-or-segment-aligned-leading-prefix for topology fragments; never unanchored substring (data-model.md / contract table).
- [x] T016 [P] [US2] Unit tests for `PortMatcher::matches`: exact device path hit/miss; exact topology hit; whole-device prefix matches all interfaces; a non-anchored substring MUST NOT match (contract matching table).
- [x] T017 [US2] Apply the blocklist in `candidate_tty_ports`/`scan_all_inner`: drop any candidate whose device or interface path matches a `policy.excluded` matcher BEFORE any open happens (FR-007), for both startup and rescans (FR-010). File: `gsm-sip-bridge/src/modules/discovery.rs`.
- [x] T018 [P] [US2] Integration test: a blocklisted fake port is never opened (zero probe attempts) and absent from results, while other ports probe normally (SC-003); an entry matching no attached port is harmless (US2 scenario 4).
- [x] T019 [P] [US2] Test FR-008: empty/absent `[discovery]` (unfiltered policy) ⇒ scan results identical to the no-config baseline.
- [x] T020 [US2] Document the `[discovery]` section in `docs/configuration.md` (both keys, matching rules) and add a commented `[discovery]` block with an `excluded_ports` example to `config.toml.example`; confirm `gsm-sip-bridge/tests/test_config_docs.rs` passes (every accepted key documented; section present in example).

**Checkpoint**: Operators can permanently exclude a known-bad port from config; survives replug/reboot via topology form.

---

## Phase 5: User Story 3 - Operator can see which ports were abandoned or skipped and why (Priority: P3)

**Goal**: Logs distinguish abandoned-by-timeout vs skipped-by-config vs skipped-by-quarantine vs SIM-unusable, each by port and reason.

**Independent Test**: Trigger each outcome; confirm the logs are distinguishable by port and reason.

- [x] T021 [US3] Emit a distinct skip log for blocklisted ports (named port + "skipped by exclusion list") and for quarantined ports (named port + "quarantined after 3 timeouts"), distinct from the T008 abandon log and the existing SIM-unusable/no-AT logs. File: `gsm-sip-bridge/src/modules/discovery.rs`.
- [x] T022 [P] [US3] Test the log/outcome taxonomy: assert the four `ProbeOutcome` cases (Resolved, AbandonedTimeout, SkippedBlocklistConfig, SkippedQuarantine) are produced for the corresponding inputs and carry the port identity (assert on the outcome value; log-line assertion only if a capture harness already exists).

**Checkpoint**: A missing line is diagnosable from logs alone; operator knows whether to add an `excluded_ports` entry.

---

## Phase 6: Polish & Cross-Cutting Concerns

- [x] T023 [P] Edge-case tests: multiple simultaneously-wedged fake ports each bounded independently and the scan still completes; a modem whose ONLY AT-capable interface is the bad port is reported unresolved while other modems are unaffected (spec Edge Cases).
- [x] T024 [P] Verify the `scan_all_inner` call-site guard test (`gsm-sip-bridge/src/modules/discovery.rs:1088`) still passes after the signature change, and update it if the SIM-recovery-arg grep needs adjusting for the new policy parameter.
- [x] T025 Update `quickstart.md` if any identifier/log-string diverged from the implementation, so the operator instructions match real log output.
- [x] T026 Full green gate: `make format && make lint && make test` (whole workspace, all targets) — must pass with zero warnings before commit (Constitution II/IV).

---

## Dependencies & Execution Order

- **Setup (T001)** → **Foundational (T002–T005)** → user stories.
- **Foundational blocks everything**: T002 (candidate iface path) and T004 (policy threading) are prerequisites for US1; T005 (config) + T002 for US2.
- **US1 (P1, T006–T014)** is the MVP and can ship alone (no config needed).
- **US2 (P2, T015–T020)** depends only on Foundational (T002, T005), not on US1 — but in practice lands after US1. `PortMatcher` (T015) is independent of the probe mechanism.
- **US3 (P3, T021–T022)** depends on US1 (T008/T009 logs) and US2 (T017/T021 skip logs) existing.
- **Polish (T023–T026)** last.

### Parallel opportunities

- Within US1: T011, T012, T013 (separate test cases) run in parallel after T007–T010; T006 in parallel with early impl.
- Within US2: T016 and T018/T019 in parallel; T015 (matcher, in config/) is parallelizable against discovery.rs work.
- Across stories: once Foundational is done, `PortMatcher` (T015/T016, config/) can proceed in parallel with the US1 probe mechanism (discovery.rs) — different files.

## Implementation Strategy

- **MVP = Phase 1 + 2 + 3 (US1)**: the daemon stops wedging on a hung port with no operator action. Ship here if time-boxed.
- **Increment 2 = US2**: operator blocklist as the permanent escape hatch.
- **Increment 3 = US3 + Polish**: diagnosability and edge-case hardening.
- Commit per task or logical group (Constitution III); run the green gate (T026 command) before each commit, never just at the end.
