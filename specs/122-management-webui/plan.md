# Implementation Plan: Read-only management web UI

**Branch**: `122-management-webui` | **Date**: 2026-10-24 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `specs/122-management-webui/spec.md`

## Summary

The daemon gets a second, authenticated HTTP listener (default `:8080`, off by default) that serves a small embedded single-page UI and a read-only JSON API. The UI has five pages: Overview, Calls, SMS, Events and Config.

Almost everything shown already exists in the daemon. The feature adds four pieces of data:

1. **Signal polling**: piggy-backed on the two existing modem-owning loops. These are the CS `ModuleWorker` liveness tick and the VoWiFi/VoLTE modem SMS sweep. Readings are exported as Prometheus gauges and carried to the daemon in a new optional `AgentReport.details`.
2. **A persisted `events` timeline**: recorded at `alerts::dispatch` and at the existing tunnel/uplink/module counter sites, with in-process raise/resolve dedup.
3. **Outbound call rows**: a `direction` column, plus per-call quality columns in the `calls` table (schema v5).
4. **A redacted config snapshot**: taken from the pre-`env:`-resolution TOML.

The frontend is three files with no framework and no build step, embedded with `include_str!`. That keeps image growth to a few hundred KB and keeps Node out of the build.

## Technical Context

**Language/Version**: Rust (workspace toolchain per `rust-toolchain.toml`), plain ES2020 JavaScript, CSS
**Primary Dependencies**: All existing: `axum` 0.8, `tokio`, `serde`/`serde_json`, `rusqlite` (bundled), `prometheus` 0.13, `rand` 0.9, `base64`, `chrono`. **New direct dependency**: `subtle` 2.6 (already in `Cargo.lock` transitively; for constant-time password comparison). No JavaScript dependencies.
**Storage**: Existing SQLite (`[sms].db_path`, WAL, several writer processes). Schema v4 → v5: `calls.direction`, `calls.loss_percent`/`jitter_ms`/`rtt_ms`, and a new `events` table.
**Testing**: `cargo test` through `make test`. Integration tests in `gsm-sip-bridge/tests/test_webui_*.rs` run the real router on `127.0.0.1:0` with a real temp SQLite file and a `reqwest` client. No mocks (Constitution I).
**Target Platform**: Linux containers on amd64 and arm64 (Raspberry Pi), with `network_mode: host`.
**Project Type**: An addition to the existing single-crate daemon (a web service plus an embedded static frontend).
**Performance Goals**:
- Each page loads in under 2s on the Pi with 10k calls and 10k SMS (SC-010).
- 5s refresh while the page is visible.
- Negligible CPU when idle (SC-008).

**Constraints**:
- Image growth under 1 MB, and no JavaScript toolchain (FR-070).
- Works offline (FR-071).
- No new AT port users (FR-021).
- No effect on call or audio paths (FR-074).
- Disabled UI means `/metrics` is unchanged apart from the new signal and event series (SC-009).

**Scale/Scope**: A single operator, 1–4 lines per bridge, and a handful of concurrent browser tabs.

All open questions are resolved in [research.md](research.md) (R1–R15). Nothing is marked NEEDS CLARIFICATION.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-checked after Phase 1 design.*

| Principle | How the plan complies | Status |
|---|---|---|
| I. Integration-First Testing | Every API behaviour is tested through the real router, real SQLite and real `apply_report`. Pure parsers and the health derivation get unit tests in addition. Hardware-only checks are listed in quickstart.md as live verification, not mocked | PASS |
| II. Green-on-Commit | `make format && make lint && make test` before every commit (CLAUDE.md). Each task in tasks.md is sized to land green on its own | PASS |
| III. Frequent Atomic Commits | Tasks are ordered so each is one logical commit: schema, then protocol, then each producer, then the API, then each page | PASS |
| IV. Makefile-Driven Build | No new targets are needed. The assets compile in via `include_str!`, so `make build`/`make test`/`make lint` cover them. The asset grep check is a Rust test, so it runs under `make test` | PASS |
| V. Simplicity & Refactorability | No framework and no build step. The UI is served from the process that already has the data. Signal polling reuses the existing modem loops instead of adding a poller. One justified addition, the separate `details` map in ingest, is recorded under Complexity Tracking | PASS (1 justified) |

**Post-design re-check**: still PASS. The design adds no new processes, threads (beyond one axum listener task and one periodic prune timer) or AT port users.

## Project Structure

### Documentation (this feature)

