# Implementation Plan: Bound the tunnel-initiation work so a silent ePDG cannot freeze the shared IPsec daemon

**Branch**: `118-bound-swanctl-initiate` | **Date**: 2026-10-21 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `specs/118-bound-swanctl-initiate/spec.md` (GitHub issue #139)

## Summary

Four parts, in dependency order. Part 1 alone removes the freeze; 2-4 make the system hold that property.

**1. One owner for initiation, with a finite lifetime (US1, US2, US6).**
- Every `swanctl --initiate` goes through one engine method, `StrongswanEngine::initiate`. Today two sites build the command: the startup call in `orchestrate.rs` and `swanctl_background` in `engines.rs`.
- The command becomes `timeout -k 2 25 env STRONGSWAN_CONF=... swanctl --initiate --child <conn> --timeout 20`. `--timeout 20` makes charon release the worker thread at 20s while it keeps negotiating (research R1, reproduced). The outer `timeout` covers a VICI that never answers; worst-case lifetime is 27s, under the 30s re-initiate cadence.
- The engine keeps the **tracked** `ChildHandle` of its one outstanding client (not `spawn_detached`). A new initiate is skipped while that handle is alive. A finished handle is `wait`ed, which reaps it and yields the exit status to log. This removes the waiter thread altogether instead of bounding it.
- A new `TunnelEngine::cancel_initiate` reaps the outstanding client; the line supervisor calls it on every exit path.

**2. Startup loop defers to an in-flight negotiation (US3).**
- `tick_establishing` gains the rule `tick_half_open` already applies in steady state: on a re-initiate tick it lists the line's SAs (bounded), and if the IKE_SA is `CONNECTING` and the charon log shows activity for it within `HALF_OPEN_STUCK_AFTER`, it does nothing.
- The activity-tracking part of `tick_half_open` is extracted into a helper both phases call, so there is one rule.

**3. Wedged-daemon detection and recovery during establishment (US4, US5).**
- The same bounded `--list-sas` probe doubles as the control-interface health check. Two consecutive timeouts (~60s at the 30s cadence) mean *control interface unresponsive*.
- Recovery needs new code: `restart_if_dead` never restarts a live daemon and `load_all` has no deadline, so today's `ViciBroken` path cannot repair a wedged charon and can block the supervisor thread itself (research R3). Add `SharedCharon::restart_if_wedged` that kills and respawns the daemon once for all lines (generation-guarded so concurrent lines do not kill each other's replacement), and put a deadline on `load_all`. Steady-state `ViciBroken` uses it too.

**4. Proof (all stories).**
- Unit tests on the supervisor seams, a real-process test of the lifetime bound, and the committed rig (`rig/`) used for the triage reproduction, wrapped as the quickstart's manual run.
- A source-scan test fails the build if `--initiate` is built anywhere but the one helper (SC-007).

## Technical Context

**Language/Version**: Rust (workspace edition), crate `gsm-sip-bridge`, module `src/supervise/`
**Primary Dependencies**: none new. strongSwan fork pinned in `docker/Dockerfile` (`022f3e5`); `timeout` from coreutils already used by `list_sas`.
**Storage**: N/A (state is in-process; charon log and VICI are existing inputs)
**Testing**: `cargo test` via `make test`; supervise unit tests with `MockCommandRunner`; `RealCommandRunner` conformance suite; the container rig for the live reproduction
**Target Platform**: Linux, amd64 + arm64 container (host networking)
**Project Type**: single Rust workspace (daemon/supervisor)
**Performance Goals**: at most one live initiation client per line; worst-case client lifetime 27s; wedge detected within ~2 probe cycles
**Constraints**: client lifetime < `STRONGSWAN_REINITIATE_EVERY` x `ESTABLISH_POLL_INTERVAL` (30s); charon pool is 16 and not changed; per-line cost must leave headroom for N lines
**Scale/Scope**: 1-4 lines per shared charon; touches `engines.rs`, `line_supervisor.rs`, `orchestrate.rs`, `runner.rs` (tests only)

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Status | Note |
| --- | --- | --- |
| I. Integration-first testing | PASS | Real-process lifetime test (`RealCommandRunner` spawning a never-ending child under the real `timeout`), plus the container rig. The supervisor unit tests use the existing `MockCommandRunner`, which the repo's conformance suite already holds equivalent to the real runner. |
| II. Green-on-commit | PASS | Each task commits green; `make format`, `make lint`, `make test` before every commit (CLAUDE.md). |
| III. Atomic commits | PASS | Tasks are ordered so each commit is one logical change. |
| IV. Makefile-driven | PASS | No new entry points; the rig is run per quickstart, not a build target. |
| V. Simplicity | PASS | Constants, not new config keys (no operator needs to tune a 20s deadline). Reuses the existing half-open rule and `list_sas`. The only new mechanism is `restart_if_wedged`, justified by R3. |

Post-design re-check: unchanged, PASS. No Complexity Tracking entries.

## Project Structure

### Documentation (this feature)

```text
specs/118-bound-swanctl-initiate/
├── plan.md
├── research.md
├── data-model.md
├── quickstart.md
├── contracts/
│   └── initiate-command.md
├── rig/                 # the triage reproduction harness (silent-peer charon rig)
└── tasks.md             # /speckit-tasks
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/supervise/
├── engines.rs           # StrongswanEngine::initiate / cancel_initiate; SharedCharon::restart_if_wedged; load_all deadline
├── line_supervisor.rs   # tick_establishing half-open rule + probe; shared HalfOpen helper; TunnelEngine::cancel_initiate
├── orchestrate.rs       # startup initiate -> engine.initiate; cancel on exits; wedge outcome handling
└── runner.rs            # (no API change; real-process test only)
docker/strongswan/       # (no change; keyingtries=0 stays)
```

**Structure Decision**: existing single-crate layout; all changes inside `supervise/`.

## Complexity Tracking

None.
