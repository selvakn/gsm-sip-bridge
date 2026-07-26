# Implementation Plan: Container Orchestration Move into the Rust Supervisor

**Branch**: `021-entrypoint-supervise-rust` | **Date**: 2026-07-26 | **Spec**: [./spec.md](./spec.md)
**Input**: Feature specification from `specs/021-entrypoint-supervise-rust/spec.md`

## Summary

`docker/entrypoint.sh` (~1,350 lines) mixes bootstrap, config-asset rendering (heredocs+`sed`),
per-line process supervision (three near-duplicate state machines), and lifecycle/teardown (a trap
over ~15 hand-tracked global PID arrays) into one bash script. None of the high-risk parts are
unit-testable, and the operational knowledge that governs them lives only in comments.

This feature moves that orchestration into a new `supervise` subcommand inside the existing
`gsm-sip-bridge` binary, following the pattern the binary already established in
`src/volte/netcfg.rs`: pure decision functions (`*_steps() -> Vec<NetStep>`) plus a thin executor
(`run_step`), unit-tested on the pure part. Concretely:

- **Rendering** (strongswan.conf, swanctl.conf, updown wrapper, vpcd reader.conf) becomes pure
  `String`-returning functions, snapshot-tested with `insta`.
- **Teardown** becomes an owned, ordered `ShutdownPlan` of typed `TeardownStep`s built from the
  started-line records — replacing the PID arrays and the `cleanup()` trap.
- **Supervision** (charon/pcscd/swu/carrier-agent/GSM-daemon restart, vici-broken detection,
  P-CSCF-change handling, tun-vanished recovery) becomes one `LineSupervisor` state machine plus a
  daemon-supervision loop, both driven over an injected `CommandRunner` that owns spawning,
  signalling, and liveness-checking of every long-running child (clarified 2026-07-26: the runner
  is not limited to transient leaf commands — it is the real process-ownership boundary).
- `docker/entrypoint.sh` shrinks in the same four phases the spec lays out, ending as a ~50-line
  shim that execs `gsm-sip-bridge supervise`.

The move is a strangler: each phase ships, is live-validated, and leaves the container fully
functional before the next begins.

## Technical Context

**Language/Version**: Rust stable (pinned by `rust-toolchain.toml`), unchanged. `bash` for the
shrinking `docker/entrypoint.sh` and its Phase 0 test harness.

**Primary Dependencies**: No new *runtime* crate. Two new *dev*-only additions, both already
resolved via clarification (2026-07-26):
- `insta` (Rust snapshot-testing crate, dev-dependency) for Phase 1 rendering snapshots and Phase 2
  teardown-plan snapshots.
- `bats-core` (bash TAP test harness, invoked from `make test`/CI, not a Cargo dependency) for
  Phase 0's pure-bash-helper tests.
Both require a `deny.toml` allowance (dev-dependency license/advisory check for `insta`; `bats-core`
is a system/CI tool, not a Rust dependency, so it does not touch `deny.toml` directly).

