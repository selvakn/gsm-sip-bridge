# Feature Specification: Container Orchestration Move into the Rust Supervisor

**Feature Branch**: `021-entrypoint-supervise-rust`
**Created**: 2026-07-26
**Status**: Draft
**Input**: User description: "Refactor the container orchestration currently in docker/entrypoint.sh into the gsm-sip-bridge Rust binary as a testable `supervise` subcommand, using a strangler-fig approach so entrypoint.sh shrinks to a thin shim over time."

## Overview

`docker/entrypoint.sh` has grown to ~1,350 lines that mix four distinct
responsibilities: (1) bootstrap and config resolution, (2) rendering of config
assets (strongswan.conf, swanctl.conf, updown wrappers, vpcd reader.conf) via
heredocs and `sed`, (3) per-line process-supervision state machines
(charon/pcscd/swu/carrier-agent restart, vici-broken detection, P-CSCF-change
handling, tun-vanished recovery — a sequence duplicated in three places), and
(4) lifecycle/teardown (an `EXIT/INT/TERM` trap coordinating ~15 hand-tracked
global PID arrays with ordering-critical SIGKILL and namespace-scoped cleanup).

The high-risk parts (2, 3, 4) are not unit-testable, and the hard-won
operational knowledge that governs them lives only in comments — so an edit can
silently violate an invariant and only fail on live hardware. This feature moves
that orchestration into the `gsm-sip-bridge` binary, which already owns every
leaf operation and already demonstrates the target pattern in
`src/volte/netcfg.rs` (pure `*_steps() -> Vec<NetStep>` decision functions plus a
thin `run_step` executor, with unit tests over the pure part). The move is
executed as a strangler-fig: `entrypoint.sh` shrinks phase by phase, remaining a
working supervisor at every step, until it is a thin shim that `exec`s the Rust
supervisor.

The unit of value is **maintainability and behavior-preserving testability**.
This feature explicitly does NOT change what the container does on real hardware
— only where the orchestration logic lives and how it is verified.

## Clarifications

### Session 2026-07-26

- Q: What should the injectable command-runner abstract — how much does the Rust
  supervisor actually own? → A: The runner spawns AND owns the long-running
  children (charon, pcscd, agents, keepalives), returning handles/PIDs;
  `supervise` is the real in-process supervisor doing spawn + signal + wait +
  liveness. The runner is not limited to transient leaf commands.
- Q: Is the always-on circuit-switched GSM-to-SIP daemon's supervision (the
  top-level restart loop) also moved into `supervise`? → A: Yes — its restart
  loop moves into the Rust supervisor alongside VoWiFi/VoLTE, so the final
  `entrypoint.sh` contains no supervision loops at all.
- Q: May the work add new dependencies for testing (repo has deny.toml /
  cargo-deny gating)? → A: Yes — a Rust snapshot crate (`insta`) for
  render/teardown snapshots and `bats-core` for the Phase 0 bash tests are
  permitted; add the corresponding `deny.toml` allowance.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Config/asset rendering is verified by tests, not by live deployment (Priority: P1)

A maintainer changes how a per-line strongSwan/swanctl/updown/vpcd asset is
generated (for example, adjusting the `@SRC_ADDR@` handling or a filelog field).
Today the only way to know the rendered file is correct is to build the image
and deploy it against a modem. After this story, the rendering is a pure
function with snapshot tests, so a wrong substitution or dropped directive fails
in `cargo test` before any image is built.

**Why this priority**: Highest value, lowest risk. Rendering bugs are silent
(a malformed config file) and currently only surface live. Extracting pure
functions is behavior-preserving and independently shippable, and it establishes
the pattern the later phases follow.

**Independent Test**: Run `gsm-sip-bridge render <asset> --line N` (and the
corresponding `cargo test`) and diff the output against the byte-for-byte
heredoc output the current script produces for the same inputs; the snapshot
suite covers both the `@SRC_ADDR@`-present and `@SRC_ADDR@`-absent branches.

**Acceptance Scenarios**:

1. **Given** a line with a source address configured, **When** the swanctl ePDG
   connection asset is rendered, **Then** the output is byte-for-byte identical
   to the current script's `sed`-substituted template including the
   `local_addrs` line.
2. **Given** a line with no source address configured, **When** the same asset
   is rendered, **Then** the `local_addrs ... @SRC_ADDR@` line is omitted,
   identical to the current script's deletion branch.
3. **Given** any line index, **When** the strongswan.conf / swanctl.conf /
   updown / vpcd reader.conf assets are rendered, **Then** each matches the
   current heredoc output for the same inputs, and a maintainer's change to a
   rendered field is caught by a failing snapshot test.

