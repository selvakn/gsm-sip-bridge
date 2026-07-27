# Implementation Plan: Discord Alerts for Critical Events

**Branch**: `022-discord-critical-alerts` | **Date**: 2026-07-27 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `specs/022-discord-critical-alerts/spec.md`

## Summary

Extend the existing SMS-to-Discord forwarding into a general, configurable
critical-event alerting mechanism covering four new categories — module/modem
lifecycle failure (SIM/discovery/AT-worker), IMS/SIP registration loss,
VoWiFi tunnel failure, and PBX-missed calls — each alerting only once a
condition survives past the system's own built-in auto-recovery (a bounded
retry count for SIM resets, or a 5-minute continuous-unhealthy threshold for
registration/tunnel, per spec Clarifications Q7–Q9), and sending a distinct
recovery notice once healthy again. Configuration lives in a new `[alerts]`
`config.toml` section: one shared default webhook, per-category
enable/disable (SMS defaults on, the four new categories default off), and
per-category webhook overrides.

Architecturally this reuses, rather than reinvents, four things already in
the codebase: the async `DiscordClient` (`sms::discord`), the
sync→async bridging pattern `sms::record_and_forward` already documents for
non-tokio callers, the `AgentState.{registered,tunnel_up}` fields every
VoWiFi/VoLTE line agent already reports into `metrics::ingest`, and the
`supervise::sim_recovery::Action::GiveUpForThisIncident` signal that exists
in the type system today but is not yet acted on anywhere (see
[research.md](research.md) R1–R4).

## Technical Context

**Language/Version**: Rust stable (unchanged, pinned by `rust-toolchain.toml`).

**Primary Dependencies**: None new. Reuses `reqwest` (async, via the existing
`DiscordClient`), `tokio` (already a full-featured dependency; one small
dedicated `Runtime` built inside `supervise::orchestrate::run`, matching the
precedent `vowifi::mod`'s accept loop already sets), `prometheus` (two new
metric families), `chrono`, `tracing`.

**Storage**: None new. The existing `sms` SQLite table is unchanged; the four
new categories are logged (`tracing`) and metered (`prometheus`) only —
no new table, no migration (research.md R5).

**Testing**: `cargo test --workspace` (unchanged gate). Concretely:
- Config parsing: table-driven unit tests over `parse_alerts` (new function
  in `config/mod.rs`), mirroring existing `parse_sms`/`parse_scheduled_restart`
  tests — including the `[sms]`-without-`[alerts.sms]` backward-compat case.
- Duration-threshold logic (`metrics::ingest`'s new
  `evaluate_critical_alerts`, and the AT-worker-unresponsive check in
  `modules::mod`): unit tests constructing `Instant`s in the past
  (`Instant::now() - Duration::from_secs(301)`) — real `Instant` arithmetic,
  no sleeping, no mock (Constitution I).
- Discord delivery: `wiremock`, the same external-service mock already
  justified and used in `tests/test_sms_discord.rs` — extended to cover the
  generalized alert payload for all five categories.
- `supervise::orchestrate`'s new `GiveUpForThisIncident` handling: extends
  the existing table-driven `sim_recovery` tests (already pure, already
  synchronous, no process execution) plus one `MockCommandRunner`-based
  assertion that the alert dispatcher is invoked, matching the module's
  existing mock-injection convention.
- Integration: one end-to-end test per category posting through a
  `wiremock` server and asserting the resulting `critical_alerts_total`
  counter and log line, similar in shape to `test_sms_discord.rs`.

**Target Platform**: Linux, unchanged — no new capabilities, no Dockerfile/
compose changes.

**Project Type**: Extension of the existing `gsm-sip-bridge` binary. New
`alerts` module; modifications to `config`, `metrics::ingest`, `metrics::mod`,
`modules::mod`, `supervise::orchestrate`. No new crate.

**Performance Goals**: SC-001 (Discord message within 30s of internal
detection); SC-002 (zero measurable impact on call setup, audio, or SMS
latency) — satisfied structurally by keeping every Discord POST
fire-and-forget (`handle.spawn`/dedicated-runtime `.spawn`, never `.await`ed
by a call/SMS/AT-command hot path), exactly as `sms::record_and_forward`
already does today.

**Constraints**: Zero new `unsafe` (unchanged gate). No new concurrency
model — every alert-dispatch call site uses one of the two patterns already
established in the codebase (`Handle::current()` from async, or a small
dedicated `Runtime` from sync), never a bespoke third shape.

