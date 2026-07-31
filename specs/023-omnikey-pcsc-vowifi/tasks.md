---

description: "Task list for PC/SC Card-Reader-Backed VoWiFi Lines"
---

# Tasks: PC/SC Card-Reader-Backed VoWiFi Lines

> **Historical record.** The `[X]` items below describe the feature as
> implemented at v8.1.0 and are left unedited. One decision has since been
> superseded: T003/T009/T015's mandatory `mcc`/`mnc` for a `pcsc_reader` line.
> Those are now optional and derived from the card's `EF_IMSI`/`EF_AD`
> (`imsi_override` is still mandatory). Do not restore the `mcc`/`mnc`
> validation or its tests from this list — see `data-model.md`'s Validation
> Summary and the Unreleased section of `RELEASE_NOTES.md` for the current
> contract.

**Input**: Design documents from `/specs/023-omnikey-pcsc-vowifi/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/pcsc-line-config-contract.md, quickstart.md

**Tests**: Included — this project's constitution (`.specify/memory/constitution.md`,
Principle II "Green-on-Commit" + Development Workflow's TDD default) makes
tests non-optional; unit tests below extend the existing table-driven test
modules already in `discovery.rs`/`orchestrate.rs`/`config/mod.rs` rather than
introducing a new test style.

**Organization**: Tasks are grouped by user story (spec.md's US1/US2/US3, in
priority order) so each is independently implementable and testable.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files/functions, no dependency on an incomplete task)
- **[Story]**: US1, US2, or US3 — maps to spec.md's prioritized user stories
- File paths are exact and relative to the repo root

## Path Conventions

Single Rust workspace (per plan.md's Project Structure) — no new crates or
top-level directories; all changes are within `gsm-sip-bridge/src/{config,
vowifi,supervise}`, `docker/`, `config.toml.example`, and `docs/`.

---

## Phase 1: Setup

**Purpose**: Confirm a clean starting point — no new project scaffolding is
needed since this feature is additive within the existing workspace.

- [X] T001 Run `cargo fmt --all && make lint && cargo test --workspace` on this
      branch before any change, to confirm a clean, green baseline per
      CLAUDE.md's pre-commit checklist.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: The shared data-model additions every user story's logic
branches on. **No user story task below can be implemented until this phase
is complete.**

- [X] T002 [P] Add `pub pcsc_reader: bool` (default `false` via `#[serde(default)]`)
      to `VowifiLineOverride` in `gsm-sip-bridge/src/config/mod.rs` (struct at
      line 644-664).
- [X] T003 Add validation in `load_config` (`gsm-sip-bridge/src/config/mod.rs:712`)
      so that any `[[vowifi.line]]` entry with `pcsc_reader = true` and a
      missing/empty `imsi_override`, `mcc`, or `mnc` returns
      `BridgeError::Config` naming the entry's position and the missing
      field(s) — following the existing `Err(BridgeError::Config(format!(...)))`
      pattern already used throughout this function.
- [X] T004 [P] Add `pub pcsc_reader: bool` to `ResolvedLine` in
      `gsm-sip-bridge/src/vowifi/discovery.rs` (struct at line 110-118).
- [X] T005 [P] Add `pub pcsc_reader: bool` to `LineResolutionEntry` in
      `gsm-sip-bridge/src/vowifi/discovery.rs` (struct at line 269-284), and
      thread it through the `From<&ResolvedLine> for LineResolutionEntry`
      impl (line 286-305).

**Checkpoint**: Data model carries `pcsc_reader` end-to-end (config →
resolved line → serialized line-resolution entry). User story implementation
can now begin.

---

## Phase 3: User Story 1 - Register a VoWiFi line from a directly-inserted SIM card (Priority: P1) 🎯 MVP

**Goal**: A single card-reader-backed line (no modem lines configured at all)
registers to the carrier's IMS network using the SIM in the attached reader
and can carry a call, exactly like a modem-backed line does today.

**Independent Test**: With one `[[vowifi.line]] pcsc_reader = true` entry and
no modem-backed lines configured, bring the service up and confirm the line
reaches a registered state and handles a test call — per quickstart.md
steps 1-6.

### Tests for User Story 1

> Write these first; confirm they fail against the Phase 2 baseline before
> implementing.

