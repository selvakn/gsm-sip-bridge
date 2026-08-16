# Implementation Plan: Reliable SMS Delivery

**Branch**: `038-reliable-sms-delivery` | **Date**: 2026-08-16 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/038-reliable-sms-delivery/spec.md`

**Note**: This template is filled in by the `/speckit.plan` command. See `.specify/templates/plan-template.md` for the execution workflow.

## Summary

VoWiFi- and VoLTE-only lines (`[cs].enabled = false`) can have the carrier deliver an SMS into the modem's own storage instead of over the IMS registration; nothing reads that storage on the VoWiFi path today, so those texts are silently lost (confirmed live: 6 of 7 parts of a real SMS stuck unread in modem storage). VoLTE already has a fix for this shape of problem (`volte::sms::run_modem_reader`, specs/017 US5) — this feature wires the same, already-tested mechanism into the VoWiFi agent, and closes a second gap found during research: the cross-bearer duplicate-suppression the existing tests describe is not actually connected to the production registration-message path for *either* subsystem today, so it is wired for real for both VoWiFi and VoLTE here, not just added for VoWiFi.

## Technical Context

**Language/Version**: Rust, edition 2021 (workspace `gsm-sip-bridge` v8.12.0)
**Primary Dependencies**: `rusqlite` (SQLite store — unchanged by this feature), `crossbeam-channel`, `tracing`, existing in-tree `volte::sms` / `sms::reader` / `modules::at_commander` modules
**Storage**: SQLite, `[sms].db_path`; no schema change (research.md Decision 3)
**Testing**: `cargo test` via `make test`; project precedent for this exact mechanism is integration-style tests over the pure decision logic (`Dedupe`, `decide`, route tagging) plus a `UnixStream`-pair mock-serial harness (`tests/test_at_commander.rs`) for AT-protocol-level testing — no mocking of the logic under test itself, real hardware I/O is the one Constitution-sanctioned mock/skip boundary
**Target Platform**: Linux container running on the operator's own host/Pi, talking to a real Quectel EC20 modem (or a PC/SC reader for `pcsc_reader` lines, which this feature does not touch)
**Project Type**: Single Rust workspace, multi-subcommand daemon/CLI (no frontend/backend split)
**Performance Goals**: No new performance target — reuses the existing ~20s modem-storage poll interval (`MODEM_SWEEP_INTERVAL`), negligible additional CPU/AT-port traffic
**Constraints**: All AT-port access for a given line MUST stay serialized through that line's existing `modem_lock` Mutex — this port has documented history of wedging for hours under interleaved/concurrent AT traffic (see `VowifiConfig::imei_override` doc comment); the fix must not introduce a new source of contention that bypasses that lock
**Scale/Scope**: Per-line (one OS process per VoWiFi/VoLTE line already, per specs/013 and specs/020); bounded by `[vowifi].max_lines` / VoLTE's equivalent, single-digit line counts in practice

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

- **I. Integration-First Testing**: PASS, with the Constitution's own hardware exception invoked deliberately. Real EC20 hardware is not available in CI, so the thread-spawn wiring that opens a real device path is verified by code inspection plus manual on-device testing (documented in quickstart.md), matching how the VoLTE equivalent (`run_modem_reader`'s wiring in `commands/volte.rs`) was verified — no prior precedent in this codebase integration-tests that spawn path either. Everything else this feature touches (`Dedupe`, `decide`, the newly-shared-ownership wiring, route tagging) is tested via real in-process types, extending the existing `test_volte_sms.rs` pattern — no mocking of the logic under test.
- **II. Green-on-Commit**: PASS — `make format && make lint && make test` gates every commit in this plan's task list, no exceptions.
- **III. Frequent Atomic Commits**: PASS — tasks below are scoped to single logical changes (shared-dedupe plumbing separate from the VoWiFi spawn wiring separate from docs).
- **IV. Makefile-Driven Build**: PASS — no build/tooling changes; existing `make` targets cover everything.
- **V. Simplicity & Refactorability**: PASS — reuses `volte::sms` rather than duplicating it for VoWiFi (research.md Decision 1); satisfies FR-009 via structured logging rather than a new schema column (research.md Decision 3); does not add persistence to the deliberately-ephemeral `Dedupe` window (research.md Decision 2 alternatives).

No violations to record in Complexity Tracking.

## Project Structure

### Documentation (this feature)

```text
specs/038-reliable-sms-delivery/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md         # Phase 1 output
├── quickstart.md        # Phase 1 output
└── tasks.md             # Phase 2 output (/speckit.tasks — not this command)
```

No `contracts/` directory: this feature adds no new external interface. The control-channel message it relies on (`ControlMessage::SmsReceived`) is unchanged, internal-only IPC between two processes of the same deployment (Agent A ↔ Agent B / the recording side), not a public contract this project documents externally.

### Source Code (repository root)

Single Rust workspace (existing structure, no new crates/directories):

```text
gsm-sip-bridge/
├── src/
│   ├── ims/agent/mod.rs        # vowifi-ims-agent entry point (run_inner); gains modem-sweep
│   │                           # wiring + shared Dedupe consulted by handle_message
│   ├── volte/
│   │   ├── sms.rs              # run_modem_reader/sweep_modem_storage: accept an externally-
│   │   │                       # owned Arc<Mutex<Dedupe>> instead of an internal one
│   │   ├── carrier_agent.rs    # VoLTE diagnostic path: construct + share the per-line Dedupe
│   │   └── bridge.rs           # VoLTE diagnostic path (single --modem mode): same change
│   └── commands/volte.rs       # volte-carrier-agent subcommand: construct + share the Dedupe
├── tests/
│   ├── test_volte_sms.rs       # extended: shared-Dedupe cross-bearer suppression via
│   │                           # production call paths, not just the pure decide() API
│   └── test_vowifi_sms_reader.rs  # new: VoWiFi-side mirror of the above
└── docs/
    └── configuration.md        # note under [[vowifi.line]]/[[volte]] that modem storage
                                 # is now swept regardless of [cs].enabled
```

**Structure Decision**: No new modules or files beyond one new test file. This is a wiring change across existing modules — `volte::sms`'s already-built, already-tested mechanism gains a shared-ownership parameter and a second caller (`ims::agent`), rather than any new subsystem being introduced.
