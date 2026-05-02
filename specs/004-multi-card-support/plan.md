# Implementation Plan: Multi EC20 Card Support

**Branch**: `004-multi-card-support` | **Date**: 2026-05-02 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `/specs/004-multi-card-support/spec.md`

## Summary

Extend the GSM-SIP bridge to detect and operate multiple Quectel EC20 USB modules simultaneously. Each module runs its own call-handling thread with independent AT command polling and ALSA audio path, while sharing a single PJSIP endpoint and SIP account registration. A CardPool coordinates lifecycle management including startup discovery, per-card initialization, background retry of failed modules, and graceful multi-card shutdown.

## Technical Context

**Language/Version**: C++17 (GCC 9+)
**Primary Dependencies**: PJSIP/PJSUA2 (SIP + media), libasound2 (ALSA), mINI (INI parsing, header-only, MIT), Google Test
**Storage**: N/A (stateless runtime, config.ini read-only at startup)
**Testing**: Google Test via CMake/CTest, integration-first per constitution
**Target Platform**: Linux (x86_64/ARM) with ALSA and USB support
**Project Type**: CLI / embedded service
**Performance Goals**: <300ms end-to-end voice latency per bridge, <10s boot detection for all modules
**Constraints**: One voice call per EC20 module at a time, shared single SIP registration
**Scale/Scope**: 2-8 concurrent EC20 modules on one host

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Status | Notes |
|-----------|--------|-------|
| I. Integration-First Testing | PASS | Tests exercise real sysfs scanning. Hardware-dependent tests use GTEST_SKIP when EC20 not connected. No new mocks introduced. |
| II. Green-on-Commit | PASS | Each task produces a green commit via `make test`. |
| III. Frequent Atomic Commits | PASS | Work broken into 7 focused tasks, each a single logical change. |
| IV. Makefile-Driven Build | PASS | Existing Makefile targets (`build`, `test`, `run`, `clean`, `lint`) unchanged. No new targets needed. |
| V. Simplicity & Refactorability | PASS | CardInstance wraps existing per-card logic (extract, don't abstract). CardPool is a flat container. No design patterns beyond what the problem requires. |

## Project Structure

### Documentation (this feature)

```text
specs/004-multi-card-support/
├── plan.md
├── research.md
├── data-model.md
├── quickstart.md
└── tasks.md
```

### Source Code (repository root)

```text
src/
├── device_discovery.h         # MODIFIED: DeviceInfo gains serial_number, add discover_all_ec20()
├── device_discovery.cpp       # MODIFIED: multi-device discovery + serial number extraction
├── bridge/
│   ├── main.cpp               # MODIFIED: use CardPool, per-card threads
│   ├── card_instance.h        # NEW: per-module encapsulation
│   ├── card_instance.cpp      # NEW: per-module thread + call handling
│   ├── card_pool.h            # NEW: multi-card lifecycle management
│   ├── card_pool.cpp          # NEW: discovery, init, retry, shutdown
│   ├── bridge_account.h       # MODIFIED: support multiple concurrent calls
│   ├── bridge_account.cpp     # MODIFIED: thread-safe multi-call tracking
│   ├── bridge_config.h        # UNCHANGED
│   ├── bridge_config.cpp      # UNCHANGED
│   ├── bridge_call.h          # UNCHANGED
│   ├── bridge_call.cpp        # UNCHANGED
│   ├── alsa_media_port.h      # UNCHANGED
│   ├── alsa_media_port.cpp    # UNCHANGED
│   └── beep_generator.h/cpp   # UNCHANGED
├── logger.h                   # UNCHANGED (card prefix via format string)
├── serial_port.h/cpp          # UNCHANGED
├── at_commander.h/cpp         # UNCHANGED
└── ring_buffer.h              # UNCHANGED

tests/integration/
├── test_device_discovery.cpp  # MODIFIED: add multi-device tests
├── test_card_pool.cpp         # NEW: pool lifecycle tests
└── ...                        # UNCHANGED
```

**Structure Decision**: Follows existing flat `src/bridge/` layout. Two new files (card_instance, card_pool) added to `src/bridge/`. No new directories or layers introduced.

## Complexity Tracking

No constitution violations. No complexity justification needed.
