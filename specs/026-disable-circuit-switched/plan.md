# Implementation Plan: Disable Circuit-Switched Handling

**Branch**: `026-disable-circuit-switched` | **Date**: 2026-08-04 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `specs/026-disable-circuit-switched/spec.md`

## Summary

Add `[cs].enabled` (default `true`) to gate the circuit-switched call path. The daemon process keeps running and keeps hosting the metrics endpoint, control socket, and message store — the VoWiFi and VoLTE subsystems depend on all three. What stops is `CardPool`: no modem discovery, no periodic rescan, no AT traffic, no circuit-switched calls.

Three consequences drive most of the work beyond the flag itself:

1. `CardPool::run` currently owns the control-command receiver. Not running it would leave card commands hanging with no reply, so a small responder task must drain the channel and answer "disabled".
2. The daemon's own SIP side must stay down (FR-009a), which folds into the existing `owns_sip_side` suppression rather than adding a parallel mechanism.
3. The VoWiFi role assignment reserves voice-capable modems for circuit-switched use; with the path off it must stop reserving (FR-010a).

The metrics requirement (FR-021a — circuit-switched series absent, not zeroed) needs no work: every such metric is a `once_cell::sync::Lazy` that registers into the registry on *first touch*, and every touch site is inside `modules/mod.rs`. Not running `CardPool` means they are never registered and never exported. This is verified by test, not assumed.

## Technical Context

**Language/Version**: Rust (workspace pinned by `rust-toolchain.toml`)
**Primary Dependencies**: `serde`/`toml` (config), `prometheus` + `once_cell` (metrics), `tokio` (daemon runtime), `pjsua-safe` (SIP)
**Storage**: SQLite via `store::StoreHandle` — unchanged by this feature
**Testing**: `cargo test` via `make test`; integration tests in `gsm-sip-bridge/tests/`
**Target Platform**: Linux container on the EC20/EC25 gateway host
**Project Type**: Single Rust workspace — CLI + long-running daemon
**Performance Goals**: Eliminate the periodic bus scan (~2,880/day at the default 30 s interval) on a VoWiFi-only deployment
**Constraints**: Strictly backward compatible — an existing config with no `[cs]` section must behave identically (FR-002, User Story 2). Unknown config keys are a hard startup error, so the new section must be registered in `section_key_lists()` *and* documented, or `test_config_docs.rs` fails.
**Scale/Scope**: ~10 source files, 4 documentation/config files; no new crate, no new dependency

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Assessment | Verdict |
|---|---|---|
| I. Integration-First Testing | Every requirement is testable through real components: config loading via `load_config` on real TOML, gating via the real `CardPool`/daemon wiring, metrics via the real registry and a real scrape, modem assignment via the real `RoleAssignment::from_probed`. No new mocks are introduced. The existing `CommandRunner` trait already has a conformance suite covering both implementations, so `supervise`-level assertions reuse it. | PASS |
| II. Green-on-Commit | `make format && make lint && make test` before each commit, per `CLAUDE.md`. `make lint` covers all test targets. | PASS |
| III. Frequent Atomic Commits | Tasks are grouped into commit-sized units in `tasks.md`, each one logically self-contained (config plumbing, then each gating site, then docs). | PASS |
| IV. Makefile-Driven Build | No new tooling; existing `make` targets suffice. | PASS |
| V. Simplicity & Refactorability | The gate is a single boolean threaded to five decision points, not an abstraction layer. FR-009a explicitly requires *reusing* the existing telephone-side suppression rather than adding a second mechanism. The one genuinely new construct is the disabled-command responder, justified below. | PASS |

**Complexity note (Principle V)**: the disabled-command responder is new code rather than a reuse. It exists because `CardPool::run` is the sole consumer of `control_rx`; without a replacement drainer, `card list`/`card restart` would block forever with no reply, violating FR-020's "rather than failing obscurely or hanging". The alternative — running `CardPool` with an empty slot table — was rejected because `CardPool::new`/`run` also start the scheduler, touch the CS metrics (defeating FR-021a), and call `sip_bridge.register()`, so "empty pool" would not actually satisfy the requirements. The responder is ~30 lines and holds no state.

No gate violations. No entries required in Complexity Tracking.

## Project Structure

### Documentation (this feature)