- [X] T006 [P] [US1] Unit test in `gsm-sip-bridge/src/vowifi/discovery.rs`'s
      test module: `resolve_lines` given only a `pcsc_reader = true` override
      (no `ProbedModem`s) produces exactly one `ResolvedLine` with
      `pcsc_reader = true`, an empty `modem_port`, and the override's
      `imsi_override`/`mcc`/`mnc` copied through.
- [X] T007 [P] [US1] Unit test in `gsm-sip-bridge/src/supervise/orchestrate.rs`'s
      test module: `start_vowifi_line` with a `pcsc_reader = true` line does
      NOT run the modem-path-existence check or invoke `modem-ims --modem`
      (assert via `MockCommandRunner`'s recorded command list).
- [X] T008 [P] [US1] Unit test in `gsm-sip-bridge/src/supervise/orchestrate.rs`'s
      test module: `start_vowifi_line_strongswan` with a `pcsc_reader = true`
      line never spawns `vowifi-usim-bridge` (assert via `MockCommandRunner`'s
      recorded spawn calls), while an equivalent modem-backed line still does
      (regression check in the same test).
- [X] T009 [US1] Unit test in `gsm-sip-bridge/src/config/mod.rs`'s test module:
      `load_config` rejects a `pcsc_reader = true` entry missing
      `imsi_override` (and separately, missing `mcc`/`mnc`) with a
      `BridgeError::Config` naming the problem.

### Implementation for User Story 1

- [X] T010 [US1] Implement `resolve_one_pcsc_line(index: u32, override: &VowifiLineOverride, base: &VowifiConfig) -> ResolvedLine`
      in `gsm-sip-bridge/src/vowifi/discovery.rs`, reusing
      `resolve_one_line`'s existing per-index infra derivation block (netns,
      veth addrs/ifaces, strongswan if_id/tun_iface, vpcd_port — lines
      218-233) and setting `modem_port = PathBuf::new()`, `pcsc_reader = true`.
- [X] T011 [US1] In `resolve_lines` (`gsm-sip-bridge/src/vowifi/discovery.rs:130-178`),
      after building `lines` from the modem-derived `ready` list, append one
      `ResolvedLine` per `base.line_overrides` entry with `pcsc_reader = true`
      via T010's helper, continuing the same index counter and subject to
      the same `max_lines` bound/overflow reporting as modem lines.
- [X] T012 [US1] In `start_vowifi_line` (`gsm-sip-bridge/src/supervise/orchestrate.rs:467-529`),
      branch on `line.pcsc_reader` immediately after the existing log line to
      skip the modem-path-exists check (lines 481-484) and the
      `modem-ims --modem` reconcile call (lines 486-495).
- [X] T013 [US1] In `start_vowifi_line_strongswan` (`gsm-sip-bridge/src/supervise/orchestrate.rs:532-679`),
      skip the entire `vowifi-usim-bridge` spawn block (lines 609-668) when
      `line.pcsc_reader` is `true`.
- [X] T014 [US1] Add Alpine's `ccid` package to the runtime `apk add` list in
      `docker/Dockerfile` (around line 227-234), and update the comment block
      above it (lines 220-226) to note both drivers now coexist: `ifd-vpcd`
      (virtual, modem-bridged lines) and `ccid` (real USB PC/SC readers).
- [X] T015 [P] [US1] Add a `[[vowifi.line]]` example block to
      `config.toml.example` (near the existing per-line examples around
      line 233-254) showing `pcsc_reader = true` with mandatory
      `imsi_override`/`mcc`/`mnc`, noting it requires
      `tunnel_engine = "strongswan"`.
- [X] T016 [P] [US1] Write `docs/omnikey-pcsc-vowifi.md` covering: reading the
      IMSI once via `pySim-read.py`, the config block from T015, the `ccid`
      driver requirement, and a verification checklist (mirrors
      quickstart.md); cross-link it from `docs/vowifi-bridge.md`.

**Checkpoint**: User Story 1 is fully functional and independently
deployable/testable — a standalone card-reader line works with zero modem
lines configured.

---

## Phase 4: User Story 2 - Run card-reader and modem-backed lines side by side (Priority: P2)

**Goal**: A card-reader-backed line and one or more modem-backed lines
coexist in the same deployment, sharing the `max_lines` bound, each
registering and failing independently.

**Independent Test**: Configure one modem-backed line (as before this
feature) plus one `pcsc_reader = true` line, start the service, and confirm
both register independently and a failure in one doesn't affect the other —
per quickstart.md step 8.

### Tests for User Story 2

