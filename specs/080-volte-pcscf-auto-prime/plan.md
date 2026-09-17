# Implementation Plan: Automatic VoLTE P-CSCF Priming

**Branch**: `080-volte-pcscf-auto-prime` | **Date**: 2026-09-17 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/080-volte-pcscf-auto-prime/spec.md`

## Summary

`orchestrate_volte::start` (and the `volte-register`/`volte-bridge` children it
spawns) only ever read a P-CSCF from an explicit override or from
`[volte].pcscf_source_path` — never discover one — so on every carrier tested
so far (Jio, Vodafone) VoLTE cannot register until an operator manually
enables `[vowifi]`, restarts the container so the ePDG tunnel writes that
file, then flips back to `[volte]` and restarts again
(`docs/operations.md`, "The VoWiFi priming dance").

This feature moves that dance inside `supervise` itself. Before
`orchestrate_volte::start` dispatches to either of its two startup paths, it
checks whether a usable address already exists (override or valid cache —
unchanged, FR-005). If not, it runs a **transient, one-shot VoWiFi capture**:
borrow `discover`'s existing modem-discovery output for one line, bring up
exactly that line's ePDG tunnel far enough to receive the IKE_AUTH config
payload's P-CSCF (reusing the same `line_supervisor`/`engines` machinery the
persistent `[vowifi]` path already uses on real hardware), write the address
to `[volte].pcscf_source_path`, and tear the transient line all the way back
down using the **existing, already-hardware-exercised shutdown-plan
machinery** (`shutdown::build_shutdown_plan` / `execute_shutdown_plan`) scoped
to a `StartedState` containing only that one line — not new teardown code.
VoLTE registration then proceeds exactly as it does today, reading the same
file it always has.

This is deliberately an *extraction*, not a rewrite: every persistent
`[vowifi]` code path in `orchestrate.rs` keeps its current control flow
unchanged (same functions, same order, same tests) — priming calls the
extracted pieces once, from a new module, with a synthetic single line, and
adds nothing to the container-wide steady-state supervision, `StartedState`,
or the real shutdown plan.

## Technical Context

**Language/Version**: Rust 1.94.0 (workspace `rust-toolchain.toml`, edition 2021)
**Primary Dependencies**: No new external crate. Reuses existing in-tree modules: `crate::supervise::{engines, line_supervisor, render, vpcd, epdg_iface, shutdown, runner}`, `crate::vowifi::discovery`, `crate::volte::pcscf`
**Storage**: Same on-disk cache file as today (`[volte].pcscf_source_path`, default `/tmp/pcscf-0`) — no new persistent storage, no new format
**Testing**: `cargo test` across the workspace; new logic is exercised through `supervise::runner::MockCommandRunner` exactly as `orchestrate.rs`'s existing per-line and shutdown-plan tests already do (Integration-First Testing via the `CommandRunner` seam, not new mocks) — see Constraints below for what this cannot cover
**Target Platform**: Linux (Docker, `network_mode: host`), the bridge's only deployment target; this feature only ever runs inside `supervise` (`gsm-sip-bridge supervise` / the container entrypoint), never in a standalone CLI invocation (FR-002a)
**Project Type**: Single Cargo workspace member (`gsm-sip-bridge`), new code lives in `src/supervise/`
**Performance Goals**: N/A — this runs once per boot, at most, before VoLTE's own registration; not in any call-answering or steady-state path
**Constraints**:
- Must not change the observable behavior or control flow of the existing persistent `[vowifi]` path (`orchestrate.rs`'s `start_vowifi_subsystem`/`start_vowifi_line*`) — extraction only, same functions.
- Must not add a new subcommand invocable by an operator directly (FR-002a) — the transient capture is an internal detail of `orchestrate_volte::start`, not a CLI surface.
- **Cannot be validated against real hardware in this environment** (no root/CAP_NET_ADMIN in this sandbox — see project memory on sandbox network-testing limits). The persistent `[vowifi]` bring-up sequence this feature reuses is already hardware-proven; what is *new* here — bringing up and then fully tearing down one transient line, then immediately letting VoLTE's own PDN/registration bring-up touch the same modem again in the same process lifetime — has only been exercised across a full container restart before (the manual dance), never within one continuous process. This specific hand-off is the one thing that genuinely needs a live-hardware pass before this can be trusted in production; flagged again in quickstart.md.
**Scale/Scope**: One new module (`src/supervise/orchestrate_prime.rs`), small extractions from `orchestrate.rs` (no behavior change to what's extracted), two call sites added in `orchestrate_volte.rs` (`start_legacy_registration`'s retry loop, `start_multiline`'s pre-flight), one new pure helper in `volte::pcscf` for the "is a usable address already available" check (FR-001/005)

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

- **Integration-First Testing**: Satisfied by design — the new priming path is
  built entirely from existing, already-integration-tested building blocks
  (`line_supervisor::tick_establishing`, `engines::StrongswanEngine`/
  `SharedCharon`, `render::*`, `vpcd::start_pcscd_with_retries`,
  `shutdown::build_shutdown_plan`/`execute_shutdown_plan`) exercised through
  the same `CommandRunner` seam (`MockCommandRunner`) their own existing
  tests already use — no new mocking concept is introduced. The one thing
  this test seam cannot exercise — real charon/IKE_AUTH behavior and the
  same-process modem hand-off from priming to VoLTE's own bring-up — is
  called out explicitly rather than papered over (see Constraints and
  quickstart.md); this mirrors `orchestrate_volte.rs`'s own existing header
  comment flagging its lack of live-hardware validation.
- **Green-on-Commit**: Each extraction step (moving code out of
  `orchestrate.rs` into a callable function without changing its logic) is
  its own commit, verified with `cargo test` before moving to the next step,
  so the persistent `[vowifi]` path's existing test coverage continuously
  proves nothing broke.
- **Frequent Atomic Commits**: Natural break points: (1) `volte::pcscf`
  "usable address available" check, (2) extract the reusable establish
  sequence in `orchestrate.rs` with zero behavior change, (3) new
  `orchestrate_prime.rs` module wiring the extracted pieces into one
  synthetic-line prime-then-teardown call, (4) wire the two call sites in
  `orchestrate_volte.rs`.
- **Makefile-Driven Build**: No new build steps; `make format`/`make
  lint`/`make test` cover everything this feature touches.
- **Simplicity & Refactorability**: The design's central decision — reuse
  `shutdown::build_shutdown_plan`/`execute_shutdown_plan` against a
  purpose-built, one-line `StartedState` instead of writing new teardown
  code — exists specifically to avoid a second, divergent teardown
  implementation. No new abstraction layer, no new config flag (the
  existing cache-file/override check from FR-001 already gates whether
  priming runs at all).

**Result**: PASS, no violations to justify.

## Project Structure

### Documentation (this feature)

```text
specs/080-volte-pcscf-auto-prime/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/           # Phase 1 output
│   └── volte-auto-prime-contract.md
└── tasks.md             # Phase 2 output (/speckit.tasks — not this command)
```

### Source Code (repository root)

```text
gsm-sip-bridge/
└── src/
    ├── volte/
    │   └── pcscf.rs          # New: `pcscf_is_available(&Path, Option<&str>) -> bool`
    │                         # — the FR-001/FR-005 check (override or valid
    │                         # cache), reused by both call sites below. Pure
    │                         # function over `probe_epdg_cache`, already tested.
    └── supervise/
        ├── orchestrate.rs     # Extraction only: the per-line "bring up netns +
        │                      # tun + usim-bridge + swanctl conn + wait for
        │                      # Established" sequence inside
        │                      # `start_vowifi_line_strongswan` becomes a
        │                      # callable helper both the existing persistent
        │                      # path and `orchestrate_prime` call. No change to
        │                      # what runs for a real `[vowifi]` line, or in
        │                      # what order.
        ├── orchestrate_prime.rs  # New. `prime_pcscf(runner, bin, config_path,
        │                      # config) -> Result<(), String>`:
        │                      # 1. runs `discover` (unconditionally — FR-002b
        │                      #    needs a candidate line even though
        │                      #    `[vowifi].enabled` is false here) and takes
        │                      #    `resolution.lines[0]`
        │                      # 2. builds a private `SharedCharon` (its own
        │                      #    conf/log paths — never the container-wide
        │                      #    `SHARED_*` constants, so it cannot collide
        │                      #    with, or be mistaken for, a real `[vowifi]`
        │                      #    deployment) and a private pcscd/vpcd pair
        │                      # 3. calls the extracted establish helper with a
        │                      #    bounded timeout (new — the persistent path's
        │                      #    establish loop is deliberately unbounded;
        │                      #    priming is not, per the spec's "capture
        │                      #    takes too long" edge case)
        │                      # 4. on success: writes
        │                      #    `[volte].pcscf_source_path` (FR-003), builds
        │                      #    a one-line `shutdown::StartedState`, and
        │                      #    tears it down via
        │                      #    `build_shutdown_plan`/`execute_shutdown_plan`
        │                      # 5. on any failure: tears down whatever partially
        │                      #    started (same plan machinery, partial state)
        │                      #    and returns `Err` with a message distinct
        │                      #    from a carrier registration failure (FR-007)
        └── orchestrate_volte.rs  # `start_legacy_registration`'s existing
                                  # 15s-cadence retry loop gains a
                                  # `pcscf_is_available(...)` check at its top,
                                  # calling `orchestrate_prime::prime_pcscf`
                                  # when it's false (FR-008 for free — no new
                                  # retry logic). `start_multiline` gains a
                                  # one-time pre-flight call with its own small
                                  # loop at the same 15s cadence, placed after
                                  # `volte-discover-lines` succeeds and before
                                  # the per-line spawn loops.
```

**Structure Decision**: No new crates. One new file
(`orchestrate_prime.rs`), one new pure function in `volte/pcscf.rs`, and a
pure extraction (no behavior change) inside `orchestrate.rs`'s existing
per-line startup function. Matches the workspace's existing pattern of
`supervise` submodules calling into `volte`/`vowifi` domain modules; no
alternative structure (e.g., a new subcommand/subprocess) was chosen — see
research.md for why that alternative was rejected.

## Complexity Tracking

*No Constitution Check violations — this section is not applicable.*
