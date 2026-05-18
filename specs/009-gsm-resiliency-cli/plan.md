# Implementation Plan: GSM Modem Resiliency & CLI Utilities (v5.1.0)

**Branch**: `009-gsm-resiliency-cli` | **Date**: 2026-05-17 | **Spec**: [./spec.md](./spec.md)
**Input**: Feature specification from `specs/009-gsm-resiliency-cli/spec.md`

## Summary

Extend the running GSM-SIP bridge daemon with: (1) automatic recovery when a modem loses USB connection or network registration — with exponential backoff, per-slot give-up tracking, and IMEI-keyed persistent slot assignments; (2) startup diagnostics printing each card's phone number and network type before the ready message; (3) a Unix domain socket control API that the CLI uses for on-demand `card restart`, `card set-mode`, and `card get-mode` operations; and (4) database-persisted network mode preferences re-applied on every card initialization. All changes are additive to the existing Rust workspace (`gsm-sip-bridge` crate, SQLite store, TOML config).

## Technical Context

**Language/Version**: Rust stable (toolchain pinned by `rust-toolchain.toml`; MSRV: stable − 2 minor versions, same as v5.0.x)

**Primary Dependencies** (all already in `gsm-sip-bridge/Cargo.toml`):
- `tokio` — async runtime, spawn control-socket listener task
- `serde` + `serde_json` — JSON control protocol frames
- `rusqlite` — SQLite store (new tables: `card_slots`, `card_mode_prefs`)
- `serialport` — AT command I/O (new commands: `AT+CGSN`, `AT+CNUM`, `AT+QNWINFO`, `AT+QCFG`)
- `clap` v4 derive — new `card` subcommand tree
- `tracing` — structured log events for all recovery lifecycle events
- No new external crates required

**Storage**: Existing SQLite database at the configured path. Schema bumped from v1 → v2 with two new tables (`card_slots`, `card_mode_prefs`); migration is additive (no column drops). The store writer thread gains new command variants.

**Testing**: `cargo test --workspace` integration tests. AT command parsing tested via `AtCommander::from_stream` with in-process PTY/pipe pairs (existing pattern). Control socket protocol tested with a pair of connected `UnixStream`s in-process. Schema migration tested with `tempfile`. No new mocks; hardware interactions remain behind the existing PTY-based test harness.

**Target Platform**: Linux (Debian/Ubuntu, x86\_64 and aarch64). Unix domain sockets and `/sys/bus/usb/devices` sysfs polling are Linux-specific — same assumption as existing codebase.

**Project Type**: Cargo workspace binary daemon + CLI (same binary, different subcommand path)

**Performance Goals**:
- USB disconnect detected within 5 s (FR-001) — met by existing serial read-error path; no extra polling needed.
- Network loss detected within 60 s (default timeout, FR-002) — periodic `AT+CREG?` / `AT+CEREG?` poll on 30 s interval.
- Startup diagnostics displayed within 10 s of process start (SC-003).
- `card restart` completes within 30 s (SC-005).
- `card set-mode` confirms mode change within 15 s (SC-006).

**Constraints**:
- Zero new `unsafe` blocks in `gsm-sip-bridge` crate (existing rule).
- CLI subcommands communicate with daemon only via Unix socket — no in-process state sharing.
- Schema migration must be forwards-compatible: a v1 DB is silently upgraded to v2 on first start; a v2 DB proceeds directly.
- Control socket path defaults to `/tmp/gsm-sip-bridge.sock` (no root required); configurable in TOML as `[control] socket_path`.

**Scale/Scope**: Up to 8 slots (inherited). Control socket handles one command at a time (sequential; concurrent CLI invocations queue on the OS-level socket accept backlog, acceptable for homelab management traffic).

## Constitution Check

*Gate: must pass before Phase 0. Re-checked after Phase 1.*

### I. Integration-First Testing — PASS
- AT command parsing (IMEI, phone number, network type, network mode) tested via `from_stream` PTY pattern already established in the codebase.
- Schema migration (v1→v2) tested with `tempfile` real SQLite file.
- Control socket round-trip tested with in-process `UnixStream` pair — real I/O, no mocking.
- No new mocks introduced; hardware remains behind existing PTY stub.

### II. Green-on-Commit — PASS (process gate)
- Every task ends with `make test` passing before commit. Pre-commit hook enforces `cargo fmt --check && cargo clippy -D warnings && cargo test --workspace`.

### III. Frequent Atomic Commits — PASS
- Tasks are sized for one commit each; each task is independently buildable and testable.

### IV. Makefile-Driven Build — PASS
- No new Makefile targets required; all operations remain under `make build`, `make test`, `make lint`. The new `card` subcommands are just CLI arguments, not build targets.

### V. Simplicity & Refactorability — PASS
- Control protocol is newline-framed JSON — simplest option that works, no custom framing, no protobuf.
- No new abstraction layers beyond what the tasks directly require.

## Project Structure

### Documentation (this feature)

```text
specs/009-gsm-resiliency-cli/
├── plan.md              ← this file
├── research.md          ← Phase 0 output
├── data-model.md        ← Phase 1 output
├── contracts/
│   └── control-protocol.md   ← Phase 1 output
└── tasks.md             ← Phase 2 output (/speckit-tasks)
```

### Source Code Changes (all in `gsm-sip-bridge/src/`)

```text
gsm-sip-bridge/src/
├── cli.rs                       MODIFY — add `card` subcommand tree
├── config/
│   └── mod.rs                   MODIFY — add ResilienceConfig + ControlConfig; update AppConfig
├── control/                     NEW
│   ├── mod.rs
│   ├── protocol.rs              NEW — ControlCmd / ControlResp JSON types
│   ├── server.rs                NEW — tokio Unix socket listener
│   └── client.rs                NEW — blocking Unix socket client (used by CLI subcommands)
├── modules/
│   ├── at_commander.rs          MODIFY — add query_imei, query_phone_number, query_network_type,
│   │                                     query_network_mode, set_network_mode
│   ├── card.rs                  MODIFY — add SlotState enum (Initializing/Ready/Recovering/GivenUp),
│   │                                     slot: u32 field, imei field, retry_count field
│   ├── mod.rs (CardPool)        MODIFY — exponential backoff, give-up tracking, IMEI→slot lookup,
│   │                                     startup diagnostics, apply stored network mode on init,
│   │                                     handle ControlCmd from control channel
│   └── supervisor.rs            NEW (optional) — extract recovery state machine if CardPool grows
│                                     too large (defer until needed per Principle V)
├── store/
│   ├── mod.rs                   MODIFY — add StoreCommand variants for slot/mode ops, add sync
│   │                                     query helpers for card_slots lookups
│   ├── schema.rs                MODIFY — v1→v2 migration, add card_slots + card_mode_prefs tables
│   └── slots.rs                 NEW — DB CRUD for card_slots and card_mode_prefs
└── main.rs                      MODIFY — detect card subcommand → run CLI path; otherwise daemon path
```
