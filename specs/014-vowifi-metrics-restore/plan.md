# Implementation Plan: Restore Call and SMS Observability Under VoWiFi

**Branch**: `014-vowifi-metrics-restore` | **Date**: 2026-07-21 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `/specs/014-vowifi-metrics-restore/spec.md`

## Summary

The VoWiFi agents handle calls and SMS in processes that own no Prometheus
registry, so the dashboard has been blind to them. This plan keeps a **single
scrape target** by having the agents *report events* to the daemon that already
serves `/metrics`, over the **existing CLI control Unix socket** — which works
unchanged from inside the `ims` network namespace, because Unix domain sockets
are not network-namespaced.

The daemon owns every counter. Agents send deltas, never absolute counts, so a
supervised agent restart cannot reset or rewind a counter. Agents additionally
re-report their full state on a fixed 10-second heartbeat, which is what makes
health indicators re-converge after the *daemon* restarts, and what makes a dead
agent distinguishable from an idle one (the daemon expires stale agents at scrape
time).

Call and SMS **history** stays out of that path entirely: each agent writes its
own rows through its own `StoreHandle` against the shared WAL database, exactly
as Agent B already does for SMS. History therefore survives the daemon being
down, per the spec's stated assumption.

## Technical Context

**Language/Version**: Rust (workspace pinned via `rust-toolchain.toml`)
**Primary Dependencies**: `prometheus` (registry + text encoding), `axum` (metrics endpoint), `tokio` (control server), `serde_json` (newline-JSON framing), `rusqlite` (history), `crossbeam-channel` (store writer)
**Storage**: SQLite in WAL mode, shared by daemon + both agents; schema versioned in `meta.schema_version`, currently `2`
**Testing**: `cargo test --workspace` — integration tests in `gsm-sip-bridge/tests/`, unit tests in-module
**Target Platform**: Linux (Docker, `network_mode: host`), agents split across the default netns and the `ims` netns
**Project Type**: Single Rust workspace — daemon + CLI + agent subcommands in one binary
**Performance Goals**: Reporting must be off the call path; a burst of 10 calls + 10 SMS in one minute must not affect call setup or audio (SC-008)
**Constraints**: Observability MUST NOT fail a call (FR-018); buffer is bounded, never unbounded (FR-019a); single scrape target, no new Prometheus config (FR-015)
**Scale/Scope**: 1–4 modems per host, a handful of concurrent calls; event rate is single-digit per call

## Constitution Check

*GATE: checked before Phase 0, re-checked after Phase 1 design.*

| Principle | Assessment |
|---|---|
| I. Integration-First Testing (NON-NEGOTIABLE) | **PASS.** Every layer here runs locally with no hardware: the control socket is a real `UnixListener`, the registry is real, SQLite is real. Tests bind a real socket in a temp dir, send real newline-JSON, and scrape the real `/metrics` handler. No mocks are needed or introduced — the only piece not testable locally (a live carrier call) is already covered by the manual live-test convention from `specs/011`/`specs/012`. |
| II. Green-on-Commit (NON-NEGOTIABLE) | **PASS.** Each task is independently green. The label-widening task (adding `transport`) touches every existing call site in one commit so the tree never sits half-migrated. |
| III. Frequent Atomic Commits | **PASS.** Work decomposes into ~8 commits along natural seams: schema migration, protocol types, daemon ingest, agent reporter, Agent A wiring, Agent B wiring, dashboard, docs. |
| IV. Makefile-Driven Build | **PASS.** No new tooling; `make test` / `make lint` cover everything. No new targets required. |
| V. Simplicity & Refactorability | **PASS with one justified addition** — see Complexity Tracking. The design reuses the existing socket, existing framing, existing store handle, and existing metric names rather than introducing a metrics sidecar, a push gateway, or a second scrape target. |

**Post-Phase-1 re-check**: still PASS. The design adds no new dependency, no new
process, no new network listener, and no new configuration file. The one new
config key (`[metrics].agent_report_interval_seconds`) has a working default and
is optional.

## Project Structure

### Documentation (this feature)

```text
specs/014-vowifi-metrics-restore/
├── plan.md              # This file
├── research.md          # Phase 0 — decisions and rejected alternatives
├── data-model.md        # Phase 1 — event types, metric inventory, schema v3
├── quickstart.md        # Phase 1 — how to verify the fix end to end
├── contracts/
│   ├── observability-protocol.md   # Agent → daemon wire contract
│   └── metrics-inventory.md        # The exported metric surface
├── checklists/
│   └── requirements.md  # Spec quality checklist (from /speckit-specify)
└── tasks.md             # Phase 2 — NOT created by /speckit-plan
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/
├── control/
│   ├── protocol.rs        # + ControlCmd::Observe { report }  (new variant)
│   └── server.rs          # + short-circuit Observe → metrics ingest, never CardPool
├── metrics/
│   ├── mod.rs             # + transport label on 6 existing vecs; + 7 new metrics
│   ├── server.rs          # + expire stale agents at scrape time
│   └── ingest.rs          # NEW — applies a report to the registry
├── observability/
│   └── reporter.rs        # NEW — agent-side bounded buffer + sender thread
├── ims/
│   └── agent.rs           # Agent A: report call lifecycle + registration/tunnel; write call rows
├── vowifi/
│   └── mod.rs             # Agent B: report SMS + PBX-leg outcomes
├── modules/
│   ├── discovery.rs       # + resolve a modem port to its module id
│   └── mod.rs             # CS call sites pass transport="cs"
├── sms/mod.rs             # record_and_forward takes a transport
└── store/
    ├── schema.rs          # v2 → v3: transport column on calls + sms
    └── calls.rs           # CallRecord gains transport

gsm-sip-bridge/tests/      # integration tests (see Phase 1 § test strategy)
docker/grafana/provisioning/dashboards/gsm-sip-bridge.json   # additive panels
docs/observability.md      # metric table + transport semantics
```