- [X] T017 [P] [US2] Unit test in `gsm-sip-bridge/src/vowifi/discovery.rs`'s
      test module: `resolve_lines` given one `ProbedModem` (ready) plus one
      `pcsc_reader = true` override produces two `ResolvedLine`s with
      distinct `index`/`netns`/veth addresses, the modem line unchanged in
      shape from today, and the pcsc line's `modem_port` empty.
- [X] T018 [P] [US2] Unit test in `gsm-sip-bridge/src/vowifi/discovery.rs`'s
      test module: with `max_lines` set low enough that modem lines alone
      nearly fill it, an additional `pcsc_reader` override that would exceed
      the bound is reported in `LineTableResult.failed` with reason
      `max_lines_exceeded`, identically to how an excess modem line is
      reported today.
- [X] T019 [US2] Regression test in `gsm-sip-bridge/src/vowifi/discovery.rs`'s
      test module: an existing modem-only `resolve_lines` scenario (no
      `pcsc_reader` overrides present) produces byte-identical output to
      before this feature — confirms spec FR-005/SC-004 (no behavior change
      for existing deployments).

### Implementation for User Story 2

- [X] T020 [US2] Verify (and adjust if a gap is found) that T011's
      pcsc-line-appending logic in `resolve_lines` counts pcsc lines against
      `max_lines` together with modem lines (not as a separate unbounded
      pool) — satisfy T018.
- [X] T021 [US2] Verify (and adjust if a gap is found) that a pcsc line's
      registration failure cannot affect a modem line's thread in the
      per-line `std::thread::spawn` loop (`gsm-sip-bridge/src/supervise/orchestrate.rs:224-243`)
      — each line already runs on its own thread, so this is confirming, not
      introducing, isolation (spec FR-007).

**Checkpoint**: User Stories 1 AND 2 both work — mixed modem + card-reader
deployments are verified independent and non-regressing.

---

## Phase 5: User Story 3 - Fail fast on an unsupported combination (Priority: P3)

**Goal**: A `pcsc_reader = true` line configured under `tunnel_engine = "swu"`
causes `supervise` to fail at startup with a clear, specific error, instead
of silently starting without that line.

**Independent Test**: Configure a `pcsc_reader = true` line with
`tunnel_engine = "swu"` and start the service; confirm it exits non-zero with
an error naming the offending line and setting, before any per-line process
is spawned — per quickstart.md's engine-compatibility note.

### Tests for User Story 3

- [X] T022 [P] [US3] Unit test in `gsm-sip-bridge/src/supervise/orchestrate.rs`'s
      test module: the supervise entrypoint, given `tunnel_engine = "swu"`
      and at least one `pcsc_reader = true` resolved line, returns a failure
      exit code with an error message naming that line's index/card_id,
      before any `ChildSpec` is spawned (assert via `MockCommandRunner`
      recording zero spawns).

### Implementation for User Story 3

- [X] T023 [US3] Add the engine-compatibility check in
      `gsm-sip-bridge/src/supervise/orchestrate.rs`, before the per-line
      spawn loop (before line 224): if `config.vowifi.tunnel_engine != "strongswan"`
      and any resolved line has `pcsc_reader = true`, print a clear error
      naming the line and return `ExitCode::FAILURE` immediately.

**Checkpoint**: All three user stories are independently functional.

---

## Phase 6: Polish & Cross-Cutting Concerns

- [X] T024 [P] Verify `vowifi-status`/Prometheus metrics output for a
      `pcsc_reader` line (empty `modem_port`) renders sensibly with no
      distinguishing field — spec FR-010/SC-005; fix any awkward empty-field
      rendering found (e.g. in the status-query code path under
      `gsm-sip-bridge/src/vowifi/` or `gsm-sip-bridge/src/metrics/`).