```text
specs/026-disable-circuit-switched/
├── plan.md              # This file
├── spec.md              # Feature specification (clarified)
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/
│   ├── config-schema.md     # [cs] section contract
│   ├── control-protocol.md  # Card-command responses when disabled
│   └── metrics-contract.md  # Which series appear/disappear
├── checklists/
│   └── requirements.md
└── tasks.md             # Phase 2 output (/speckit-tasks)
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/
├── config/
│   ├── raw.rs               # + RawCs section, manual Default (true), section_key_lists entry
│   ├── build.rs             # + build_cs, wire into build()
│   └── mod.rs               # + CsConfig, AppConfig.cs
├── commands/
│   ├── daemon.rs            # Gate CardPool; startup warnings; CS_ENABLED gauge; CLI-override conflict
│   └── healthcheck.rs       # Report the path as intentionally disabled
├── modules/
│   └── mod.rs               # (unchanged internals — gated from daemon.rs)
├── control/
│   └── disabled.rs          # NEW: responder that answers card commands while the path is off
├── sip/
│   └── mod.rs               # owns_sip_side gains && cs.enabled; log names the reason
├── vowifi/
│   └── discovery.rs         # RoleAssignment::from_probed takes cs_enabled
├── commands/discover.rs     # Pass config.cs.enabled into from_probed
└── metrics/mod.rs           # + CS_ENABLED gauge

gsm-sip-bridge/tests/
├── test_config.rs           # Default-true, explicit false, [cs] accepted
├── test_config_docs.rs      # (existing) forces docs + example updates
├── test_card_pool.rs        # Gating behaviour
├── test_metrics_endpoint.rs # CS series absent, status gauge present
├── test_discovery.rs        # Role assignment with the path off
└── test_cs_disabled.rs      # NEW: end-to-end disabled-path integration

docs/configuration.md        # [cs] section + [modules] cross-reference
config.toml.example          # [cs] with the default
```

**Structure Decision**: Existing single-workspace layout is unchanged. One new source file (`control/disabled.rs`) and one new integration test file (`test_cs_disabled.rs`); everything else is an edit to a file that already owns the concern.

## Design Decisions

Full rationale in [research.md](./research.md). The load-bearing ones:

| Decision | Why |
|---|---|
| `RawCs` needs a **hand-written** `Default` returning `true` | The `section!` macro applies `#[serde(default)]`, so an omitted key falls back to `Default`. `#[derive(Default)]` on a `bool` yields `false` — which would silently disable circuit switching for every existing deployment on upgrade, the exact regression User Story 2 forbids. This is the single highest-risk line in the feature. |
| Gate at `commands/daemon.rs`, not inside `CardPool` | One choke point instead of a flag threaded through the pool's internals. Keeps `CardPool` unaware of the flag and keeps FR-021a working by construction (the CS metric statics are simply never touched). |
| Extend `owns_sip_side`, don't add a second suppression | FR-009a requires it, and `register()` already early-returns on `!owns_sip_side` before `register_trunk` is consulted — so one term covers both trunk registration and the host-side registrar. |
| Only VoWiFi's partition needs the `cs_enabled` parameter | `resolve_volte_lines` applies no audio-capability reservation; the reservation lives solely in `RoleAssignment::from_probed`. VoLTE needs no change. |
| CS metrics absence is free; the status gauge is not | `Lazy` statics register on first touch and all touch sites are in `CardPool`. The new `CS_ENABLED` gauge must be set unconditionally in `daemon.rs` so it is present in *both* states (FR-021b). |

## Risks

| Risk | Mitigation |
|---|---|
| `RawCs` default silently becomes `false`, disabling CS everywhere on upgrade | A dedicated test asserts a config with no `[cs]` section parses to `enabled == true`. Written first (TDD), and called out in the task list as the highest-priority test. |
| Control commands hang when the path is off | The responder task; integration test asserts a real `Err` reply, not a timeout. |
| `Observe` reports accidentally routed through the disabled responder, breaking VoWiFi metrics | `control::server::handle_connection` routes `Observe` straight to `metrics::ingest` and never to the pool mailbox. Test asserts VoWiFi metrics still land with the path off. |
| Docs drift fails the build late | `test_config_docs.rs` already enforces this; docs tasks are sequenced with the config change, not deferred to the end. |

## Post-Design Constitution Re-check

Re-evaluated after Phase 1. No new dependencies, no new abstraction layers, no mocks added. The single new construct (disabled-command responder) is justified above. Test strategy is integration-first throughout: every functional requirement maps to a test exercising real config parsing, real daemon wiring, a real metrics registry, or the real role-assignment function. **PASS** — no Complexity Tracking entries required.
