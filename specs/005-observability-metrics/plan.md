# Implementation Plan: Observability Metrics and Dashboard

**Branch**: `005-observability-metrics` | **Date**: 2026-05-02 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `/specs/005-observability-metrics/spec.md`

## Summary

Add Prometheus-compatible metrics exposition to the GSM-SIP bridge, covering all call lifecycle events, SIP registration, module health, and error tracking. Include a Docker Compose stack with Prometheus (collection) and Grafana (visualization) with a pre-provisioned dashboard ready out of the box.

## Technical Context

**Language/Version**: C++17 (GCC 9+)
**Primary Dependencies**: prometheus-cpp (MIT, Prometheus client for C++), PJSIP/PJSUA2, libasound2, mINI
**Storage**: N/A (Prometheus handles metric storage)
**Testing**: Google Test via CMake/CTest, integration-first per constitution
**Target Platform**: Linux (x86_64/ARM) with Docker
**Project Type**: CLI / embedded service with monitoring sidecar
**Performance Goals**: <1ms overhead per metric event
**Constraints**: Metrics must be fire-and-forget; never block call handling
**Scale/Scope**: 2-8 modules, ~20 metric series, Prometheus scrape interval 5s

## Constitution Check

| Principle | Status | Notes |
|-----------|--------|-------|
| I. Integration-First Testing | PASS | Test real /metrics HTTP endpoint returns valid Prometheus exposition format. |
| II. Green-on-Commit | PASS | Each task produces a green commit. |
| III. Frequent Atomic Commits | PASS | 6 focused tasks. |
| IV. Makefile-Driven Build | PASS | Existing targets unchanged. `make docker` covers full stack. |
| V. Simplicity & Refactorability | PASS | Single metrics module with free functions. No abstraction layers. prometheus-cpp handles serialization and HTTP. |

## Project Structure

### Documentation (this feature)

```text
specs/005-observability-metrics/
├── plan.md
├── research.md
├── data-model.md
├── quickstart.md
└── tasks.md
```

### Source Code (repository root)

```text
src/bridge/
├── metrics.h              # NEW: metric declarations and init
├── metrics.cpp            # NEW: metric definitions, exposer startup
├── main.cpp               # MODIFIED: start metrics, pass to components
├── card_instance.cpp      # MODIFIED: instrument call lifecycle
├── card_pool.cpp          # MODIFIED: instrument module gauges
├── bridge_account.cpp     # MODIFIED: instrument SIP registration
└── ...                    # UNCHANGED

docker/
├── prometheus.yml         # NEW: Prometheus scrape config
└── grafana/
    ├── provisioning/
    │   ├── datasources/
    │   │   └── prometheus.yml   # NEW: auto-provision Prometheus datasource
    │   └── dashboards/
    │       └── dashboard.yml    # NEW: auto-provision dashboard provider
    └── dashboards/
        └── gsm-bridge.json      # NEW: pre-built Grafana dashboard

docker-compose.yml         # MODIFIED: add prometheus + grafana services
Dockerfile                 # MODIFIED: add prometheus-cpp build dependency
CMakeLists.txt             # MODIFIED: add prometheus-cpp via FetchContent

tests/integration/
└── test_metrics.cpp       # NEW: metrics endpoint and increment tests
```

**Structure Decision**: New `docker/` directory for monitoring stack configuration. Single `metrics.h/cpp` in existing `src/bridge/` -- no new layers.

## Complexity Tracking

No constitution violations. No complexity justification needed.
