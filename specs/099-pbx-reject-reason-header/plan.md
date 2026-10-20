# Implementation Plan: Q.850 Reason-based PBX reject detection

**Branch**: `099-pbx-reject-reason-header` | **Date**: 2026-10-20 | **Spec**: [spec.md](spec.md)
**Input**: `specs/099-pbx-reject-reason-header/spec.md`

## Summary

Follow-up to #110 / issue #108. A FreePBX rejects with `403` +
`Reason: Q.850;cause=21`; the bridge sees a failure and sends `480`, and the
carrier rings again. Fix in four parts:

1. **pjsua-safe**: in `on_call_state_cb`, read the `Reason` header from the
   received failure final response and store its Q.850 cause beside the
   status in the existing per-call disconnect log.
2. **Classifier** (`vowifi/mod.rs`): cause 21/17 on a failure response is a
   reject, whatever the status; otherwise the status is looked up in a
   reject set (486/600/603 plus config).
3. **Config**: `[sip].reject_statuses` adds to the set. 403 is not a default.
4. **Observability**: `:q850=<n>` on the record; log fields for the 480 path.

Control wire format, To-tag handling and metrics labels do not change. See
[research.md](research.md) R1-R8.

## Technical Context

**Language/Version**: Rust (toolchain per `rust-toolchain.toml`)
**Primary Dependencies**: `pjsua-safe` / `pjsua-sys` (existing bindings
include `pjsip_msg_find_hdr_by_name` and `pjsip_generic_string_hdr`); no new
crates
**Storage**: N/A (in-memory log, bounded at 64)
**Testing**: `make test`. Pure-function tests for parsing and
classification; stub-`Call` tests for `wait_for_pbx_answer`; live check with
`siptest` in the privileged container
**Target Platform**: Linux (x86_64, arm64), Docker
**Project Type**: daemon (VoWiFi/VoLTE two-agent bridge)
**Performance Goals**: unchanged; one header lookup per disconnect
**Constraints**: no wire-format change; unreadable header must never fail a
call; no real subscriber data in tests
**Scale/Scope**: ~5 source files, one config key, docs

## Constitution Check

| Principle | Assessment |
|---|---|
| I. Integration-first testing | **Pass.** Parsing and classification are pure and tested directly. The stub PJSIP `Call` is existing practice (PJSIP and a PBX are not available in CI) and the live `siptest` run covers the real path (R8). |
| II. Green-on-commit | **Pass.** `make format/lint/test` before each commit. |
| III. Atomic commits | **Pass.** One commit per slice (tasks.md). |
| IV. Makefile-driven | **Pass.** No new targets. |
| V. Simplicity | **Pass.** The cause rides in the existing log entry (R2); one config key; no new store or message type. |

Re-check after design: pass, no violations.

## Project Structure

```text
specs/099-pbx-reject-reason-header/
├── spec.md  plan.md  research.md  data-model.md  quickstart.md
├── contracts/{config.md,decline-signaling.md}
├── checklists/requirements.md
└── tasks.md
```

```text
pjsua-safe/src/
├── endpoint.rs   # Disconnect entry, Reason read in on_call_state_cb, parse_q850_cause
└── call.rs       # Call::last_disconnect(); stub setter for the cause

gsm-sip-bridge/src/
├── config/{raw,mod,build}.rs  # [sip].reject_statuses, RejectStatuses, validation
└── vowifi/mod.rs              # classify_pbx_disconnect, wait_for_pbx_answer, record string, logs

docs/configuration.md          # key row + FreePBX example
RELEASE_NOTES.md               # Unreleased bullet (bold only the feature name)
```

**Structure Decision**: extend the existing files; no new modules.

## Risks

- R1 may not hold on every disconnect path. Mitigation: fall back to status;
  verify live first (T001) and log `response_available`.
- Wrongly calling a failure "busy". Mitigation: causes limited to 21/17,
  failure responses only, 403/503 not defaulted.
- `Reason` lookup is by raw name; compact forms do not exist for it.
