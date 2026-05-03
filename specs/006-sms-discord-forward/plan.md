# Implementation Plan: SMS to Discord Forwarding

**Branch**: `006-sms-discord-forward` | **Date**: 2026-05-03 | **Spec**: [spec.md](spec.md)

## Summary

Add SMS reception monitoring to all connected EC20 modules. When an SMS arrives, persist it to a local SQLite database, forward it to a configured Discord webhook as a rich embed notification, and delete it from the SIM. Integrate with the existing Prometheus metrics and Grafana dashboard.

## Technical Context

**Language/Version**: C++17 (GCC 9+)
**Primary Dependencies**: cpp-httplib v0.41.0 (MIT, header-only HTTP client), SQLite3 (public domain), existing PJSIP/ALSA/prometheus-cpp stack
**Storage**: SQLite3 for SMS persistence (`sms.db`)
**Testing**: Google Test via CMake/CTest
**Constraints**: SMS handling must not block AT command polling loop; Discord POST must be asynchronous

## Constitution Check

| Principle | Status | Notes |
|-----------|--------|-------|
| I. Integration-First Testing | PASS | Test real SQLite operations, real HTTP payload formatting. Mock Discord endpoint only (external service). |
| II. Green-on-Commit | PASS | Each task produces a green commit. |
| III. Frequent Atomic Commits | PASS | 7 focused tasks. |
| IV. Makefile-Driven Build | PASS | Existing targets unchanged. |
| V. Simplicity | PASS | Two new modules (sms_handler, sms_store). No abstraction layers. Direct SQLite C API. |

## Project Structure

### Source Code

```text
src/bridge/
├── sms_store.h            # NEW: SQLite SMS persistence
├── sms_store.cpp          # NEW: DB open, insert, update status
├── sms_handler.h          # NEW: SMS URC parsing, Discord posting, orchestration
├── sms_handler.cpp        # NEW: AT command SMS flow, HTTP POST, queue
├── bridge_config.h        # MODIFIED: add SmsConfig
├── bridge_config.cpp      # MODIFIED: parse [sms] section
├── card_instance.h        # MODIFIED: add SmsHandler reference
├── card_instance.cpp      # MODIFIED: handle +CMTI URC in run_loop
├── main.cpp               # MODIFIED: init SMS handler, pass to CardPool
├── metrics.h              # MODIFIED: add SMS metrics
├── metrics.cpp            # MODIFIED: add SMS metrics

tests/integration/
├── test_sms_store.cpp     # NEW: SQLite CRUD tests
└── test_sms_handler.cpp   # NEW: SMS parsing and Discord payload tests
```
