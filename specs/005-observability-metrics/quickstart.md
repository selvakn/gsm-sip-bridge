# Quick Start: Observability Metrics and Dashboard

## Prerequisites

- Docker and Docker Compose installed
- GSM-SIP bridge built (or use Docker build)

## Start Full Stack

```bash
docker compose up -d --build
```

This starts three services:
1. **gsm-sip-bridge**: The bridge application exposing metrics on port 9091
2. **prometheus**: Scrapes metrics every 5 seconds, available at http://localhost:9090
3. **grafana**: Pre-configured dashboard, available at http://localhost:3000

## Access the Dashboard

1. Open http://localhost:3000
2. Login: `admin` / `admin` (skip password change prompt)
3. Navigate to Dashboards > GSM-SIP Bridge

The dashboard is auto-provisioned with panels for:
- Active modules and call volume
- SIP registration status
- Call duration distribution
- Module health and retry counts
- Error rates

## Verify Metrics Endpoint

```bash
curl http://localhost:9091/metrics
```

Expected output includes lines like:
```text
# HELP gsm_bridge_calls_total Total GSM calls
# TYPE gsm_bridge_calls_total counter
gsm_bridge_modules_active 2
gsm_bridge_sip_registered 1
```

## Configuration

| Environment Variable | Default | Description |
|---|---|---|
| `METRICS_PORT` | `9091` | Port for metrics HTTP server |

## Local Development (without Docker)

The metrics endpoint starts automatically with the bridge. Access at `http://localhost:9091/metrics`.

For Prometheus + Grafana without Docker, point a local Prometheus at the bridge's metrics port and configure Grafana to use that Prometheus instance as a datasource.
