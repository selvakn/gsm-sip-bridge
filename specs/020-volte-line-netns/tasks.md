---
description: "Task list for Per-Line Network Isolation for VoLTE (020)"
---

# Tasks: Per-Line Network Isolation for VoLTE

**Input**: Design documents from `/specs/020-volte-line-netns/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: Included — the constitution's Integration-First Testing principle is NON-NEGOTIABLE for
this repo, and every prior VoLTE/VoWiFi feature ships table-driven tests alongside the code this
closely mirrors (`vowifi::discovery`'s tests are the direct template for this feature's derivation
tests). All file paths are relative to the repo root.

**Organization**: Tasks are grouped by user story (spec.md's US1/US2/US3, priority order), per
plan.md's Project Structure.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no unfinished dependency)
- **[Story]**: US1 (P1, cross-line traffic isolation), US2 (P2, compatibility), US3 (P3, fault
  isolation)

---

## Phase 1: Setup

**Purpose**: No new crate, no new Makefile target (plan.md) — `serde`/`serde_json` are already
workspace dependencies. This phase only stakes out the new module and test-fixture files so later
tasks don't collide.

- [X] T001 Create `gsm-sip-bridge/src/volte/carrier_agent.rs` with a module doc comment describing
      its scope (data-model.md/research.md R3: the extracted per-line carrier-half entry point,
      launched via `ip netns exec` — the counterpart to `vowifi-ims-agent`) and no logic yet; wire
      it into `gsm-sip-bridge/src/volte/mod.rs` (`pub mod carrier_agent;`).
- [X] T002 [P] Create `gsm-sip-bridge/tests/test_volte_line_netns.rs` with a module doc comment
      describing its scope (per-line namespace/veth derivation, FR-004a non-collision) and no
      tests yet — later Phase 3 tasks append to it.

**Checkpoint**: Nothing else blocks starting Phase 2.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Config schema and manifest fields every user story's code depends on
(data-model.md).

**⚠️ CRITICAL**: No user story task can begin until this phase is complete.

- [X] T003 Add `netns`, `veth_carrier_iface`, `veth_telephony_iface`, `veth_carrier_addr`,
      `veth_telephony_addr` fields (data-model.md's `VolteConfig` table) to `VolteConfig` in
      `gsm-sip-bridge/src/config/mod.rs`, with defaults distinct from `VowifiConfig`'s own
      `netns`/`veth_*` defaults (research.md R4 — e.g. `"volte"` vs `"ims"`, a distinct `/30`
      block). Parse the equivalent `[volte]` keys with the existing config-parsing helpers. Unit
      tests alongside the existing `volte_*` config tests: defaults are set, an explicit override
      parses, and (the FR-004a regression guard) the `VolteConfig` default `netns` never equals
      the `VowifiConfig` default `netns`.
- [X] T004 [P] Add `netns` to `VolteLineManifestEntry` and `VolteLineManifest`'s serialization in
      `gsm-sip-bridge/src/volte/discovery.rs` (data-model.md), read by `docker/entrypoint.sh`'s
      cleanup (research.md R6) and `volte-status`. Update `write_manifest` in
      `gsm-sip-bridge/src/volte/bridge.rs` to populate it. No derivation logic yet (Phase 3);
      this task only adds the field and plumbs it through serialization round-trip, with a unit
      test that a manifest written and re-read preserves `netns`.

**Checkpoint**: Config schema and manifest field exist; `cargo test --workspace` still green; user
story phases can now begin.

---

## Phase 3: User Story 1 - A Line's Traffic Can Never Leave on Another Line's Connection (Priority: P1) 🎯 MVP

**Goal**: Every VoLTE line's carrier-facing traffic is confined, by kernel-enforced namespace
boundary, to that line's own LTE interface — closing the routing-table collision research.md
documents (spec FR-001–FR-004b).

**Independent Test** (from spec.md): two same-carrier LTE lines, independent per-interface capture
across a full registration-and-call cycle, zero cross-line packets on either interface
(quickstart.md steps 1-2).

### Tests for User Story 1

- [X] T005 [P] [US1] In `gsm-sip-bridge/tests/test_volte_line_netns.rs`: table-driven tests for
      the new per-line derivation function (T006) mirroring
      `vowifi::discovery::resolve_one_line`'s existing test shape — index 0 resolves to the
      unindexed base (back-compat identity), index > 0 derives distinct `netns`/veth
      iface/addr per line, two lines' derived values are pairwise distinct
      (`assert_ne!`), and the **FR-004a regression guard**: a VoLTE line's derived `netns` at any
      index is never equal to a `vowifi::discovery`-derived line's `netns` at the same index
      (construct both and assert inequality directly — not just "different default string", to
      catch a future accidental prefix collision).
- [X] T006 is the implementation this test drives — write T005 first and confirm it fails against
      a stub, per this repo's TDD convention (constitution Development Workflow).

### Implementation for User Story 1

- [X] T006 [US1] Implement the per-line namespace/veth derivation in
      `gsm-sip-bridge/src/volte/discovery.rs`'s line-resolution function (extending
      `resolve_volte_lines`/`resolve_one_volte_line`, data-model.md's `ResolvedVolteLine` table),
      shaped exactly like `vowifi::discovery::resolve_one_line` (`discovery.rs:224-241`): `index`
      appended to the `VolteConfig` base fields from T003, veth addresses `shift_ipv4`-stepped by
      `4 * index`. Makes T005 pass.
- [X] T007 [US1] Extract `run_line`/`run_line_carrier`'s bodies from
      `gsm-sip-bridge/src/volte/bridge.rs` (lines ~240-399) into
      `gsm-sip-bridge/src/volte/carrier_agent.rs` as a `pub fn run(line: &ResolvedVolteLine,
      app_config: &AppConfig, control_addr: SocketAddr) -> ...` entry point (research.md R3) —
      logic unchanged (attach → derive PLMN → register → `ims::agent::serve_inbound`), only its
      home moves. `pbx_registered` admission is now read over the control channel from Agent B
      (already how `ims::agent::serve_inbound`'s `pbx_registered` parameter works — see
      contracts/volte-carrier-agent-contract.md's fault-isolation obligations), not a shared
      `Arc`.
- [X] T008 [US1] Add the `VolteCarrierAgent { line: u32 }` CLI subcommand in
      `gsm-sip-bridge/src/cli.rs` and its dispatch in `gsm-sip-bridge/src/main.rs` (mirroring
      `vowifi-ims-agent --line N`'s existing dispatch shape): loads this line's settings from the
      manifest (T004; no independent re-discovery, contracts/volte-carrier-agent-contract.md), and
      calls `carrier_agent::run` (T007).
- [X] T009 [US1] Modify `run_inner` in `gsm-sip-bridge/src/volte/bridge.rs`: stop spawning one
      thread per line; spawn only the shared telephony thread (Agent B, unchanged in-process,
      default namespace), passing each line's **real** `veth_carrier_addr`/`veth_telephony_addr`
      (T006) to `crate::vowifi::run_telephony_side` in place of today's `LOOPBACK`
      (research.md R2). `volte-bridge` becomes Agent-B-only, permanently (FR-004b — no
      conditional path back to the old in-process arrangement).
- [X] T010 [US1] Add the per-line VoLTE loop to `docker/entrypoint.sh`'s VoLTE section (mirroring
      the existing `ensure_epdg_interface`/`start_line_tail` VoWiFi functions,
      contracts/volte-carrier-agent-contract.md's entrypoint contract):
      1. idempotent namespace creation,
      2. idempotent interface move (`ip link set <iface> netns <netns>`, research.md R5's
         three-way check: already-there / in-default-move-it / neither-wait-retry),
      3. idempotent veth pair creation,
      4. launch `ip netns exec <netns> "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG"
         volte-carrier-agent --line <idx>`, supervised.
      Runs for every resolved line **before** the shared `volte-bridge` (Agent B) starts (ordering
      obligation, contracts/volte-carrier-agent-contract.md).
- [X] T011 [US1] Extend `docker/entrypoint.sh`'s `config volte-shell-env` consumer (or add a
      parallel per-line shell-env emission analogous to `discover --shell-env`'s `LINE_*` bash
      arrays) so the loop in T010 has, per line: `card_id`, `modem_port`, `netns`,
      `veth_carrier_iface`/`veth_telephony_iface`, `veth_carrier_addr`/`veth_telephony_addr`. Wire
      the corresponding `print_*_shell_env` function in `gsm-sip-bridge/src/main.rs`.
- [X] T012 [US1] Fix ordering per research.md R6: the interface move in T010 step 2 MUST run
      before `carrier_agent::run` (T007) calls `attach()`/`netcfg::configure()` for that line —
      confirm this is structurally guaranteed by T010's launch order (the move happens in
      `entrypoint.sh` before the `ip netns exec` launch), not by a runtime check inside
      `carrier_agent.rs`. Add a code comment at the T007 call site cross-referencing research.md
      R5 so the ordering dependency is documented at the point a future change could break it.

**Checkpoint**: At this point, two VoLTE lines can be brought up with independent namespaces and
veth pairs, and quickstart.md steps 1-3 (startup, cross-line traffic capture, concurrent calls) can
be run live. User Story 1 is independently testable.

---

## Phase 4: User Story 2 - Existing Single-Line and Multi-Line Behavior Is Unchanged (Priority: P2)

**Goal**: No externally observable regression for an operator running one line today, or several
lines on different carriers today (spec FR-005–FR-007).

**Independent Test** (from spec.md): existing single-line and multi-line VoLTE test/quickstart
procedures pass unmodified against the namespaced implementation.

### Tests for User Story 2

- [X] T013 [P] [US2] In `gsm-sip-bridge/tests/test_volte_bridge.rs`: confirm the existing bridge
      lifecycle tests (attachment-loss-during-call deferral, `pre_renewal`/`attachment_check`
      wiring, SMS-route-both-ways) still pass unchanged against the `carrier_agent::run` entry
      point from T007 — same assertions, new call path. Add one explicit single-line (`index ==
      0`) test asserting the line resolves to `VolteConfig`'s unindexed `netns`/veth defaults
      (T006), i.e. isolation exists but changes nothing observable for the one-line case (FR-005).
      [Done: `test_volte_bridge.rs`'s existing lifecycle tests needed no changes and pass unchanged
      (confirmed via `cargo test --workspace`); the index-0-identity assertion was added as
      `volte::discovery::tests::index_zero_keeps_the_unindexed_netns_and_veth_defaults` instead —
      co-located with the derivation it tests, matching this file's own existing convention.]
- [X] T014 [P] [US2] In `gsm-sip-bridge/tests/test_volte_line_netns.rs`: a multi-line (2+),
      different-carrier scenario test confirming per-line PLMN/MCC/MNC derivation (already
      per-line since specs/018) is unaffected by the new namespace/veth fields — the two concerns
      are independent, and this test documents that independence rather than assuming it.
      [Revised on implementation: `ResolvedVolteLine`/discovery.rs carries no MCC/MNC field at
      all — unlike VoWiFi, VoLTE derives PLMN per-line at runtime (`carrier_agent::run` →
      `vowifi::plmn::derive_plmn`, an AT-command read), not during discovery resolution, so there
      is no discovery-level field for the new netns/veth fields to interfere with. Coverage
      actually added: `later_lines_derive_distinct_netns_and_veth_identifiers` (discovery.rs)
      confirms the new fields derive correctly across a multi-line table; the PLMN-independence
      claim is structural (different code path entirely, verified by reading `carrier_agent.rs`
      not by a new test) rather than something a discovery-level test could regress-guard.]

### Implementation for User Story 2

- [X] T015 [US2] Audit every place that read `LOOPBACK`-based addressing or assumed the carrier
      half and telephony half shared a process (`gsm-sip-bridge/src/volte/*.rs`,
      `gsm-sip-bridge/src/ims/agent.rs`'s VoLTE-specific branches if any) for a lingering
      same-process assumption T007-T009 broke silently; fix or add a comment explaining why it's
      still safe. This is a review task, not new logic — its output is either "no change needed"
      or a small fix, recorded in the task's completion note.
- [X] T016 [US2] Update `specs/017-volte-inbound-bridge` and prior multi-modem quickstart
      references, if any, that describe the in-process thread arrangement as current
      architecture, to point at this feature instead (docs consistency — avoid two contradictory
      "how VoLTE bridging works" descriptions in the tree).
      [Revised on implementation: this project's convention (confirmed against specs 011→012→013)
      is that a spec's own docs are a point-in-time record, never retroactively edited by a later
      feature — 012/015/017/018 were not edited when superseded either. Updated the *living* docs
      instead: `volte/bridge.rs` and `volte/discovery.rs`'s module-level doc comments now describe
      the current (020) architecture, which is what a future reader/editor actually consults.]

**Checkpoint**: User Stories 1 AND 2 both work independently; single-line deployments are
unaffected; existing multi-line (different-carrier) behavior is unaffected.

---

## Phase 5: User Story 3 - One Line's Network Failure Does Not Affect Another Line (Priority: P3)

**Goal**: A failure confined to one line's network setup is reported for that line alone; every
other line's registration and in-progress calls continue (spec FR-008/FR-009).

**Independent Test** (from spec.md): simulate one line's interface losing carrier or its
namespace/interface setup failing; confirm the other line is unaffected and the failure is
attributed to the correct card identifier (quickstart.md step 4).

### Tests for User Story 3

- [ ] T017 [P] [US3] In `gsm-sip-bridge/tests/test_volte_line_netns.rs`: a table-driven test that
      one line's interface-move failure (T010's three-way check exhausting its retry) does not
      prevent `resolve_volte_lines`/the per-line loop from proceeding to the remaining lines —
      mirroring `vowifi::discovery`'s existing "modem beyond max_lines is reported and skipped"
      test shape, adapted to "namespace/interface setup failed, reported and skipped."
      [Not implemented: the failure mode this task targets (`ensure_volte_line_netns` exhausting
      its retry against a real interface) is a `docker/entrypoint.sh` bash behavior, not something
      `resolve_volte_lines` (a pure Rust function with no netns/interface awareness) can exhibit —
      there is no unit-testable seam for it. The equivalent guarantee is implemented directly in
      bash (T018's `continue` on failure) and is exercised live per quickstart.md step 4, the same
      boundary this project draws for every namespace-dependent behavior (specs/012/013).]

### Implementation for User Story 3

- [X] T018 [US3] In `docker/entrypoint.sh`'s per-line VoLTE loop (T010): on namespace/interface
      setup failure, log clearly against that line's `card_id` and `continue` to the next line
      (mirroring `start_line_strongswan`/`start_line_swu`'s existing `return 1` /
      "skipping this line" pattern) rather than aborting the whole VoLTE subsystem.
- [X] T019 [US3] Add per-line process supervision for `volte-carrier-agent` in
      `docker/entrypoint.sh` (mirroring the existing `IMS_AGENT_SUPERVISOR_PIDS` pattern for
      `vowifi-ims-agent`): a crashed/exited carrier-agent for one line restarts only that line's
      process, never the container or another line's process.
- [X] T020 [US3] Implement research.md R6's teardown ordering in `docker/entrypoint.sh`'s
      `cleanup()` trap: for each started VoLTE line, run `ip netns exec <netns>
      "$GSM_SIP_BRIDGE_BIN" --config "$GSM_SIP_BRIDGE_CONFIG" volte-pdn --action down ...` (or
      `volte-cleanup` for the auto-discovered case) **before** that namespace is deleted, appending
      each VoLTE line's `netns` to the existing `STARTED_NETNS` array/deletion loop (currently
      VoWiFi-only) so both subsystems' namespaces are cleaned up the same way. Best-effort: a
      failure here must not hang container shutdown (matching every other cleanup step's existing
      tolerance).
- [ ] T021 [US3] Verify (live, per quickstart.md step 6) that an unclean shutdown
      (`docker kill -s KILL`) followed by a restart brings every line back up via T010's
      three-way idempotency check with no manual intervention — record the result in this task's
      completion note; this is an operator-run verification, not something automatable in
      `cargo test`.
      [Blocked: requires live hardware/namespaces (see T025's note). The idempotency logic itself
      (`ensure_volte_line_netns`'s three-way check) is implemented and code-reviewed; only the
      live verification is outstanding.]

**Checkpoint**: All three user stories are independently functional. A line's network failure is
contained; teardown and restart are robust.

---

## Phase 6: Polish & Cross-Cutting Concerns

**Purpose**: Documentation and final validation across all three stories.

- [X] T022 [P] Document the new `[volte]` config fields (`netns`, `veth_carrier_iface`,
      `veth_telephony_iface`, `veth_carrier_addr`, `veth_telephony_addr`) in
      `config.toml.example`, mirroring how `56cd35d` documented `[[vowifi.line]]` — including a
      note that these are not expected to be operator-tuned (data-model.md).
- [X] T023 [P] Update `docker/docker-compose.yml`'s top-of-file comment (the one describing what
      `privileged`/`network_mode: host` are for) to mention VoLTE's per-line namespaces alongside
      VoWiFi's, since the capability set already covers both (no compose change needed, comment
      currently describes only VoWiFi's use of it).
- [X] T024 Run `cargo fmt --all && make lint && cargo test --workspace` and fix anything the new
      code trips (unsafe-ratio check included — this feature adds zero `unsafe`, per plan.md's
      Technical Context constraint). [All green: 566 lib tests + every integration test file pass,
      0 failures; `make lint` reports 0 unsafe blocks in `gsm-sip-bridge/src`.]
- [ ] T025 Run quickstart.md end to end against real hardware (two same-carrier LTE modems) and
      record the result — this is the feature's actual acceptance bar (spec SC-001) and cannot be
      substituted with unit tests.
      [Blocked: no physical modems/network namespaces available in this environment (sandboxed, no
      root/CAP_NET_ADMIN — see the project's own `sandbox-blocks-root-network-testing` precedent).
      Everything gated behind hardware is deferred to the operator: quickstart.md steps 1-7,
      T017's live equivalent, and T021's unclean-shutdown restart check.]
- [X] T026 Update `CLAUDE.md`'s plan pointer (already retargeted to
      `specs/020-volte-line-netns/plan.md` as part of `/speckit-plan`) — confirm it still resolves
      correctly after this feature merges, or retarget to the next active feature if superseded.
      [Confirmed still correct.]

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies — can start immediately.
- **Foundational (Phase 2)**: Depends on Setup — BLOCKS all user stories.
- **User Story 1 (Phase 3)**: Depends on Foundational. This is the feature's MVP — the other two
  stories are hardening/compatibility around it and have limited meaning without it.
- **User Story 2 (Phase 4)**: Depends on Foundational; in practice also depends on US1's T007/T009
  existing (there is nothing to check for "unchanged behavior" against until the new call path
  exists) — sequence after US1 even though the checklist format doesn't encode that dependency.
- **User Story 3 (Phase 5)**: Depends on Foundational; in practice also depends on US1's T010
  (entrypoint per-line loop) existing, since T018-T020 modify that same loop — sequence after US1.
- **Polish (Phase 6)**: Depends on all three stories being complete.

### Within Each User Story

- Tests before implementation (T005 before T006; T017 before T018).
- Derivation (discovery.rs) before the code that consumes it (carrier_agent.rs, entrypoint.sh).
- Rust-side changes (T006-T009) before the `entrypoint.sh` changes that launch them (T010-T012),
  since the subcommand T010 shells out to must exist first.

### Parallel Opportunities

- T001/T002 (Setup) in parallel.
- T003/T004 (Foundational) in parallel — different files.
- T005 (US1 test) can be written in parallel with T003/T004, but must fail against a stub before
  T006 makes it pass (TDD).
- T013/T014 (US2 tests) in parallel — different files.
- T022/T023 (Polish docs) in parallel with T024/T025 (validation).

---

## Parallel Example: User Story 1

```bash
# Foundational, in parallel:
Task: "Add netns/veth VolteConfig fields in gsm-sip-bridge/src/config/mod.rs"
Task: "Add netns to VolteLineManifestEntry in gsm-sip-bridge/src/volte/discovery.rs"

# Then, sequential (each depends on the previous within US1):
Task: "Write failing derivation tests in tests/test_volte_line_netns.rs"
Task: "Implement per-line netns/veth derivation in volte/discovery.rs"
Task: "Extract carrier_agent.rs from bridge.rs"
Task: "Add volte-carrier-agent CLI subcommand"
Task: "Modify bridge.rs run_inner to Agent-B-only"
Task: "Add entrypoint.sh per-line loop"
```

---

## Implementation Strategy

### MVP First (User Story 1 Only)

1. Complete Phase 1: Setup.
2. Complete Phase 2: Foundational.
3. Complete Phase 3: User Story 1.
4. **STOP and VALIDATE**: run quickstart.md steps 1-3 against two same-carrier modems — this is
   the actual defect the feature closes; everything else is hardening around it.
5. Demo/deploy if ready.

### Incremental Delivery

1. Setup + Foundational → foundation ready.
2. User Story 1 → cross-line isolation proven live (quickstart steps 1-3) → this is the MVP.
3. User Story 2 → regression-checked against existing single-line/multi-line behavior.
4. User Story 3 → fault isolation and teardown ordering hardened (quickstart steps 4-6).
5. Polish → docs, full quickstart run (step 7), lint/test gate.

---

## Notes

- [P] tasks = different files, no dependencies.
- [Story] label maps task to specific user story for traceability.
- This feature is unusually low-risk for new abstraction (plan.md's Constitution Check): most
  tasks are *extraction* (T007) or *derivation shaped after existing code* (T006, mirroring
  `vowifi::discovery` line for line) rather than new design — keep it that way during
  implementation; if a task starts requiring a new trait or indirection layer to finish, stop and
  check it against `run_telephony_side`'s existing generality (research.md R2) before adding one.
- Commit after each task or logical group, per the constitution's Frequent Atomic Commits
  principle — this feature's own phase boundaries (discovery/derivation → carrier-agent extraction
  → entrypoint wiring → fault isolation) are natural commit boundaries, mirroring how 013 phased
  its structurally identical VoWiFi change.