- [X] T025 Run `cargo fmt --all && make lint && cargo test --workspace` full
      pass (CLAUDE.md's mandatory pre-commit checklist) across every change
      above.
- [X] T026 (see checklists/requirements.md) Execute
      `specs/023-omnikey-pcsc-vowifi/quickstart.md` end-to-end against the
      real OmniKey reader + Vodafone SIM (manual, hardware- and
      network-dependent — not automatable in CI, per this project's
      constitution's stated exception for hardware unavailable in CI).
      Steps 1-4 done live; reader discovery, line resolution, and
      eap-sim-pcsc's reader discrimination all confirmed correct in a real
      mixed deployment. 2026-07-28: the Vodafone ePDG tunnel is confirmed UP
      (EAP-AKA succeeded, IKE_SA + CHILD_SA established, verified on the
      wire with tcpdump) — the earlier "carrier not responding" note was
      wrong and is retracted; the real cause was port contention from
      running a second bridge container alongside the production one under
      `--network host`. Steps 5-8 (IMS registration + test call) also now
      confirmed, run genuinely card-reader-only (Quectel modem physically
      removed): IMS-AKA REGISTER got 200 OK, network NOTIFY confirmed an
      active registration, and a real inbound call was signaled and dialed
      into the PBX. This required adding a PC/SC transport for IMS-AKA
      registration itself (spec's original scope only covered the ePDG
      tunnel's PC/SC path) plus an auto-generated IMEI and a READ RECORD
      SW=6C1A retry fix — see checklists/requirements.md's
      "Post-implementation follow-up (2026-07-28)" section for the full
      account.
- [X] T027 [P] Update `specs/023-omnikey-pcsc-vowifi/checklists/requirements.md`
      with any follow-up notes if implementation surfaced a spec gap.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies.
- **Foundational (Phase 2)**: Depends on Setup — BLOCKS all user stories (T010-T023 all read/write the `pcsc_reader` fields T002/T004/T005 add).
- **User Story 1 (Phase 3)**: Depends on Foundational only. This is the MVP.
- **User Story 2 (Phase 4)**: Depends on Foundational + US1's `resolve_one_pcsc_line`/`resolve_lines` changes (T010, T011) — it tests the *combination*, so it needs US1's appending logic to exist first.
- **User Story 3 (Phase 5)**: Depends on Foundational only — independent of US1/US2's implementation (it's a pure config/startup validation check), though it makes most sense to land after US1 exists to have something to reject.
- **Polish (Phase 6)**: Depends on all three user stories being complete.

### Parallel Opportunities

- T002, T004, T005 (Phase 2) touch different structs/files and can run in parallel.
- T006, T007, T008 (US1 tests) touch different test modules and can run in parallel; T009 touches `config/mod.rs` and can run alongside them.
- T015, T016 (US1 docs) can run in parallel with each other and with T014 (Dockerfile).
- T017, T018 (US2 tests) can run in parallel.
- Phase 5 (US3) is fully independent of Phase 4 (US2) and could be staffed in parallel once Phase 3 lands.

---

## Parallel Example: User Story 1

```bash
# Tests, in parallel (different test modules):
Task: "Unit test: resolve_lines with a single pcsc_reader override in gsm-sip-bridge/src/vowifi/discovery.rs"
Task: "Unit test: start_vowifi_line skips modem checks in gsm-sip-bridge/src/supervise/orchestrate.rs"
Task: "Unit test: start_vowifi_line_strongswan skips vowifi-usim-bridge in gsm-sip-bridge/src/supervise/orchestrate.rs"

# Docs, in parallel with each other and with the Dockerfile change:
Task: "Document pcsc_reader example in config.toml.example"
Task: "Write docs/omnikey-pcsc-vowifi.md"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Complete Phase 1 (Setup) and Phase 2 (Foundational).
2. Complete Phase 3 (User Story 1) — a standalone card-reader line works.
3. **STOP and VALIDATE**: run quickstart.md steps 1-6 against the real reader.
4. This alone is deployable/demoable value — an operator can already run a
   pure card-reader VoWiFi deployment.

### Incremental Delivery

1. Setup + Foundational → foundation ready.
2. User Story 1 → validate independently → MVP.
3. User Story 2 → validate mixed deployment independently.
4. User Story 3 → validate the fail-fast guard independently.
5. Polish → full regression pass + live hardware soak (quickstart.md step 7's
   auto-recovery check especially benefits from a longer soak, since it's
   confirming existing supervision behavior rather than new code).

---

## Notes

- No new external Rust crate dependencies (research.md §3) — this is
  orchestration/config plumbing, not a new SIM-access implementation.
- FR-010 (observability parity) and FR-011 (auto-recovery) are satisfied by
  *existing* unmodified machinery per research.md §4-5 — T024 is a
  verification/fix-if-needed task, not new feature work; no dedicated task
  builds new recovery or metrics code.
- Commit after each task or logical group, per this project's constitution
  (Frequent Atomic Commits) and CLAUDE.md's pre-commit checklist.
