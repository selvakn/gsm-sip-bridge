# Implementation Plan: SIP Audio Echo Server

**Branch**: `002-sip-audio-echo` | **Date**: 2026-05-02 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `/specs/002-sip-audio-echo/spec.md`

## Summary

Build a SIP-based audio echo server that registers with a SIP server using credentials from a config.ini file, automatically answers incoming calls, and echoes the caller's audio back in real time using PJSIP's conference bridge. This is the VoIP counterpart to the existing GSM echo module.

## Technical Context

**Language/Version**: C++17 (GCC 9+)
**Primary Dependencies**: PJSIP/PJSUA2 (SIP + media), mINI (INI parsing, header-only, MIT)
**Storage**: N/A (configuration file only, no persistent state)
**Testing**: GTest (FetchContent, consistent with existing project), PJSIP loop transport for integration tests
**Target Platform**: Linux (Debian/Ubuntu)
**Project Type**: CLI daemon
**Performance Goals**: <200ms audio round-trip, <2s call answer, <5s registration
**Constraints**: Single call at a time, G.711 codec, null audio device (no local sound hardware)
**Scale/Scope**: One concurrent call, sequential call handling

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Status | Notes |
|-----------|--------|-------|
| I. Integration-First Testing | PASS | Tests use PJSIP loop transport + null audio for real SIP stack testing without mocking |
| II. Green-on-Commit | PASS | All tests must pass before each commit |
| III. Frequent Atomic Commits | PASS | Implementation broken into small phases per user story |
| IV. Makefile-Driven Build | PASS | New `run-sip` target; existing `build`, `test`, `clean`, `lint` targets extended |
| V. Simplicity & Refactorability | PASS | Flat source structure, no unnecessary abstraction layers, PJSUA2 high-level API |

**License gate**: PJSIP is GPL v2+. This violates the preferred license policy (Apache 2.0 / MIT). **Justified because**: (a) user explicitly specified PJSIP as a binding requirement, (b) this is an internal diagnostic tool not distributed commercially. mINI is MIT licensed (compliant).

## Project Structure

### Documentation (this feature)

```text
specs/002-sip-audio-echo/
├── plan.md
├── research.md
├── data-model.md
├── quickstart.md
├── contracts/
│   └── cli-interface.md
└── tasks.md
```

### Source Code (repository root)

```text
src/
├── logger.h                  # Shared (existing)
├── sip/
│   ├── main.cpp              # CLI, signal handling, PJSIP endpoint lifecycle
│   ├── sip_config.h          # SipConfig struct + INI parser
│   ├── sip_config.cpp
│   ├── echo_account.h        # pj::Account subclass (registration, incoming call)
│   ├── echo_account.cpp
│   ├── echo_call.h           # pj::Call subclass (call state, media loopback)
│   └── echo_call.cpp
├── ... (existing GSM echo files)

tests/integration/
├── test_sip_config.cpp       # Config parsing happy/sad paths
├── test_sip_echo.cpp         # Full SIP call lifecycle via loop transport
└── ... (existing GSM echo tests)

vendor/
└── mini/
    └── ini.h                 # mINI header-only library (MIT)
```

**Structure Decision**: SIP echo code lives under `src/sip/` to separate it from the existing GSM echo module. Shared utilities (`logger.h`) remain at the `src/` root. The `vendor/` directory holds the header-only mINI library. Both modules produce separate binaries (`audio-echo` for GSM, `sip-echo` for SIP).

## Complexity Tracking

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|-------------------------------------|
| GPL dependency (PJSIP) | User-specified binding constraint | No MIT/Apache SIP library offers comparable maturity and conference bridge |