```text
specs/122-management-webui/
├── spec.md
├── plan.md              # this file
├── research.md          # R1–R15
├── data-model.md        # schema v5, in-memory views
├── quickstart.md        # enable, dev loop, verification matrix
├── contracts/
│   ├── http-api.md      # routes, auth, payloads
│   ├── config.md        # [webui], [metrics].signal_poll_interval_seconds, [alerts].event_retention_days
│   ├── agent-report.md  # AgentReport.details
│   └── metrics.md       # new Prometheus series
├── checklists/requirements.md
└── tasks.md             # /speckit-tasks
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/
├── webui/                     # NEW
│   ├── mod.rs                 # serve(): listener, router, security headers
│   ├── auth.rs                # sessions, login/logout, rate limit, middleware
│   ├── api.rs                 # /api/* handlers (overview, calls, sms, events, health, config)
│   ├── lines.rs               # LineView assembly + pure health derivation
│   ├── queries.rs             # read-only SQLite queries (paged), own connection
│   ├── config_view.rs         # pre-resolution TOML → redacted JSON
│   ├── signal.rs              # daemon-local CS signal map
│   └── assets/
│       ├── index.html
│       ├── app.js
│       └── style.css
├── modules/signal.rs          # NEW: AT+CSQ / AT+QENG parse → SignalReading (shared by CS + IMS paths)
├── store/
│   ├── schema.rs              # v5 migration
│   ├── calls.rs               # direction + quality columns
│   └── events.rs              # NEW: EventKind, insert/prune, process-global sink + raise/resolve dedup
├── control/protocol.rs        # AgentReport.details, AgentDetails, *Wire types
├── metrics/
│   ├── mod.rs                 # signal gauges, events_recorded_total, webui_logins_total
│   ├── ingest.rs              # store details; record tunnel/uplink events
│   └── server.rs              # clear stale details; signal age refresh
├── alerts/mod.rs              # dispatch() → events::record
├── modules/worker.rs          # signal poll in tick_liveness_probe (idle only)
├── modules/pool/{mod.rs,dispatch.rs,restart_cycle.rs}  # module/restart events, CS outbound rows
├── volte/sms.rs               # signal poll in run_modem_reader
├── ims/agent/mod.rs, ims/observability.rs, ims/agent/call.rs  # details reporting, quality columns
├── vowifi/mod.rs              # outbound rows, outbound active_call_details
├── supervise/orchestrate.rs   # StoreHandle + events sink
├── config/{mod.rs,raw.rs,build.rs}  # [webui] + new keys, snapshot of pre-resolution document
└── commands/daemon.rs         # start webui::serve, events sink, retention timer

gsm-sip-bridge/tests/
├── test_webui_auth.rs  test_webui_overview.rs  test_webui_calls.rs
├── test_webui_sms.rs   test_webui_events.rs    test_webui_config.rs
└── test_webui_assets.rs   (+ additions to test_migration_sql.rs, test_config.rs, test_metrics_endpoint.rs)

config.toml.example, docs/ (config reference), docker/.env example
```

**Structure Decision**: This is an addition to the existing `gsm-sip-bridge` crate, in a new `webui` module plus targeted edits to producers. There's no separate frontend project, because the assets are three embedded files.

## Delivery order

This maps to the spec's priorities. Each step is independently demonstrable.

1. **Foundation**:
   - schema v5;
   - `[webui]` config plus the config snapshot;
   - `webui::serve` with auth and an empty shell;
   - `AgentReport.details` plumbing.

   Delivers US6 (auth) on its own.
2. **US1 + US2 (P1)**:
   - Overview from ingest (without signal at first);
   - SMS page;
   - then signal polling: the CS tick, then the IMS sweep.
3. **US3 (P2)**: the Calls page, then outbound rows (VoWiFi/VoLTE, then CS), then per-call quality, then live active calls.
4. **US4 (P2)**: the events table and sink, then hooks (dispatch, ingest, pool, supervise), then retention, then the page.
5. **US5 (P3)**: the Config page with the redaction test.
6. **Polish**: docs, the live-verification run, and the image-size check.

## Risks

| Risk | Mitigation |
|---|---|
| The signal read in the IMS sweep adds AT contention and starves `vowifi-usim-bridge` during EAP-SIM | One port hold for both commands, under `modem_lock`, through the existing `open_with_retry`, at most once per 60s. Live check in quickstart step 2 before merge |
| The events table grows unbounded on a flapping line | Retention prune every 6h, plus dedup of retried raises. A flapping tunnel is one raise per outage (the folded decision in ingest), not one per report |
| The UI port collides with something on a host-network deployment | Configurable port, and a bind failure is non-fatal (FR-004) |
| Secrets leak through the Config page | Redaction by construction (pre-resolution document), plus the sentinel test over every `Secret` key (R11) |
| XSS through SMS body or caller ID | `textContent`-only rendering, a strict CSP, and a test that forbids HTML sinks in `app.js` |
| Schema v5 migration races between processes at upgrade | The existing migration loop already runs under `busy_timeout` and re-reads `schema_version`, and is covered by `test_store_concurrent_writers.rs` |

## Complexity Tracking

| Addition | Why needed | Simpler alternative rejected because |
|---|---|---|
| A separate `ingest::details()` map next to the `liveness()` map | `AgentRecord`/`AgentState` are `Copy` and read with `.copied()` all through ingest; the new fields include `String`/`Vec` | Dropping `Copy` would touch every ingest code path and the alert state machine for a display-only concern |
| A process-global events sink (`OnceLock<Sender<StoreCommand>>`) | Events originate in four processes and deep inside `alerts::dispatch`, which has no store handle | Threading a store sender through every `dispatch` call site and `AlertContext` changes far more signatures for the same effect |
