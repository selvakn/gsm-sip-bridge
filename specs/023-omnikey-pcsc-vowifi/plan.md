# Implementation Plan: PC/SC Card-Reader-Backed VoWiFi Lines

**Branch**: `023-omnikey-pcsc-vowifi` | **Date**: 2026-07-27 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/023-omnikey-pcsc-vowifi/spec.md`

**Note**: This template is filled in by the `/speckit.plan` command. See `.specify/templates/plan-template.md` for the execution workflow.

## Summary

Add a second SIM source for a VoWiFi line: today every line is derived from a
USB-scanned cellular modem, whose SIM the `strongswan` tunnel engine reaches
either directly (`swu` engine, `AT+CSIM`) or via a virtual PC/SC reader bridged
to the modem (`vowifi-usim-bridge`). This feature lets a line instead be backed
by a **real, physically attached PC/SC smart-card reader** (validated hardware:
OmniKey AG 3x21, USB `076b:3031`) holding the SIM directly — no modem involved.
strongSwan's `eap-sim-pcsc` plugin already auto-detects any PC/SC reader pcscd
can see and matches it to a line by IMSI, so the only genuinely new engineering
is (a) making pcscd able to see a *real* USB reader (today it only has vpcd's
virtual-reader driver) and (b) creating an alternate, non-modem path through
this project's line-resolution/orchestration pipeline, since every existing
code path assumes a modem is present. Per spec clarifications, such a line
must (1) show up in existing status/metrics/alerting identically to a
modem-backed line, and (2) recover automatically once its reader/card is
reachable again, with no operator restart.

## Technical Context

**Language/Version**: Rust 1.94.0 (workspace `edition = "2021"`, pinned via `rust-toolchain.toml`)
**Primary Dependencies**: No new Rust crate dependencies expected — this is orchestration/config plumbing on top of already-vendored `strongSwan` (with the `eap-sim-pcsc` plugin, already built in `docker/Dockerfile`) and `pcscd`/PC-SC Lite (already installed); the new runtime dependency is Alpine's `ccid` package (generic USB CCID driver), added to the Docker image only.
**Storage**: N/A — line configuration lives in `config.toml`; per-run line resolution is the existing `LineResolution` JSON artifact (`gsm-sip-bridge/src/vowifi/discovery.rs`), unchanged in shape beyond one new field.
**Testing**: `cargo test --workspace` (unit tests extending `vowifi/discovery.rs`'s existing `resolve_lines_*` table-driven tests); per this project's constitution (Integration-First Testing), a mocked `CommandRunner` is used only because the physical OmniKey reader and a live carrier network are hardware/network dependencies impractical to run in CI — the same justification already applied throughout `supervise/orchestrate.rs`'s existing tests. Live verification against the real reader + SIM + carrier network is manual (documented in quickstart.md), matching how this project has always proven each VoWiFi phase (see `docs/vowifi-epdg-research-notes.md`'s Phase 1-5 live-hardware verification history).
**Target Platform**: Linux container (Alpine, the existing `docker/Dockerfile` image), `privileged: true` with full `/dev` passthrough (`docker-compose.yml`) — already sufficient for USB PC/SC reader access, no new passthrough needed.
**Project Type**: Single Rust workspace (CLI + supervised multi-process daemon) — matches this repo's existing structure, not a web/mobile app.
**Performance Goals**: None beyond parity with existing modem-backed line startup/registration latency — this is a one-time-per-line setup path, not a hot path.
**Constraints**: No SIM PIN/CHV1 handling (spec Assumption — out of scope; user's SIM has PIN disabled). Only the `strongswan` tunnel engine gains PC/SC-reader support; the `swu` engine has no PC/SC support at all and must reject the combination (spec FR-008). No change to existing modem-backed line behavior or configuration (spec FR-005, SC-004).
**Scale/Scope**: One new SIM-source variant for the existing per-line model; card-reader lines share the existing `[vowifi].max_lines` bound with modem lines (spec FR-006) — no new capacity dimension.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

- **I. Integration-First Testing (NON-NEGOTIABLE)**: New `resolve_lines`/orchestration logic is tested through the same integration-style harness already used (`MockCommandRunner`/`RealCommandRunner` trait, table-driven `resolve_lines_*` tests) — no new mocking beyond what's already standard here. The one exception this project's constitution explicitly allows — "hardware not available in CI" — covers the physical OmniKey reader and live carrier network; live proof is manual (quickstart.md), exactly as every prior VoWiFi phase in this repo was proven (docs/vowifi-epdg-research-notes.md). **PASS**.
- **II. Green-on-Commit**: `cargo fmt --all && make lint && cargo test --workspace` before every commit, per this repo's CLAUDE.md pre-commit checklist (equivalent to this constitution's `make test`/`make lint` gate). **PASS** (to be enforced during implementation).
- **III. Frequent Atomic Commits**: Plan below is decomposed into independently committable steps (config → line resolution → orchestration → Docker image → docs), matching the constitution's "single logical change" requirement. **PASS**.
- **IV. Makefile-Driven Build**: No change — this repo's existing `Makefile`/`make lint`/`make test` targets already cover this workspace; no new build entry points needed. **PASS**.
- **V. Simplicity & Refactorability**: Deliberately scoped to a minimal "second SIM source" branch through the existing pipeline (one new bool field, one new resolution function, two skip-branches in orchestration) rather than a general pluggable-SIM-backend abstraction — YAGNI, since only one non-modem source is needed today. **PASS**.

No violations requiring Complexity Tracking justification.

## Project Structure

### Documentation (this feature)

```text
specs/023-omnikey-pcsc-vowifi/
├── plan.md              # This file (/speckit.plan command output)
├── research.md          # Phase 0 output (/speckit.plan command)
├── data-model.md        # Phase 1 output (/speckit.plan command)
├── quickstart.md        # Phase 1 output (/speckit.plan command)
├── contracts/           # Phase 1 output (/speckit.plan command)
└── tasks.md             # Phase 2 output (/speckit.tasks command - NOT created by /speckit.plan)
```

### Source Code (repository root)

```text
gsm-sip-bridge/src/
├── config/
│   └── mod.rs                  # VowifiLineOverride: + pcsc_reader field, validation
├── vowifi/
│   ├── discovery.rs             # ResolvedLine/LineResolutionEntry: + pcsc_reader field;
│   │                            #   resolve_lines: append pcsc-derived lines
│   └── usim_bridge.rs           # unchanged — only used for modem-backed lines
└── supervise/
    └── orchestrate.rs           # start_vowifi_line / start_vowifi_line_strongswan:
                                 #   skip modem-only steps for pcsc_reader lines;
                                 #   fail fast if swu engine + pcsc_reader line

docker/
├── Dockerfile                   # + `ccid` package (generic USB CCID driver)
└── strongswan/*                 # unchanged — eap-sim-pcsc config is already generic

config.toml.example              # + documented `pcsc_reader = true` line example
docs/
└── omnikey-pcsc-vowifi.md       # new — setup/verification guide (this doc, not code)
```

**Structure Decision**: Single Rust workspace, existing layout — this feature is
entirely additive within `gsm-sip-bridge/src/{config,vowifi,supervise}` plus one
Docker image change and docs; no new crates, services, or top-level directories.

## Complexity Tracking

*No Constitution Check violations — table omitted.*
