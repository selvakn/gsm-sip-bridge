# Tasks: Container Orchestration Move into the Rust Supervisor

**Input**: Design documents from `specs/021-entrypoint-supervise-rust/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/, quickstart.md

**Tests**: Included — the spec explicitly requires them (FR-003 snapshot tests, FR-004
ordering tests, FR-007 hardware-free unit testability, FR-009 named tests per invariant).
Unit tests live inline (`#[cfg(test)] mod tests`) in each `src/supervise/*.rs` file,
matching the existing convention in `gsm-sip-bridge/src/volte/netcfg.rs` — not in
`gsm-sip-bridge/tests/`.

**Organization**: Tasks are grouped by user story (US1-US4, matching spec.md's P1-P4),
preceded by Setup and Foundational phases. Phase 0 (bash safety net) is Foundational,
not its own user story — FR-010 requires it complete before any Rust port begins, and
it has no independent user-facing value on its own.

## Format: `[ID] [P?] [Story] Description`

---

## Phase 1: Setup

- [ ] T001 Add `insta` as a dev-dependency in `gsm-sip-bridge/Cargo.toml`; add the
      corresponding allowance in `deny.toml` (`[licenses]`/advisory check passes for it)
- [ ] T002 [P] Add a `test-bash` target to `Makefile` that runs `bats-core` over
      `docker/lib/*.bats` (fetch/vendor `bats-core` per research.md R5); fold it into the
      `test` target
- [ ] T003 [P] Add `shellcheck docker/*.sh` to the `lint` target in `Makefile`

**Checkpoint**: `make lint`/`make test` run (and currently pass trivially — nothing to
shellcheck-fail or bats-test yet beyond what Phase 0 adds next).

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Phase 0's bash safety net (must precede any Rust port, FR-010/FR-011) plus
the `supervise` module skeleton and `CommandRunner` trait every user story is built on.

**⚠️ CRITICAL**: No user story (US1-US4) work can begin until this phase is complete.

### Phase 0: bash safety net

- [ ] T004 Extract `extract_latest_pcscf` from `docker/entrypoint.sh` into
      `docker/lib/render_helpers.sh`, unchanged logic; `entrypoint.sh` sources it
- [ ] T005 [P] Extract `render_line_strongswan_conf`, `render_line_swanctl_conf`,
      `render_line_updown_script` from `docker/entrypoint.sh` into
      `docker/lib/render_helpers.sh`, unchanged logic; `entrypoint.sh` sources it
- [ ] T006 Write `docker/lib/render_helpers.bats` covering: `extract_latest_pcscf` picks
      the chronologically last valid P-CSCF line across mixed v4/v6 entries (the Greptile
      PR #2 case named in the source comment), tolerates no match; each `render_line_*`
      helper's output for representative inputs
- [ ] T007 Run `shellcheck` over `docker/entrypoint.sh` and `docker/lib/render_helpers.sh`,
      fix any findings without changing behavior
- [ ] T008 Verify `make lint && make test-bash` pass; commit ("Phase 0: extract pure bash
      helpers with bats coverage before the Rust port")

**Checkpoint (Phase 0 done)**: bash helpers behavior-locked by tests; safe to start porting.

### Supervise module skeleton

- [ ] T009 Create `gsm-sip-bridge/src/supervise/mod.rs` (empty `supervise` subcommand
      entry point) and add `pub mod supervise;` to `gsm-sip-bridge/src/lib.rs`
- [ ] T010 Define `CommandRunner` trait, `ChildSpec`, `ChildHandle(u64)`, `Signal` enum in
      `gsm-sip-bridge/src/supervise/runner.rs` per data-model.md
- [ ] T011 [P] Implement `RealCommandRunner` (backed by `std::process::Command`/`Child`,
      `std::fs`, an internal `Mutex<HashMap<u64, Child>>` for handle→child mapping) in
      `gsm-sip-bridge/src/supervise/runner.rs`
- [ ] T012 [P] Implement `MockCommandRunner` (in-memory call log + injectable
      observations/liveness/exit-codes) in `gsm-sip-bridge/src/supervise/runner.rs`, gated
      `#[cfg(test)]`
- [ ] T013 Add `Supervise` and `Render(RenderArgs)` variants to the `Commands` enum in
      `gsm-sip-bridge/src/cli.rs` (stubs — not yet wired to logic)
- [ ] T014 Verify `cargo fmt --all && make lint && cargo test --workspace` pass; commit
      ("supervise: module skeleton + CommandRunner trait + Real/Mock impls")

**Checkpoint**: Foundation ready — US1-US4 can now proceed in order (spec FR-010 mandates
strict 0→1→2→3→4 ordering, so "in order" not "in parallel" despite normal task-template
guidance).

---

## Phase 3: User Story 1 - Config/asset rendering is verified by tests (Priority: P1) 🎯 MVP

**Goal**: strongswan.conf / swanctl.conf ePDG connection / updown wrapper / vpcd
reader.conf rendering becomes pure, snapshot-tested Rust — no more heredoc/`sed`.

**Independent Test**: `gsm-sip-bridge render <asset> --line N ...` output is
byte-for-byte identical to the current script's heredoc/`sed` output for the same inputs;
`cargo test` catches a wrong substitution before any image is built.

### Tests for User Story 1

- [ ] T015 [P] [US1] insta snapshot tests for `render_strongswan_conf` (varying `idx`,
      `vici_socket`, `charon_log`) in `gsm-sip-bridge/src/supervise/render.rs`
- [ ] T016 [P] [US1] insta snapshot tests for `render_swanctl_epdg`, **including both the
      `src_addr: Some(..)` and `src_addr: None` branches** (spec Acceptance Scenarios 1-2)
      in `gsm-sip-bridge/src/supervise/render.rs`
- [ ] T017 [P] [US1] insta snapshot tests for `render_updown_script` and
      `render_vpcd_reader_conf` in `gsm-sip-bridge/src/supervise/render.rs`

### Implementation for User Story 1

- [ ] T018 [US1] Implement `render_strongswan_conf(idx, vici_socket, charon_log) -> String`
      in `gsm-sip-bridge/src/supervise/render.rs`, 1:1 port of `render_line_strongswan_conf`
      (depends on T015 existing to fail first)
- [ ] T019 [US1] Implement `SwanctlEpdgParams` + `render_swanctl_epdg(params) -> String` in
      `gsm-sip-bridge/src/supervise/render.rs`, 1:1 port of `render_line_swanctl_conf` incl.
      the `@SRC_ADDR@` present/absent branch (depends on T016)
- [ ] T020 [US1] Implement `render_updown_script(idx, netns, tun_iface) -> String` and
      `render_vpcd_reader_conf(port) -> String` in `gsm-sip-bridge/src/supervise/render.rs`
      (depends on T017)
- [ ] T021 [US1] Add `RenderArgs`/asset-kind enum and wire the `Render` subcommand (from
      T013) to call the `render_*` functions and print to stdout, per
      contracts/render-contract.md, in `gsm-sip-bridge/src/cli.rs` + `src/supervise/mod.rs`
- [ ] T022 [US1] `docker/entrypoint.sh`: replace the `render_line_strongswan_conf`/
      `render_line_swanctl_conf`/`render_line_updown_script` heredoc functions (still in
      `docker/lib/render_helpers.sh` after Phase 0) with calls to
      `gsm-sip-bridge render <asset> ...`; delete the now-dead bash heredoc functions and
      `docker/lib/render_helpers.sh`'s render_* portion (keep `extract_latest_pcscf` there
      until Phase 3 ports it)
- [ ] T023 [US1] Diff Rust-rendered output against the pre-refactor script's output for a
      representative line config (manual verification per quickstart.md; record the
      command used and result in DECISIONS-LOG.md)
- [ ] T024 [US1] Verify `cargo fmt --all && make lint && cargo test --workspace` pass;
      live-validate per quickstart.md (`make docker-build && make docker-up`, confirm
      strongSwan/swanctl accept the Rust-rendered configs against the real EC20 + Airtel
      SIM); commit ("Phase 1: pure Rust config-asset rendering, entrypoint.sh calls
      `render` instead of heredocs")

**Checkpoint**: User Story 1 fully functional — rendering is unit-tested and
behavior-identical live.

---

## Phase 4: User Story 2 - Shutdown is an ordered, tested plan (Priority: P2)

**Goal**: Teardown becomes a typed `ShutdownPlan` with tested ordering invariants,
replacing the ~15 PID arrays and the `cleanup()` trap.

**Independent Test**: Build the plan from synthetic started-line records and assert the
emitted step sequence — no processes actually killed.

### Tests for User Story 2

- [ ] T025 [P] [US2] Unit test: every line's child-kill steps precede that line's
      PDN/namespace-teardown steps, in `gsm-sip-bridge/src/supervise/shutdown.rs`
- [ ] T026 [P] [US2] Unit test: a VoLTE line's `volte-cleanup` step is `RunInNetns`-scoped
      and precedes that namespace's `DeleteNetns`, in
      `gsm-sip-bridge/src/supervise/shutdown.rs`
- [ ] T027 [P] [US2] Unit test: any child that may block mid-AT-transaction (USIM-bridge
      holder, VoLTE register/bridge/carrier-agent) gets `Signal::Kill`, never `Signal::Term`
      — named after the invariant it protects, in `gsm-sip-bridge/src/supervise/shutdown.rs`

### Implementation for User Story 2

- [ ] T028 [US2] Define `TeardownStep`, `ShutdownPlan`, `StartedState` in
      `gsm-sip-bridge/src/supervise/shutdown.rs` per data-model.md (depends on T025-T027
      existing to fail first)
- [ ] T029 [US2] Implement `build_shutdown_plan(&StartedState) -> ShutdownPlan`, porting
      the current `cleanup()` trap's ordering logic (VoWiFi child kills → volte-cleanup →
      per-namespace volte-pdn down/restore-cid → VoLTE-multiline per-netns cleanup →
      pcscd → netns deletion) into step construction
- [ ] T030 [US2] Implement `ShutdownPlan::execute(&dyn CommandRunner)` — the thin executor,
      the only place a real signal is sent
- [ ] T031 [US2] Wire `supervise`'s top-level signal handler (`SIGINT`/`SIGTERM`) to build
      `StartedState` from what actually started this run and call `execute`, in
      `gsm-sip-bridge/src/supervise/mod.rs`
- [ ] T032 [US2] Verify `cargo fmt --all && make lint && cargo test --workspace` pass;
      live-validate per quickstart.md (SIGTERM mid-call and mid-tunnel-establish against
      the real EC20 + Airtel SIM; confirm PDN context restored via `AT+CGACT?`/
      `AT+CGDCONT?` before/after, `ip netns list` empty after exit); commit ("Phase 2:
      typed ShutdownPlan replaces the PID-array cleanup() trap")

**Checkpoint**: User Stories 1 AND 2 both work; teardown correctness is now testable.

---

## Phase 5: User Story 3 - One tested supervision state machine (Priority: P3)

**Goal**: `LineSupervisor` collapses the three duplicated bash loops (strongswan
establish-time, strongswan steady-state, swu) plus the GSM daemon's own restart loop
into tested state machines over the injected `CommandRunner`.

**Independent Test**: Feed synthetic observations through `MockCommandRunner`, assert
the exact command sequence and state transitions — no real charon/pcscd/swu/agent.

### Tests for User Story 3

- [ ] T033 [P] [US3] Table-driven tests for each transition in data-model.md's transition
      table (Establishing→Up, stuck-without-P-CSCF re-initiate, ProcessDied, ViciBroken,
      TunVanished, ChildSaMissing, PcscfChanged) in
      `gsm-sip-bridge/src/supervise/line_supervisor.rs`, each `MockCommandRunner` use
      annotated `// MOCK JUSTIFICATION: stands in for {charon,pcscd,swanctl,a live modem}`
      per the constitution's Integration-First Testing principle
- [ ] T034 [P] [US3] Pure-function tests for every log-parsing check ported from bash
      (CHILD_SA-established detection, `AT+CSIM failed` detection, `^ims:` CHILD_SA
      presence in `list-sas` output) in `gsm-sip-bridge/src/supervise/line_supervisor.rs`
      and `gsm-sip-bridge/src/supervise/sim_recovery.rs` — each fixture taken from the
      bash comment that motivated the original check (e.g. the Greptile PR #2
      last-matching-line case)
- [ ] T035 [P] [US3] Restart-loop tests for `daemon_supervisor::run_supervised` (spawn,
      wait, 5s-delay respawn) using `MockCommandRunner`, in
      `gsm-sip-bridge/src/supervise/daemon_supervisor.rs`

### Implementation for User Story 3

- [ ] T036 [US3] Define `LineState`, `DegradeReason`, `TunnelEngine` trait in
      `gsm-sip-bridge/src/supervise/line_supervisor.rs` per data-model.md/research.md R6
      (depends on T033 existing to fail first)
- [ ] T037 [US3] Implement `StrongswanEngine: TunnelEngine` (reinitiate via `swanctl`,
      `is_established`/`extract_pcscf` from charon.log) in
      `gsm-sip-bridge/src/supervise/line_supervisor.rs`, 1:1 port of
      `start_line_strongswan`'s establish + steady-state loops, all sleep/poll/threshold
      constants preserved and named (`VICI_SETTLE_DELAY = 2s`, `REINITIATE_EVERY = 15`
      ticks, `STEADY_STATE_POLL = 30s`, etc.)
- [ ] T038 [US3] Implement `SwuEngine: TunnelEngine` (respawn dialer, parse P-CSCF from
      dialer log) in `gsm-sip-bridge/src/supervise/line_supervisor.rs`, 1:1 port of
      `start_line_swu`'s establish + steady-state loops
- [ ] T039 [US3] Implement `LineSupervisor::tick(&dyn CommandRunner) -> LineState` driving
      both engines through the shared transition table (depends on T036-T038)
- [ ] T040 [US3] Implement `sim_recovery::reset_line_sim` decision logic (CSIM-failure
      counting, `MAX_SIM_RESETS` cap, holder freeze/resume via `Signal::Stop`/`Cont`,
      readiness polling) in `gsm-sip-bridge/src/supervise/sim_recovery.rs`, 1:1 port of
      `reset_line_sim`/`start_line_tail`'s counters (depends on T034)
- [ ] T041 [US3] Implement `daemon_supervisor::run_supervised` per data-model.md (depends
      on T035 existing to fail first)
- [ ] T042 [US3] Wire `supervise::mod` to start one `LineSupervisor` thread per resolved
      line (matching the existing `std::thread::spawn` convention, not `tokio::spawn` per
      plan.md's Constraints) and the daemon supervisor, replacing entrypoint.sh's three
      bash loops and its top-level daemon loop
- [ ] T043 [US3] Verify `cargo fmt --all && make lint && cargo test --workspace` pass;
      live-validate per quickstart.md (kill `charon` mid-session, force a P-CSCF rekey,
      trigger the CSIM-failure auto-recovery path, verify identical recovery timing)
      against the real EC20 + Airtel SIM; commit ("Phase 3: one LineSupervisor state
      machine replaces three duplicated bash loops; daemon supervision moves to Rust")

**Checkpoint**: User Stories 1-3 all work; every duplicated supervision loop is now one
tested implementation.

---

## Phase 6: User Story 4 - The entrypoint is a thin, auditable shim (Priority: P4)

**Goal**: `docker/entrypoint.sh` shrinks to ~50 lines: precondition checks +
`exec gsm-sip-bridge supervise`.

**Independent Test**: Confirm the shim's precondition checks and exec behavior; full
multi-line VoWiFi (both engines) + VoLTE deployment behaves identically to the
pre-refactor image on real hardware.

### Tests for User Story 4

- [ ] T044 [US4] Unit tests for `supervise::mod`'s top-level orchestration (discover once
      up front, VoWiFi/VoLTE mutual exclusion fatal exit, "no usable line" prominent-error
      but keep-running, vpcd-reader readiness gate) using `MockCommandRunner`, in
      `gsm-sip-bridge/src/supervise/mod.rs`

### Implementation for User Story 4

- [ ] T045 [US4] Move the remaining `entrypoint.sh` logic (discover-once-up-front
      sequencing, VoWiFi/VoLTE mutual exclusion check, vpcd-reader readiness gate,
      per-line dispatch to `LineSupervisor`, shared `vowifi-sip-agent`/`volte-bridge`
      startup) into `gsm-sip-bridge/src/supervise/mod.rs`
- [ ] T046 [US4] Reduce `docker/entrypoint.sh` to precondition checks (binary executable,
      config file present) + `exec gsm-sip-bridge --config "$GSM_SIP_BRIDGE_CONFIG"
      supervise`, matching contracts/supervise-contract.md exactly; delete
      `docker/lib/render_helpers.sh` and its `.bats` file entirely (fully superseded)
- [ ] T047 [US4] Verify `cargo fmt --all && make lint && cargo test --workspace` pass
      (`make test-bash` no longer applicable — remove the now-empty target or leave a
      no-op with a comment explaining Phase 0's tooling was retired here)
- [ ] T048 [US4] Full live-validation cold-start + warm-restart cycle per quickstart.md:
      VoWiFi under both `strongswan` and `swu` engines (if a second SIM/profile is
      available; otherwise `strongswan` + a note in DECISIONS-LOG.md) plus VoLTE, against
      the real EC20 + Airtel SIM, confirming parity with the pre-refactor image; commit
      ("Phase 4: entrypoint.sh reduced to a thin shim over `gsm-sip-bridge supervise`")

**Checkpoint**: All user stories complete. `docker/entrypoint.sh` contains no config-asset
heredocs, no supervision/restart loops, no global PID-array bookkeeping (SC-006).

---

## Phase 7: Polish & Cross-Cutting Concerns

- [ ] T049 [P] Re-read every FR-009-targeted comment in the original `entrypoint.sh` (via
      `git show <pre-refactor-commit>:docker/entrypoint.sh`) and confirm each has a named
      test counterpart; list any gaps in DECISIONS-LOG.md
- [ ] T050 [P] Update `docs/` (or repo docs referencing `docker/entrypoint.sh`'s size/
      structure) if any exist, to describe the new `supervise` module instead
- [ ] T051 Run `make coverage` (if available) and note the `supervise` module's coverage
      in DECISIONS-LOG.md
- [ ] T052 Final full `quickstart.md` pass end-to-end; update DECISIONS-LOG.md with a
      completion summary and any deferred/outstanding items for review

---

## Dependencies & Execution Order

- **Setup (Phase 1)** → **Foundational (Phase 2, incl. Phase 0)** → **US1 (Phase 3)** →
  **US2 (Phase 4)** → **US3 (Phase 5)** → **US4 (Phase 6)** → **Polish (Phase 7)**.
- Unlike the general template's default (user stories parallelizable), **this feature's
  own FR-010 mandates strict sequential phase order** — each phase is a strangler step
  that must be live-validated before the next begins. US2 depends on US1 having
  established the `render`/`CommandRunner` plumbing patterns; US3 depends on US2's
  `StartedState`/`ShutdownPlan` existing (a `LineSupervisor`-started line must register
  into `StartedState` the same way); US4 depends on US1-US3 all being in place to move.
- Within each user story: tests before implementation (write the test, watch it fail,
  implement).

## Parallel Opportunities

- T002/T003 (Setup) in parallel.
- T005 alongside T004 (different bash functions, same destination file — still fine
  since bats/shellcheck run after both land; mark [P] loosely, verify no merge conflict).
- T011/T012 (Real/Mock runner impls) in parallel once T010's trait exists.
- Within each user story's test phase, all listed [P] test tasks in parallel (different
  assertions, same file — acceptable since they're additive `#[test]` functions).

## Implementation Strategy

**MVP = User Story 1 (Phase 3)**: rendering alone already delivers the spec's core
promise (a maintainer's config-asset change gets a pass/fail signal from `cargo test`
before any image is built) and is the lowest-risk, highest-value slice. Stop and validate
there before continuing if time runs short — every later phase depends on it anyway.

**Full delivery**: Phases 1-7 in order, live-validated at each of the four
strangler-phase checkpoints (T024, T032, T043, T048) against the physical EC20 + Airtel
SIM per quickstart.md.
