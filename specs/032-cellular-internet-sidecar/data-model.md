# Phase 1 Data Model: Cellular-internet sidecar

This feature has no persistent data store. The "entities" are the sidecar's
**configuration surface** (env vars) and its **runtime status**.

## Entity: Sidecar configuration (environment variables)

Operator-set on the `internet` service (via `.env` / the override file). Separate
from the bridge's `config.toml` (FR-011).

| Name | Meaning | Default | Notes |
|------|---------|---------|-------|
| `INTERNET_APN` | Internet/data APN to dial | *(required)* | Carrier internet APN (e.g. `airtelgprs.com`); NOT an IMS APN |
| `INTERNET_QMI_DEV` | QMI control device | `/dev/cdc-wdm0` | Must be the modem's QMI node; keeps AT port free |
| `INTERNET_WWAN_IFACE` | Host netdev bound to the data session | *(auto-detect)* | e.g. `wwan0`; auto-derived from the QMI dev when unset |
| `INTERNET_IP_FAMILY` | Requested PDP type | `IPV4V6` | IPv4 is what gates health; IPv6 best-effort |
| `INTERNET_PROBE_HOST` | Hostname the DNS probe resolves | `one.one.one.one` | Operator-configurable (FR-011) |
| `INTERNET_PROBE_RESOLVER` | Resolver queried | `1.1.1.1` | Fallback `8.8.8.8`; override for locked-down carriers |
| `INTERNET_PROBE_INTERVAL` | Recurring probe cadence | `10s` | Maps to healthcheck `interval` |
| `INTERNET_ATTACH_GRACE` | First-connect grace | `90s` | Maps to healthcheck `start_period` |

**Validation rules**:
- `INTERNET_APN` MUST be set; the entrypoint fails fast with a clear message if
  empty (do not silently dial a default).
- `INTERNET_QMI_DEV` MUST exist and be a QMI (`cdc-wdm`) node; if absent or not
  QMI-capable, fail fast (FR-010) rather than fall back to AT.
- `INTERNET_PROBE_HOST` / `INTERNET_PROBE_RESOLVER` MUST be non-empty.

## Entity: Internet-readiness status (runtime)

Written by the entrypoint/healthcheck to `/run/internet-status`; also emitted to
logs. Sidecar-local only (FR-008, no external export).

| Field | Meaning | Example |
|-------|---------|---------|
| `state` | Coarse lifecycle state | `dialing` \| `up` \| `probe-fail` \| `down` \| `redialing` |
| `iface` | Bound netdev | `wwan0` |
| `ipv4` | Assigned IPv4 (if any) | `100.72.13.4` |
| `probe` | Last probe result + target | `ok one.one.one.one@1.1.1.1` |
| `last_change` | Timestamp of last state change | `2026-08-10T13:40:02Z` |
| `since` | When the current session came up | `2026-08-10T13:38:41Z` |

**State transitions**:

```text
(start) → dialing ──success──→ up ──probe ok──→ up
   ▲          │                 │
   │          └──dial fail──────┤ (retry, stays not-healthy)
   │                            │
   │                    probe fail│
   │                            ▼
   └──── redialing ←── down ←── probe-fail  (link/session lost)

Health = healthy  IFF  state == up AND last probe == ok
       = unhealthy otherwise (dialing / probe-fail / down / redialing)
```

The Docker healthcheck maps `up + probe ok` → exit 0 (healthy); everything else →
exit 1 (unhealthy), which holds the bridge's `depends_on: service_healthy` gate.

## Relationship to the bridge

- The bridge is a **dependent consumer**: in same-card mode its container start is
  gated on this status reaching healthy. There is no data exchanged between the
  two — only Docker's health state via `depends_on`.
- No shared volume is required for correctness (status is sidecar-local); the
  status file lives in the sidecar's own `/run`.
