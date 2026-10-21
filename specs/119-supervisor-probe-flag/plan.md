# Implementation Plan: Supervisor P-CSCF Probe Behind a Flag, Off by Default

**Branch**: `119-supervisor-probe-flag` | **Date**: 2026-10-21 | **Spec**: [spec.md](spec.md)
**Input**: `specs/119-supervisor-probe-flag/spec.md`

## Summary

The line supervisor's last P-CSCF probe (a TCP connect to the unprotected SIP port on every steady-state tick) goes behind one `[vowifi]` boolean, `supervisor_pcscf_probe`, default `false`. With it off the probe block in `tick_steady_state_underlay` is skipped entirely, so no connect is made and no strike, sibling choice or `pcscf_unreachable` rebuild can happen. With it on, the code path is the one that exists today, untouched. The replacements (per-tick route check, pool-exhaustion rebuild, DPD tuning) are already merged and sit ahead of the probe in the same function, so they need no change.

Approach (details in [research.md](research.md)): a `probe_disabled` field on `SteadyTracking` (the per-line state both engine loops already build), set once from config where each loop constructs it. The gate lives in the one decision function, so both engines get it, and `Default` keeps meaning "behaves as before", so no existing test needs editing.

## Technical Context

**Language/Version**: Rust (workspace crate `gsm-sip-bridge`)
**Primary Dependencies**: none new
**Storage**: N/A
**Testing**: `cargo test` via `make test`; unit tests beside `tick_steady_state_underlay` using the existing mock runner (which records every connect); config tests beside `idle_keepalive`'s; live checks per [quickstart.md](quickstart.md)
**Target Platform**: Linux host and the Pi (arm64) for the Jio line
**Project Type**: single Cargo workspace
**Performance Goals**: none; the change removes ~120 connects/line/hour
**Constraints**: flag-on behaviour byte-for-byte as before (FR-004); route repair and pool-exhaustion rebuild must still run flag-off (FR-005/FR-006)
**Scale/Scope**: about eight source files plus docs; no new modules

## Constitution Check

| Principle | Status |
|---|---|
| I. Integration-first testing | Pass. The new tests drive the real tick function through the real engine with the project's existing command-runner fake, which already records connects; the live Vodafone/Jio runs are the integration layer. |
| II. Green-on-commit | Pass. Each task phase ends with `make format && make lint && make test`. |
| III. Atomic commits | Pass. One commit per phase (see tasks.md). |
| IV. Makefile-driven | Pass. No new commands. |
| V. Simplicity | Pass. One bool setting, one bool field, one `if`. No new abstraction; the probe code is kept (deleting it is out of scope). |

Re-checked after design: no violations; Complexity Tracking not needed.

## Project Structure

### Documentation

```text
specs/119-supervisor-probe-flag/
├── spec.md
├── plan.md
├── research.md
├── data-model.md
├── quickstart.md
├── contracts/
│   ├── config.md
│   └── logs.md
├── checklists/requirements.md
└── tasks.md
```

### Source touched

```text
gsm-sip-bridge/src/config/{mod,raw,build}.rs      # the setting, default false
gsm-sip-bridge/src/commands/config.rs             # SUPERVISOR_PCSCF_PROBE in the shell export
gsm-sip-bridge/src/supervise/line_supervisor.rs   # SteadyTracking.probe_disabled + the gate
gsm-sip-bridge/src/supervise/orchestrate.rs       # set it from config (both loops), startup log line
gsm-sip-bridge/tests/test_shell_env_contracts.rs  # export contract
docs/configuration.md, config.toml.example, CHANGELOG.md, RELEASE_NOTES.md
```

**Structure Decision**: no new files in `src/`. `engines.rs` is deliberately untouched: `pcscf_reachable` stays a pure engine capability and the policy "do we ask?" stays with the supervisor's decision function.