---

### User Story 2 - Shutdown is an ordered, tested plan instead of a fragile trap (Priority: P2)

An operator stops the container (or it receives SIGTERM). Today teardown depends
on ~15 global PID arrays reconciled inside one `cleanup()` trap, where the
correctness rests on ordering rules (kill children before tearing down their
PDN; run `volte-cleanup` inside each line's own namespace before deleting it;
use SIGKILL-not-SIGTERM for children that may be blocked mid-AT-transaction).
After this story, shutdown is modeled as an owned, ordered plan of typed steps
whose ordering invariants are asserted by tests, executed at the edge.

**Why this priority**: Teardown correctness is safety-relevant (a wrong order
leaves a modem's displaced data context unrestored) but is only exercised on
shutdown, so regressions hide for a long time. Modeling it as tested data is
high value; it depends on P1's runner/edge split being in place.

**Independent Test**: Build the shutdown plan from a synthetic set of
started-line records and assert the emitted step sequence (order, target
namespace per step, signal per step) against expected fixtures — with no
processes actually killed.

**Acceptance Scenarios**:

1. **Given** a set of started VoWiFi and VoLTE lines, **When** the shutdown plan
   is built, **Then** every child-kill step for a line precedes that line's
   PDN/namespace-teardown step.
2. **Given** a started VoLTE line in namespace `N`, **When** the plan is built,
   **Then** its `volte-cleanup` step is scoped to run inside `N` and is ordered
   before `N`'s deletion.
3. **Given** a child that may block mid-AT-transaction, **When** the plan is
   built, **Then** its kill step specifies SIGKILL, and a test named for that
   invariant fails if the signal is changed.

---

### User Story 3 - One tested supervision state machine replaces three copies (Priority: P3)

A maintainer needs to change how a degraded line recovers (for example, the
cadence of re-initiating a stalled tunnel, or the detection of a broken vici
connection). Today that logic exists in three near-duplicate loops
(strongswan establish-time, strongswan steady-state, swu), so a fix to one
rarely reaches the others. After this story, there is a single per-line
supervisor state machine (`Establishing → Up → Degraded{reason} → Restarting`)
driven over an injected command runner, with the recovery transitions covered by
table-driven tests using a mock runner fed synthetic charon/swanctl output.

**Why this priority**: Largest maintainability win and the deepest behavior
change, so it carries the most live-validation risk and goes last. It depends on
the runner abstraction (P1) and the lifecycle model (P2).

**Independent Test**: Feed the state machine a table of synthetic inputs
(charon log excerpts, `swanctl --list-sas` output, process-alive signals,
changed P-CSCF addresses) through a mock runner and assert the exact sequence of
commands it emits and the state transitions it takes — no real charon/pcscd.

**Acceptance Scenarios**:

1. **Given** an established line whose charon process has died, **When** the
   supervisor ticks, **Then** it emits the same recovery sequence the current
   steady-state loop does (clear log, remove stale pidfile, relaunch charon,
   reload, re-initiate, restart that line's ims-agent) and transitions through
   `Degraded → Restarting → Up`.
2. **Given** a line whose CHILD_SA is established but no P-CSCF appears within
   the configured window, **When** the supervisor ticks, **Then** it terminates
   and re-initiates on the same cadence as today.
3. **Given** a rekey that assigns a different P-CSCF, **When** the supervisor
   ticks, **Then** it refreshes the P-CSCF file and restarts that line's
   ims-agent only, leaving other lines untouched.

---

### User Story 4 - The entrypoint is a thin, auditable shim (Priority: P4)

An operator or reviewer opens `docker/entrypoint.sh` to understand container
startup. After this story it is a small script (~50 lines) that performs
environment/precondition checks and then `exec`s `gsm-sip-bridge supervise`; all
orchestration decisions are readable and tested in Rust.

**Why this priority**: The payoff of the earlier phases; only achievable once
rendering, lifecycle, and supervision have moved. Lowest urgency, highest
dependency.

**Independent Test**: Confirm the shim performs the documented precondition
checks and execs the supervisor, and that a full multi-line VoWiFi (strongswan
and swu) plus VoLTE deployment behaves identically to the pre-refactor image on
real hardware.

**Acceptance Scenarios**:

1. **Given** a missing binary or missing config, **When** the shim runs, **Then**
   it fails fast with the same diagnostic messages as today.
2. **Given** a valid environment, **When** the shim runs, **Then** it execs the
   Rust supervisor, which reproduces the current startup order (discover once up
   front, circuit-switched daemon, then VoWiFi or VoLTE).

---

### Edge Cases

- **No usable line discovered**: when `[vowifi].enabled` or
  `[volte].bridge_inbound` is true but zero lines resolve, the supervisor MUST
  emit the same prominent error and keep the circuit-switched daemon running,
  exactly as today.
- **Mutual exclusion**: `[vowifi].enabled` and `[volte].enabled` both true MUST
  still fail fast with the same fatal message.
- **Per-line failure isolation**: a line that fails to start MUST be skipped
  with the rest continuing — never a container-wide restart.
- **Idempotent restart**: re-entering setup for an already-configured
  namespace/veth/interface MUST be a no-op, as the current `ensure_*` helpers
  guarantee.
- **Half-created veth on swu reconnect**: a missing far-end veth MUST trigger a
  rebuild, matching current behavior.
- **vpcd port unavailable / reader failed to bind**: MUST fail fast with the
  same guidance about the ephemeral-port range.
- **USIM dropped mid-run** (`AT+CSIM failed`): the CSIM-failure threshold and
  bounded AT+CFUN power-cycle recovery MUST behave identically (thresholds,
  attempt cap, holder freeze/resume).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The orchestration logic currently in `docker/entrypoint.sh` MUST
  be moved into the `gsm-sip-bridge` binary, invocable as a `supervise`
  subcommand, without changing the container's observable runtime behavior. This
  includes the always-on circuit-switched GSM-to-SIP daemon's own restart loop,
  which MUST move into `supervise` alongside the VoWiFi/VoLTE orchestration so
  that the final `entrypoint.sh` retains no supervision loops of any kind.
- **FR-002**: All config-asset generation (strongswan.conf, swanctl.conf ePDG
  connection, updown wrapper, vpcd reader.conf) MUST be implemented as pure
  functions that return the rendered text and MUST be exposed for use by the
  shim (e.g., a `render` subcommand) so no phase requires generating those
  assets via shell heredoc/`sed`.
- **FR-003**: Rendered assets MUST be byte-for-byte identical to the current
  script's output for the same inputs, verified by snapshot tests (using the
  `insta` snapshot crate) that include both the source-address-present and
  source-address-absent branches.
- **FR-004**: Container teardown MUST be modeled as an owned, ordered sequence of
  typed steps that encodes the existing ordering invariants (children killed
  before their PDN/namespace teardown; per-line `volte-cleanup` executed inside
  that line's namespace before the namespace is deleted; SIGKILL used where a
  child may be blocked mid-AT-transaction), with those invariants asserted by
  named unit tests.
- **FR-005**: Per-line supervision MUST be implemented as a single state machine
  covering the recovery transitions currently duplicated across the strongswan
  and swu loops, so a change to recovery behavior is made in exactly one place.
- **FR-006**: All operations that touch the system MUST be issued through an
  injectable command-runner abstraction so the decision logic can be tested
  against a mock without executing them. This abstraction covers not only
  transient leaf commands (invocations of `ip`, `dig`, `swanctl`, `stty`, raw
  serial AT writes, and nested `gsm-sip-bridge` subcommands) but also the
  spawning and ownership of the long-running child processes (charon, pcscd, the
  vowifi/volte agents, the circuit-switched daemon, keepalive loops): the runner
  spawns them, returns handles, and mediates signalling and liveness checks, so
  that `supervise` is the real in-process supervisor and the mock can model
  spawn, kill/signal, and process-alive outcomes.
- **FR-007**: The decision logic (what commands to run, in what order, on what
  transition) MUST be unit-testable without root, without hardware, and without
  spawning the real charon/pcscd/swu/agent processes.
- **FR-008**: All privileged timing and cadence constants (inter-step sleeps,
  re-initiate intervals, poll counts, thresholds, budgets) MUST be preserved at
  their current values and represented as named, testable constants — not
  altered or "cleaned up" during the move.
- **FR-009**: Each load-bearing operational invariant currently captured only in
  a code comment (e.g., charon ignores `pidfile=` so the unqualified pidfile
  must be removed before each launch; the swanctl flag must follow the command
  name; `network_mode: host` makes socket-owner attribution by PID impossible)
  MUST be encoded as a named test that fails if the behavior regresses.
- **FR-010**: The refactor MUST proceed in independently shippable phases, each
  of which leaves the container fully functional and live-testable on real
  modem hardware, in this order: (0) bash safety net, (1) rendering,
  (2) teardown, (3) supervision, (4) shim reduction.
- **FR-011**: Phase 0 MUST add `shellcheck` coverage for `docker/*.sh` to the
  `make lint` target and MUST cover the pure bash helpers being extracted
  (at minimum `extract_latest_pcscf` and the `render_line_*` helpers) with a
  bash test harness (`bats-core`), before any logic is ported to Rust.
- **FR-012**: The supervisor MUST preserve the current startup sequencing —
  in particular, resolving the line table (`discover`) once, up front, before
  the circuit-switched daemon's own USB scan begins — so no two processes probe
  the same modem's serial port concurrently.
- **FR-013**: The supervisor MUST preserve per-line failure isolation: a failing
  line is skipped and the remaining lines and the circuit-switched daemon
  continue; recovery restarts only the affected line's processes, never the
  whole container.
- **FR-014**: The supervisor MUST preserve the existing VoWiFi/VoLTE mutual
  exclusion, the "no usable line discovered" prominent-error-but-keep-running
  behavior, and the vpcd-reader readiness gate with its current fail-fast
  guidance.
- **FR-015**: The USIM auto-recovery behavior (CSIM-failure threshold, bounded
  reset attempts per incident, holder freeze/resume around the AT+CFUN cycle,
  readiness polling) MUST be preserved with identical thresholds and sequencing,
  and its decision logic MUST be covered by tests.
- **FR-016**: At the end of the final phase, `docker/entrypoint.sh` MUST be
  reduced to a shim that performs precondition checks and execs the supervisor,
  emitting the same fatal diagnostics as today for a missing binary or config.

### Key Entities

- **Line table**: the resolved set of VoWiFi and/or VoLTE lines (card id, modem
  port, namespace, interface/veth names and addresses, MCC/MNC, per-engine
  parameters) produced by discovery once at startup and consumed by the
  supervisor. Already a typed structure inside the binary today.
- **Config asset**: a rendered text file for a given line (strongswan.conf,
  swanctl ePDG connection, updown wrapper, vpcd reader.conf), a pure function of
  its inputs.
- **Teardown step**: a typed shutdown action with a target (optionally a
  namespace), a signal or command, and an ordering position within the shutdown
  plan.
- **Line supervisor state**: the per-line lifecycle state
  (`Establishing`, `Up`, `Degraded{reason}`, `Restarting`) and the transitions
  between them driven by observed inputs.
- **Command runner**: the injectable boundary through which all system-touching
  operations are issued; a real implementation at runtime, a mock in tests.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A maintainer can change any config-asset rendering, teardown
  ordering, or line-recovery behavior and get a pass/fail signal from the test
  suite without building a container image or attaching a modem.
- **SC-002**: The three duplicated supervision loops are reduced to one
  implementation; changing recovery behavior requires editing a single location.
- **SC-003**: Every rendered config asset is covered by a snapshot test,
  including both the source-address-present and source-address-absent branches.
- **SC-004**: Every ordering and signal invariant of teardown is covered by a
  named test that fails on regression.
- **SC-005**: The decision logic runs in unit tests with no root privilege, no
  hardware, and no real charon/pcscd/swu/agent processes.
- **SC-006**: At completion, `docker/entrypoint.sh` is at most ~50 lines and
  contains no config-asset heredocs, no supervision/restart loops (neither
  per-line nor the circuit-switched daemon's), and no global PID-array
  bookkeeping.
- **SC-007**: Each phase is deployed and validated on real hardware (multi-line
  VoWiFi under both strongswan and swu engines, plus VoLTE) with no observable
  behavioral difference from the pre-phase image.
- **SC-008**: `shellcheck` passes on all `docker/*.sh` as part of `make lint`,
  and the pre-commit checklist (`make format && make lint && make test`)
  continues to pass at every phase boundary.

## Assumptions

- The container continues to run privileged with `network_mode: host` and the
  same capabilities; the move does not change the deployment model.
- The `gsm-sip-bridge` binary remains the single artifact that owns leaf
  operations; new orchestration is additional subcommands/modules within it,
  built with the existing toolchain (tokio, clap) already present in the crate.
- Two new development/test dependencies are permitted and will be vetted through
  `deny.toml`: the `insta` snapshot crate (Rust, dev-dependency) and `bats-core`
  (bash test harness, used in CI/`make test` for `docker/*.sh`). No new runtime
  dependencies are introduced.
- "Behavior-preserving" is judged against the current `docker/entrypoint.sh` as
  the reference implementation; where a current behavior is a known bug, it is
  preserved unless a separate decision is made to change it (out of scope here).
- Live validation is performed by the maintainer on the existing modem hardware;
  automated tests cannot exercise the privileged leaf operations end to end.
- The external tools invoked (`ip`, `charon`, `pcscd`, `swanctl`, `dig`,
  `stty`, `pcscd`/`vpcd` build options) remain as they are in the current image.
- Phases are merged in order; a later phase may begin only after the previous
  phase has been live-validated.