**Scale/Scope**: Five alert categories total (one existing + four new), one
new config section, two new metric families, no new processes, no new
persistent storage.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Status | Notes |
|---|---|---|
| I. Integration-First Testing | PASS | Real `Instant` arithmetic for duration thresholds (no fake clock/mock); `wiremock` for Discord is the same, already-justified external-service carve-out `test_sms_discord.rs` uses. No new mocks introduced beyond that precedent. |
| II. Green-on-Commit | PASS | Each task ends with `cargo fmt --all && make lint && cargo test --workspace` per the repo's mandatory pre-commit checklist. |
| III. Frequent Atomic Commits | PASS | Phased below (config → GSM-side categories → ingest-side categories → supervise-side category → docs); each phase is independently committable and testable. |
| IV. Makefile-Driven Build | PASS | No new build tooling; existing `make build/test/lint` targets cover the new module. |
| V. Simplicity & Refactorability | PASS | No new DB table, no new crate, no new concurrency model, no new HTTP client — every new piece reuses an existing, precedented mechanism (research.md R1–R7). The one real complexity — five categories spread across three different subsystems (GSM async loop, central ingest, supervise sync threads) — is inherent to where each category's signal already lives, not an added abstraction. |

No violations to justify; Complexity Tracking table omitted.

## Project Structure

### Documentation (this feature)

```text
specs/022-discord-critical-alerts/
├── plan.md              # this file
├── research.md          # Phase 0 output
├── data-model.md         # Phase 1 output
├── quickstart.md         # Phase 1 output
├── contracts/
│   ├── config-schema.md  # Phase 1 output
│   └── metrics.md        # Phase 1 output
└── tasks.md              # Phase 2 output (/speckit.tasks, not this command)
```

### Source Code (repository root)

Single Rust workspace, existing `gsm-sip-bridge` crate — no new crate, no
frontend/backend split.

```text
gsm-sip-bridge/src/
├── alerts/                      # NEW module
│   ├── mod.rs                   # AlertCategory, CriticalEvent, dispatch fn(s)
│   └── discord.rs                # generalizes sms::discord::DiscordClient's
│                                  # embed-building for all 5 categories (the
│                                  # existing forward_sms stays as a thin
│                                  # wrapper calling the new generic sender)
├── config/
│   └── mod.rs                    # MODIFIED: AlertsConfig, CategoryAlertConfig,
│                                  # parse_alerts(), [sms]→[alerts.sms] seeding
├── control/
│   └── protocol.rs                # unchanged (AgentState already has the
│                                  # fields this feature reads)
├── metrics/
│   ├── mod.rs                    # MODIFIED: 2 new metric families (contracts/metrics.md)
│   └── ingest.rs                  # MODIFIED: unhealthy_since tracking +
│                                  # evaluate_critical_alerts()
├── modules/
│   └── mod.rs                     # MODIFIED: AT-worker-unresponsive timer,
│                                  # SIM absent/unreadable + missed-call hooks
│                                  # into the new alerts module
├── sms/
│   ├── mod.rs                     # MODIFIED: record_and_forward routes
│                                  # through alerts::dispatch for the sms
│                                  # category (FR-001), behavior unchanged
│   └── discord.rs                 # MODIFIED or thinned per alerts/discord.rs above
└── supervise/
    ├── orchestrate.rs             # MODIFIED: handle GiveUpForThisIncident
                                    # (research.md R2); build the dedicated
                                    # Runtime + AlertsConfig at startup
    └── sim_recovery.rs            # unchanged (GiveUpForThisIncident already exists)

gsm-sip-bridge/tests/
├── test_config.rs                 # MODIFIED: [alerts] parsing cases
├── test_alerts_discord.rs         # NEW: wiremock-based, all 5 categories
└── test_ingest_critical_alerts.rs # NEW: unhealthy_since / threshold logic
```

**Structure Decision**: Single project (existing `gsm-sip-bridge` binary +
workspace). No new crate: the feature is additive wiring across five
existing modules plus one new small `alerts` module, matching the scale of
prior features in this codebase (e.g., `006-sms-discord-forward`,
`014-vowifi-metrics-restore`) rather than justifying a new workspace member.

## Complexity Tracking

*No Constitution violations — table intentionally omitted.*
