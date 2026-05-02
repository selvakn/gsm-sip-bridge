# Research: Observability Metrics and Dashboard

## R1: C++ Prometheus Client Library

**Decision**: prometheus-cpp (jupp0r/prometheus-cpp)
**Rationale**:
- MIT license (corporate-friendly, allows patents)
- Mature and well-maintained (5k+ GitHub stars)
- Thread-safe registry and metric families
- Built-in HTTP server (`Exposer`) for /metrics endpoint
- Supports Counter, Gauge, Histogram, Summary
- CMake-native with FetchContent support
- C++11 minimum, fully compatible with C++17

**Alternatives considered**:
- OpenTelemetry C++ SDK: Heavier weight, requires separate collector, more complex setup for a simple metrics use case.
- Custom /metrics endpoint: Reinventing the wheel; prometheus text format is well-specified but tedious to implement correctly.
- StatsD: Requires separate daemon, UDP-based (lossy), less precise for histogram data.

**License chain**: prometheus-cpp (MIT) -> civetweb (MIT) for HTTP server. No restrictive dependencies.

## R2: Metrics Exposition Strategy

**Decision**: Pull-based (Prometheus scrapes the bridge)
**Rationale**:
- Bridge runs a lightweight HTTP server on port 9091 (configurable via `METRICS_PORT` env var)
- Prometheus scrapes at 5-second intervals
- No push gateway needed; simplifies architecture
- Bridge lifecycle is long-running, making pull suitable

**Alternatives considered**:
- Push gateway: Adds another component, suited for batch jobs not long-running services
- OpenTelemetry Collector: Over-engineered for a single-service monitoring setup

## R3: Visualization Stack

**Decision**: Grafana with file-based provisioning
**Rationale**:
- Grafana OSS (AGPL-3.0 for the server, but used as a tool, not embedded -- no licensing concern for internal use)
- File-based provisioning: datasource and dashboard loaded on first boot, zero manual setup
- Pre-built JSON dashboard with all panels configured
- Standard Prometheus datasource integration

**Alternatives considered**:
- Prometheus built-in UI: Too basic for meaningful dashboards
- Chronograf: Tied to InfluxDB ecosystem
- Console templates: Prometheus-native but limited flexibility

## R4: prometheus-cpp Integration Method

**Decision**: CMake FetchContent
**Rationale**:
- Consistent with existing GoogleTest integration pattern
- No system-level package dependency
- Pinned version for reproducible builds
- Downloads and builds as part of the CMake configure step

**Alternatives considered**:
- System package (libprometheus-cpp-dev): Not available in Debian bookworm
- Git submodule in vendor/: Works but FetchContent is cleaner for CMake projects
- Conan/vcpkg: Adds package manager dependency