**Structure Decision**: No new crate, no new binary, no new directory tree. Two
new modules (`metrics/ingest.rs`, `observability/reporter.rs`) slot into existing
trees. `observability/` already exists and already holds cross-cutting concerns
(`logging.rs`, `modemmanager.rs`), which makes it the natural home for the
agent-side reporter that both agents link.

## Phase 0 — Research

See [research.md](research.md). Decisions reached:

1. **Transport for agent → daemon events**: the existing control Unix socket, one
   short-lived connection per report. Unix sockets cross the netns boundary; the
   newline-JSON framing and its read/write helpers already exist and are tested.
2. **Counter ownership**: the daemon owns all cumulative state; agents send
   deltas, plus absolute values for gauges only.
3. **Event ownership** (the exactly-once mechanism, FR-017): Agent A is the sole
   reporter of VoWiFi call events and the sole writer of VoWiFi call rows — it
   sees 100% of inbound INVITEs, including those that never reach Agent B. Agent
   B is the sole reporter of SMS events and PBX-leg outcomes, and the sole writer
   of SMS rows. No event has two possible sources.
4. **Health signal for the tunnel**: derived from Agent A's own view (P-CSCF
   assignment present + its transport to the P-CSCF alive), not from charon's SA
   state via vici — see research.md for why vici was rejected for now.
5. **Module identity**: `derive_module_id` applied to the VoWiFi modem's USB
   serial, resolved from `[vowifi].modem_port` through sysfs — the same function
   the circuit-switched path uses, so the ids coincide when one modem does both.

## Phase 1 — Design & Contracts

See [data-model.md](data-model.md), [contracts/observability-protocol.md](contracts/observability-protocol.md),
[contracts/metrics-inventory.md](contracts/metrics-inventory.md), and [quickstart.md](quickstart.md).

### Test strategy (Principle I)

Every acceptance scenario maps to an integration test that runs with no hardware:

| Spec item | Test |
|---|---|
| FR-001..008, FR-017 | `test_observability_ingest.rs` — drive real reports through a real socket into the real registry; assert the encoded `/metrics` text |
| FR-011c/d | `test_migration_sql.rs` (extend) — open a v2 DB with rows, migrate, assert every row reads back with `transport='cs'` |
| FR-019/019a/019b | `test_observability_reporter.rs` — point a reporter at a dead socket, overflow the bound, start the listener, assert delivery resumes and the drop count is reported |
| FR-020 | same test — restart the reporter, assert daemon-side counters keep climbing |
| FR-021/021a/021b, SC-009/010 | `test_agent_liveness.rs` — stop heartbeating, scrape, assert `agent_up` flips to 0 and active calls zero out |
| FR-022, SC-006 | `test_metrics_endpoint.rs` (extend) — assert the full metric surface with VoWiFi disabled |

### Agent context update

`CLAUDE.md`'s `<!-- SPECKIT START -->` block now points at this plan.

## Spec Delta (resolved)

SC-006 originally required that with VoWiFi disabled, every existing metric
reports "identical values and identical series to the pre-change build". That
conflicted with FR-005/FR-008 (transport identifiable on the same metrics the
circuit-switched path uses): a Prometheus metric has one fixed label set across
all its series, so satisfying FR-005 means `gsm_sip_bridge_calls_total` gains a
`transport` label on every series, circuit-switched ones included.

Resolved 2026-07-21 by amending SC-006 in spec.md to: "every existing metric
reports identical values for the same traffic, and its series are unchanged
apart from a constant transport dimension fixed to the circuit-switched value;
every existing dashboard panel renders identically." Values and panels stay
identical; series identity gains one constant-valued label. No further design
change follows from this — the plan already assumed this shape.

## Complexity Tracking

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|-------------------------------------|
| A bounded buffer + background sender thread per agent (`observability/reporter.rs`) rather than a direct blocking send at the call site | FR-018 forbids observability from delaying or failing a call, and the daemon can be mid-restart when a call ends. A direct send would put a connect-and-write in the middle of call teardown. | Fire-and-forget with no buffer was rejected by the Q1 clarification — it loses exactly the calls that complete during a routine daemon restart. Blocking sends were rejected because a hung daemon would then hang call teardown. |
