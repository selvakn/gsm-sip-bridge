# Tasks: Observability Metrics and Dashboard

## Task 1: Add prometheus-cpp dependency and metrics module

**Files**: `CMakeLists.txt`, `src/bridge/metrics.h`, `src/bridge/metrics.cpp`

1. Add prometheus-cpp v1.3.0 via FetchContent in CMakeLists.txt
2. Create `src/bridge/metrics.h` with all metric family declarations and init/shutdown functions
3. Create `src/bridge/metrics.cpp` with metric definitions and Exposer startup
4. Link prometheus-cpp to gsm-sip-bridge target

**Exit criteria**: Compiles, metrics module has all families registered, Exposer binds to port.

## Task 2: Instrument bridge components

**Files**: `src/bridge/main.cpp`, `src/bridge/card_instance.cpp`, `src/bridge/card_pool.cpp`, `src/bridge/bridge_account.cpp`

1. Initialize metrics in main.cpp before card discovery
2. Instrument CardInstance: call start/end, caller ID, ALSA errors, SIP dial outcomes, call duration
3. Instrument CardPool: module active/failed gauges, init/retry counters
4. Instrument BridgeAccount: SIP registration counters, registered gauge

**Exit criteria**: All key events increment/set appropriate metrics.

## Task 3: Write integration tests for metrics

**Files**: `tests/integration/test_metrics.cpp`, `CMakeLists.txt`

1. Test that metrics_init creates a working Exposer
2. Test that counters increment correctly
3. Test that gauges reflect values
4. Add test_metrics.cpp to bridge-tests target

**Exit criteria**: All tests pass via ctest.

## Task 4: Update Dockerfile for prometheus-cpp build

**Files**: `Dockerfile`

1. Add libcurl4-openssl-dev and zlib1g-dev to builder stage (prometheus-cpp pull deps)
2. Verify multi-stage build still produces working binary

**Exit criteria**: `docker build` succeeds.

## Task 5: Add Prometheus and Grafana to Docker Compose

**Files**: `docker-compose.yml`, `docker/prometheus.yml`, `docker/grafana/provisioning/datasources/prometheus.yml`, `docker/grafana/provisioning/dashboards/dashboard.yml`

1. Create Prometheus config with scrape job targeting gsm-sip-bridge:9091
2. Create Grafana provisioning for auto-configured datasource and dashboard provider
3. Add prometheus and grafana services to docker-compose.yml
4. Expose Prometheus on 9090, Grafana on 3000

**Exit criteria**: `docker compose up` brings up all three services with Grafana auto-provisioned.

## Task 6: Create pre-built Grafana dashboard

**Files**: `docker/grafana/dashboards/gsm-bridge.json`

1. Build dashboard JSON with panels for all metric families
2. Include: active modules, call volume, SIP registration, call duration histogram, error rates, module health
3. Use templated module_id variable for per-module filtering

**Exit criteria**: Dashboard loads automatically in Grafana with all panels rendering.

## Task 7: Update README and version

**Files**: `README.md`

1. Add Observability section documenting metrics endpoint, Prometheus, Grafana
2. Update Quick Start with `docker compose up` for full monitoring stack
3. Bump version to 3.0.0

**Exit criteria**: README accurately reflects new monitoring capabilities.