**Storage**: None new. The existing line-table structures (`vowifi::discovery`'s resolved lines,
`volte::discovery`'s `VolteLineManifest`) are consumed as-is; `supervise` reads them the same way
the shell-env/discover subcommands already produce them for `entrypoint.sh` today, but in-process
(no `eval`-a-shell-string boundary once Phase 1+ lands).

**Testing**: `cargo test --workspace` (unchanged gate). Concretely:
- Rendering: `insta` snapshot tests over pure functions — no process execution.
- Teardown: unit tests asserting the built `Vec<TeardownStep>`'s order/target/signal fields against
  fixtures — no process execution.
- Supervision: table-driven tests feeding synthetic runner observations (log content, command
  output, liveness) through a `MockCommandRunner`, asserting the emitted command sequence and state
  transitions — no real charon/pcscd/swu/agent processes, no root, no hardware. Constitution
  Principle I requires a written justification at each mock site: charon/pcscd/swanctl/real serial
  hardware are exactly the "hardware not available in CI" carve-out the constitution names.
- Phase 0: `bats-core` over the extracted pure bash helpers (`extract_latest_pcscf`,
  `render_line_*`) — no process execution, no hardware.
- The property no unit test can prove — that the real `RealCommandRunner` correctly drives real
  `charon`/`pcscd`/`swanctl`/a real EC20 modem end to end — is validated live at each phase boundary
  against the physical EC20 + Airtel SIM, the same boundary specs/012/013/020 already drew.

**Target Platform**: Linux, the existing image, `privileged: true` + `network_mode: host`
(`docker-compose.yml`, unchanged) — `supervise` needs exactly the capabilities `entrypoint.sh`
already runs under today (`CAP_NET_ADMIN`/`CAP_SYS_ADMIN` for netns/veth/XFRM, direct serial access).

**Project Type**: Extension of the existing `gsm-sip-bridge` binary (new `supervise`/`render`
subcommands + a new `supervise` module) + a shrinking deployment shim (`docker/entrypoint.sh`). No
new crate, no new workspace member.

**Performance Goals** (from spec Success Criteria): no behavior change is itself the goal — see
SC-001..SC-008. All existing timing/cadence constants (2s vici-settle sleep, 15×2s reinitiate
cadence, 30s steady-state poll, CSIM-failure threshold=3, MAX_SIM_RESETS=5, 15s CPIN poll, etc.)
are preserved as named Rust constants, not retuned.

**Constraints**:
- **Zero new `unsafe` in `gsm-sip-bridge/src`** (unchanged gate, `tools/count-unsafe.sh`) — satisfied
  by design: `supervise` shells out via `std::process::Command`/`std::process::Child`, exactly
  `netcfg.rs`'s and `ims/agent.rs`'s existing convention. No FFI, no raw `fork`/`setns()`.
- **Concurrency model matches existing convention**: the codebase supervises concurrent per-line
  work with `std::thread::spawn` + blocking `std::process::Command`/`Child` everywhere it exists
  today (`vowifi/mod.rs`, `vowifi/usim_bridge.rs`, `ims/agent.rs`) — never `tokio::spawn` for this
  kind of long-lived supervisory work (tokio is used only for the axum metrics/control HTTP
  surface). `supervise`'s `LineSupervisor`/daemon-supervision loops use the same OS-thread model,
  not async tasks — introducing a second concurrency shape for equivalent work would be exactly the
  kind of avoidable complexity Constitution V forbids.
- Full pre-commit gate unchanged: `cargo fmt --all`, `make lint`, `cargo test --workspace`
  (CLAUDE.md's mandatory checklist), plus the new `bats-core`/`shellcheck` coverage folded into
  `make lint`/`make test` per FR-011.
- Behavior-preservation is normative (spec FR-008/FR-009): every load-bearing comment in the current
  script becomes a named test; every timing constant is preserved exactly.
- Strangler ordering is normative (spec FR-010): Phase 0 → 1 → 2 → 3 → 4, each independently
  shippable and live-validated before the next begins.

**Scale/Scope**: Up to `[vowifi]`/`[volte].max_lines` (existing bound, default 8) concurrent lines,
unchanged. This feature changes *where* orchestration logic lives, not the deployment's scale.

## Constitution Check

*Gate: must pass before Phase 0 research. Re-checked after Phase 1 design.*

| Principle | Assessment | Status |
|---|---|---|
| **I. Integration-First Testing** | The pure parts (rendering, teardown-plan construction, log-content parsing, state-transition logic) are unit-tested directly against real inputs/outputs — no mocking needed, matching how `netcfg.rs`'s `configure_steps`/`parse_ip_addr_show` are tested today. The one deliberate mock boundary is `CommandRunner` in `LineSupervisor` tests, standing in for `charon`/`pcscd`/`swanctl`/a live modem — precisely "hardware not available in CI," the constitution's named carve-out. Each mock use carries a written justification comment per the constitution's requirement. The property mocks cannot prove — real `charon`/`pcscd`/`swanctl`/EC20 driven correctly end-to-end — is validated live at each phase boundary against the physical modem, the same boundary every prior VoWiFi/VoLTE feature (012/013/020) has drawn. | ✅ PASS (with documented, spec-anticipated mock use) |
| **II. Green-on-Commit** | `make format && make lint && make test` before every commit; the new Phase 0 bash tests and Phase 1+ Rust tests all run without root, hardware, or a real modem — they gate every commit the same way existing tests do. | ✅ PASS |
| **III. Frequent Atomic Commits** | The spec's own phase structure (0→1→2→3→4) is the commit structure: each phase is independently shippable, each leaves `make test`/`make lint` green, matching how specs/013/020 phased structurally identical multi-part changes. | ✅ PASS |
| **IV. Makefile-Driven Build** | No new entry points outside the existing `gsm-sip-bridge` binary and `make` targets; `make lint`/`make test` gain bash-side coverage (shellcheck, bats) but remain the single entry point — no new tool a contributor must learn outside `make`. | ✅ PASS |
| **V. Simplicity & Refactorability** | This is a convergence onto an already-proven in-tree pattern (`netcfg.rs`'s steps-as-data + thin executor), not a new abstraction invented for this feature. The `CommandRunner` boundary is the one new trait this feature introduces; it is justified because it is the *only* way FR-007 (decision logic testable without hardware/root) can be satisfied, and it replaces three duplicated bash state machines with one — net reduction in the number of shapes to maintain, consistent with how 020 justified its own convergence. | ✅ PASS |

**Post-Phase-1 re-check**: ✅ Still passing. `data-model.md`/`contracts/` add exactly one new trait
(`CommandRunner`, justified above) and one small per-engine trait (`TunnelEngine`, R6 in
`research.md`) needed to collapse the strongswan/swu duplication into one state machine — no other
new indirection layer, no configuration surface added. The render/shutdown/render-contract designs
are direct 1:1 ports of existing heredoc/trap logic into pure functions plus a thin executor,
matching `netcfg.rs`'s existing shape file-for-file.

## Project Structure

### Documentation (this feature)

```text
specs/021-entrypoint-supervise-rust/
├── plan.md                      # This file
├── research.md                  # Phase 0 output
├── data-model.md                # Phase 1 output
├── quickstart.md                # Phase 1 output
├── contracts/
│   ├── supervise-contract.md    # `gsm-sip-bridge supervise` CLI/runtime contract
│   └── render-contract.md       # `gsm-sip-bridge render` CLI contract
├── checklists/
│   └── requirements.md
├── DECISIONS-LOG.md              # Running log of autonomous judgment calls (this session)
└── tasks.md                      # Phase 2 output (/speckit-tasks)
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/
├── supervise/                   # NEW module — all orchestration decision logic
│   ├── mod.rs                   #   `supervise` subcommand entry point; wires
│   │                            #   discover/config resolution + daemon +
│   │                            #   line supervisors + shutdown plan together
│   ├── runner.rs                #   CommandRunner trait: transient leaf commands
│   │                            #   (`run(argv) -> Output`, `read_file(path)`)
│   │                            #   PLUS long-running child ownership
│   │                            #   (`spawn`/`signal`/`is_alive`) — RealCommandRunner
│   │                            #   (std::process::Command/Child, std::thread) +
│   │                            #   MockCommandRunner (test-only, records calls,
│   │                            #   lets tests inject observations)
│   ├── render.rs                #   Phase 1: pure `render_strongswan_conf`,
│   │                            #   `render_swanctl_epdg`, `render_updown_script`,
│   │                            #   `render_vpcd_reader_conf` — ports the
│   │                            #   heredoc+sed logic 1:1, insta-snapshotted
│   ├── shutdown.rs               #   Phase 2: `TeardownStep`, `ShutdownPlan`,
│   │                            #   `build_shutdown_plan(&StartedLines) -> ShutdownPlan`
│   │                            #   — pure — plus `ShutdownPlan::execute(&dyn CommandRunner)`
│   ├── line_supervisor.rs        #   Phase 3: `LineState` enum
│   │                            #   (Establishing/Up/Degraded{reason}/Restarting),
│   │                            #   `LineSupervisor::tick(&dyn CommandRunner)`,
│   │                            #   ports the strongswan steady-state loop, the
│   │                            #   establish-time wait loop, and the swu loop
│   │                            #   into one transition table
│   ├── daemon_supervisor.rs      #   circuit-switched GSM-to-SIP daemon's restart
│   │                            #   loop (currently entrypoint.sh's simplest
│   │                            #   supervised block) — moved per clarification
│   │                            #   so entrypoint.sh retains zero loops
│   └── sim_recovery.rs           #   USIM AT+CFUN auto-recovery
│                                #   (reset_line_sim/start_line_tail's CSIM-fail
│                                #   counting) — decision logic pure/testable,
│                                #   raw AT I/O behind CommandRunner
├── lib.rs                       # MODIFY: `pub mod supervise;`
└── cli.rs                       # MODIFY: `Supervise` and `Render(RenderArgs)` subcommands

docker/
├── entrypoint.sh                 # MODIFY (shrinking each phase) → Phase 4 end
│                                #   state: precondition checks + `exec
│                                #   gsm-sip-bridge supervise`
└── lib/                          # NEW (Phase 0 only, retired by Phase 1)
    ├── render_helpers.sh         #   extracted extract_latest_pcscf, render_line_*
    └── render_helpers.bats       #   bats-core tests for the above

gsm-sip-bridge/src/supervise/*.rs  # Each module's own `#[cfg(test)] mod tests { ... }`,
                                  #   matching netcfg.rs's existing convention (inline,
                                  #   not gsm-sip-bridge/tests/) — insta snapshots,
                                  #   ShutdownPlan ordering, LineSupervisor table-driven
                                  #   tests w/ mock runner, and daemon-supervision tests
                                  #   all live next to the code they test.

Makefile                          # MODIFY: `lint` gains shellcheck over docker/*.sh;
                                  #   `test` gains a bats-core invocation (Phase 0)
deny.toml                         # MODIFY: allowance for `insta` dev-dependency
```

**Structure Decision**: Single Rust workspace, unchanged crate layout — `supervise` is a new module
inside the existing `gsm-sip-bridge` binary crate, not a new workspace member (there is no reuse
case outside this binary, and a new crate would add a moving part Constitution V doesn't justify).
`docker/lib/` is a deliberately temporary Phase-0-only scaffold: it exists to give the
about-to-be-deleted bash helpers a safety net for the one phase during which they still exist in
bash, and disappears once Phase 1 ports them to Rust.

## Complexity Tracking

*No unjustified violations.* The one new trait (`CommandRunner`) is the mechanism FR-007 requires
and is justified in the Constitution Check table above; everything else is either a direct 1:1 port
of existing logic (rendering, teardown ordering) or a consolidation of three existing duplicated
loops into one (`LineSupervisor`) — a reduction in shapes-to-maintain, not an addition.
