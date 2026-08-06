# Implementation Plan: Discovery Retry & Missing-Line Health Reporting

**Branch**: `027-discover-retry-health` | **Date**: 2026-08-06 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/027-discover-retry-health/spec.md`

## Summary

An explicitly configured VoWiFi/VoLTE line (a `[[vowifi.line]]` `modem_port`/`modem_serial` pin, or a `pcsc_reader` entry) can be missed by `discover`'s single, one-shot startup scan — the real incident that motivated this feature was an EC20 modem still enumerating over USB when `discover` ran, leaving its line completely absent (not even in the existing `failed` list) for the container's entire life, invisible to `vowifi-status`, `healthcheck`, and every existing alert.

The fix is a bounded, startup-only retry loop in `supervise::orchestrate` around the existing `discover` step: keep re-invoking discovery for whichever configured overrides didn't resolve, until they do or a time-boxed window elapses, without touching lines that already succeeded. A configured override that discovery still can't match against any probed modem is a **new failure kind** (`NotFound`, distinct from the existing `SIM not usable`/`MaxLinesExceeded` reasons already tracked in `LineTableResult::failed`), surfaced through the three places operators already look: `vowifi-status` output (today only prints resolved lines), the container `healthcheck` (today only inspects resolved lines, so a wholly-missing line reads as healthy), and a new Discord alert category following the existing `CategoryAlertConfig`/threshold/`AlertPhase` pattern used by `registration_loss` and `tunnel_failure` — paired with a matching recovery notice — plus a new Prometheus gauge alongside the existing `gsm_sip_bridge_vowifi_registered`/`gsm_sip_bridge_vowifi_tunnel_up` per-line gauges.

## Technical Context

**Language/Version**: Rust 1.94.0 (edition 2021, per `rust-toolchain.toml`)
**Primary Dependencies**: Existing workspace crates only — no new external dependency anticipated. `serde`/`serde_json` (line-resolution file), `prometheus` (metrics registry, `metrics/mod.rs`), the existing `alerts::discord::DiscordClient` (webhook dispatch).
**Storage**: The existing JSON line-resolution file (`modules::discovery::lines_file_path()`), written by `discover` and read by `supervise`, `vowifi-status`, `healthcheck`, and the per-line agents. No database.
**Testing**: `cargo nextest` (falls back to `cargo test`) via `make test`; integration tests live in `gsm-sip-bridge/tests/` (notably `test_discovery.rs`, `test_ingest_critical_alerts.rs`, `test_cli.rs`, `test_metrics_endpoint.rs` are the closest existing analogs), unit tests inline (`#[cfg(test)]`) alongside the modules they cover, per the project constitution's Integration-First Testing principle.
**Target Platform**: Linux container (the existing `docker/` image), running against real or `pcsc_reader`-backed EC20/EC25 modem hardware passed through to the container.
**Project Type**: Single Rust workspace binary (`gsm-sip-bridge`) combining a CLI (`discover`, `vowifi-status`, `healthcheck`, …) and a long-running supervised daemon (`supervise`) — not a client/server or mobile split.
**Performance Goals**: Retry polling is lightweight (bounded number of USB rescans over a multi-minute window, not a tight loop); no measurable added latency for lines that resolve on the first pass (SC-005).
**Constraints**: Must never re-probe or reopen a serial port an already-running line's agent holds (the existing "modem claimed by both subsystems" hazard `modules/discovery.rs` already guards against for its ongoing rescans — this feature's retries need the same discipline for the lines still missing). Retry window is bounded (minutes, not indefinite) so a genuinely absent device settles into a terminal failed state rather than retrying forever.
**Scale/Scope**: Small: `[modules].max_concurrent`/`max_lines` bound the line count to single digits (observed deployments run 1-2 lines, capped well under 10). No scale concerns.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Assessment |
|---|---|
| I. Integration-First Testing | Pass — plan targets integration tests against the real `discover`/`resolve_lines`/`healthcheck::evaluate`/metrics-registry code paths (no new component is mockable-in-principle here; the one boundary that's impractical to run for real, an actual USB modem enumerating late, is simulated by feeding `scan_all`-equivalent inputs across two calls in a test, which is how `test_discovery.rs` already tests role assignment). |
| II. Green-on-Commit | Pass — no exception requested; `make test` must stay green throughout. |
| III. Frequent Atomic Commits | Pass — task breakdown (below, once `/speckit-tasks` runs) is structured as independently committable slices per user story. |
| IV. Makefile-Driven Build | Pass — no new build tooling; existing `make build/test/lint/format` targets cover this feature. |
| V. Simplicity & Refactorability | Pass — reuses existing patterns (`FailedLine`, `CategoryAlertConfig`/`AlertPhase`, `GaugeVec`) rather than introducing new abstractions or a new subsystem. |

No violations to justify; **Complexity Tracking** section is not needed.

## Project Structure

### Documentation (this feature)

```text
specs/027-discover-retry-health/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md         # Phase 1 output
├── quickstart.md         # Phase 1 output
├── contracts/             # Phase 1 output
│   ├── vowifi-status-output.md
│   ├── healthcheck-contract.md
│   └── metrics-and-alerts-contract.md
└── tasks.md               # Phase 2 output (/speckit-tasks — not created by /speckit-plan)
```

### Source Code (repository root)

```text
gsm-sip-bridge/
├── src/
│   ├── vowifi/
│   │   └── discovery.rs        # resolve_lines / LineTableResult / FailedLine — add NotFound reason,
│   │                            # add the retry-window-aware resolution state this feature introduces
│   ├── supervise/
│   │   └── orchestrate.rs      # the one-shot `discover` invocation (around line 194) becomes a
│   │                            # bounded retry loop for still-missing configured overrides
│   ├── commands/
│   │   ├── discover.rs         # handle_discover_command — no interface change, still one scan per call
│   │   ├── healthcheck.rs      # evaluate() — currently ignores LineTableResult::failed entirely;
│   │                            # must report degraded for a configured line stuck in NotFound/retrying
│   │   └── vowifi.rs            # handle_vowifi_status_command → vowifi::print_status
│   ├── vowifi/
│   │   └── mod.rs               # print_status — currently only iterates resolved `lines`, needs to
│   │                            # also report `failed`/retrying configured lines
│   ├── metrics/
│   │   ├── mod.rs               # new GaugeVec alongside VOWIFI_REGISTERED/VOWIFI_TUNNEL_UP
│   │   └── ingest.rs             # AlertCategory/AlertPhase pattern — new category for this failure,
│   │                            # triggered from the retry loop rather than from an AgentReport
│   ├── alerts/
│   │   └── mod.rs               # AlertCategory enum, CategoryAlertConfig lookup
│   └── config/
│       └── mod.rs                # AlertsConfig — new CategoryAlertConfig + threshold-ish config for
│                                 # this category (the "threshold" here is the retry window, not a
│                                 # running unhealthy-duration, since there's no live agent reporting)
└── tests/
    ├── test_discovery.rs         # extend: NotFound reason, retry-affecting resolution shape
    ├── test_cli.rs                # extend: vowifi-status output includes failed/retrying lines
    ├── test_metrics_endpoint.rs   # extend: new gauge appears with expected labels/values
    └── test_ingest_critical_alerts.rs  # extend or sibling: failure/recovery notification pairing
```

**Structure Decision**: Single existing Rust workspace crate (`gsm-sip-bridge`) — this feature is additive within already-established modules (`vowifi::discovery`, `supervise::orchestrate`, `commands::healthcheck`, `metrics`, `alerts`), not a new project or service boundary. No frontend/backend or mobile split applies.

## Complexity Tracking

*(Not applicable — no Constitution Check violations.)*
