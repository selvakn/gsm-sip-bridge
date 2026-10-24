# Implementation Plan: SMS Alert Delivery Status and Retry

**Branch**: `121-sms-alert-retry` | **Date**: 2026-10-24 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `/specs/121-sms-alert-retry/spec.md`

## Summary

Every received text already lands in the shared SQLite `sms` table with a
`forwarding_status`, but a `failed` row is never looked at again. This feature
tracks delivery state **in that same SQLite table** (schema v5: attempt count,
last attempt time, last failure reason, a retry lease, an `abandoned` state)
and adds a **retry sweep** that re-posts undelivered rows to Discord:

- after each fresh text's own first attempt finishes (the "next alert" trigger),
- and on a long timer (default 3600 s) in every process that owns an SMS Discord
  client (the daemon and the VoWiFi/VoLTE agent B).

Because up to three processes write the same database, duplicate-send safety
comes from an **atomic lease claim in SQLite** (`UPDATE … WHERE claimed_until
IS NULL OR claimed_until < now`), not from in-process locks. Status is exposed
by Prometheus gauges computed from the database at scrape time (so they are
correct no matter which process did the send) and by a new `sms-status` CLI.

## Technical Context

**Language/Version**: Rust (workspace crate `gsm-sip-bridge`, edition per `Cargo.toml`)
**Primary Dependencies**: existing — `rusqlite`, `reqwest`, `tokio`, `crossbeam-channel`, `prometheus`, `chrono`, `clap`. No new dependencies.
**Storage**: existing SQLite store (`[sms].db_path`, WAL, shared by daemon + agents); schema migration v4 → v5 via the `MIGRATIONS` table in `store/schema.rs`
**Testing**: `cargo test` via `make test`; integration tests in `gsm-sip-bridge/tests/` against a real SQLite file and a real local HTTP listener standing in for Discord (see Constitution Check)
**Target Platform**: Linux (Docker container on x86_64 and Raspberry Pi arm64)
**Project Type**: single Rust workspace (daemon + helper subcommands)
**Performance Goals**: first attempt of a fresh text unchanged (no added latency); a sweep of a 500-row backlog drains at a bounded pace without blocking new texts
**Constraints**: multiple writer processes on one DB (busy_timeout 5 s); VoWiFi agent B has no scrape endpoint of its own; `post_with_retry` is shared with the critical-alert path and must not change behaviour for it
**Scale/Scope**: single-digit lines, tens of texts/day; backlog worst case a few hundred rows after a multi-day outage

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Status | Notes |
|---|---|---|
| I. Integration-First Testing | PASS | Store/claim logic is tested against a real SQLite file; the Discord endpoint is a real local HTTP listener (tokio TCP) driven to return 204/429/500/close-connection — Discord itself cannot be called from CI, so a local listener is the justified stand-in, noted at the mock site. |
| II. Green-on-Commit | PASS | Each task ends with `make format && make lint && make test`. |
| III. Frequent Atomic Commits | PASS | Tasks are ordered so each commit builds and tests green (schema first, then store API, then client, then sweep, then wiring). |
| IV. Makefile-Driven Build | PASS | No new entry points; uses `make test`/`make lint`. |
| V. Simplicity & Refactorability | PASS | Reuses the existing table, store thread and `post_with_retry`; no new queue, daemon or crate. The one new mechanism (lease column) is the minimum that makes multi-process safety correct. |

Post-design re-check: still PASS (no new projects, layers or dependencies).

Project rule (CLAUDE.md): no real phone numbers in fixtures — tests use `+919000000000`-style placeholders.

## Project Structure

### Documentation (this feature)

```text
specs/121-sms-alert-retry/
├── plan.md              # This file
├── research.md          # Phase 0 decisions
├── data-model.md        # Phase 1: schema v5, state machine
├── quickstart.md        # Phase 1: manual + automated verification
├── contracts/
│   ├── config.md        # new [sms] keys
│   ├── metrics.md       # new gauges
│   ├── cli.md           # sms-status output
│   └── discord-payload.md  # late-delivery embed
└── tasks.md             # /speckit-tasks output
```

### Source Code (repository root)

```text
gsm-sip-bridge/
├── src/
│   ├── store/
│   │   ├── schema.rs        # SCHEMA_V5_SQL + MIGRATIONS row; SCHEMA_VERSION = "5"
│   │   └── sms.rs           # claim/complete/abandon/summary queries; insert carries lease
│   ├── sms/
│   │   ├── mod.rs           # record_and_forward: lease on insert, sweep after first attempt
│   │   ├── retry.rs         # NEW: sweep(), spawn_retry_timer(), outcome types
│   │   └── discord.rs       # (unchanged; re-export)
│   ├── alerts/discord.rs    # typed PostError; forward_sms(late) marker; send_alert unchanged
│   ├── config/{mod,raw}.rs  # [sms] retry_enabled / retry_interval_sec / retry_max_age_sec
│   ├── metrics/{mod,server}.rs  # undelivered gauges refreshed at scrape time
│   ├── commands/sms_status.rs   # NEW: `sms-status` subcommand
│   ├── cli.rs               # register SmsStatus
│   ├── commands/daemon.rs   # start retry timer
│   └── vowifi/mod.rs        # start retry timer on sms_runtime
├── tests/
│   ├── test_sms_retry_store.rs     # NEW: migration, claim, lease, age limit
│   ├── test_sms_retry_sweep.rs     # NEW: sweep against local fake Discord
│   ├── test_sms_status_metrics.rs  # NEW: gauges + CLI
│   └── test_config.rs / test_config_docs.rs  # extended for new keys
docs/{configuration,observability,operations}.md, config.toml.example, CHANGELOG.md
```

**Structure Decision**: Single-crate change inside the existing `gsm-sip-bridge` workspace member. All retry logic lives in a new `sms/retry.rs` so `record_and_forward` stays readable; SQL stays in `store/sms.rs` where the other `sms` queries already live.

## Complexity Tracking

No constitution violations to justify.
