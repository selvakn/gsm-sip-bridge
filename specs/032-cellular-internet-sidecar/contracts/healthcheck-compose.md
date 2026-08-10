# Contract: Healthcheck exit codes + Compose override wiring

## Healthcheck contract (`internet-healthcheck.sh`)

The sidecar's health is the gate the bridge waits on. Exit-code semantics:

| Exit | Meaning | Docker health | Bridge gate |
|------|---------|---------------|-------------|
| `0`  | The sidecar has dialed **and** the DNS probe resolved `INTERNET_PROBE_HOST` via `INTERNET_PROBE_RESOLVER` | healthy | released |
| `1`  | No session dialed yet, or the probe failed (no route / name did not resolve) | unhealthy | held |

Both conditions are required. Gating on the dial as well as the probe is what
makes this "reachable **through the cellular link**" (FR-003) rather than merely
"this host has internet from somewhere" — otherwise a box that still had another
uplink would report healthy and release the bridge before the sidecar had
brought the cellular session up at all.

- The probe is a DNS resolution (not ICMP, not HTTP) — see research R3.
- Timing (research R4): `start_period: 90s`, `interval: 10s`, `timeout: 5s`,
  `retries: 1`.
- The healthcheck MUST be side-effect-free apart from updating
  `/run/internet-status`; it MUST NOT itself attempt to dial or reconfigure.

## Compose override contract (`docker/docker-compose.cellular-internet.yml`)

Opt-in, default off (research R5). Enabling:

```bash
docker compose \
  -f docker/docker-compose.yml \
  -f docker/docker-compose.cellular-internet.yml \
  up -d
```

The override MUST:

1. **Add** the `internet` service:
   - `network_mode: host`, `privileged: true`, `restart: unless-stopped`
   - `/dev:/dev` (for `/dev/cdc-wdm0`)
   - its healthcheck (timings above)
   - env from `.env` (`INTERNET_APN`, probe host/resolver, …)
2. **Inject** onto the existing `gsm-sip-bridge` service (via Compose merge):
   ```yaml
   gsm-sip-bridge:
     depends_on:
       internet:
         condition: service_healthy
   ```

The base `docker/docker-compose.yml` MUST remain unchanged and dependency-free,
so that without this override:
- no `internet` container is created, and
- the bridge starts with no added wait (SC-003 — behavior identical to today).

## Non-goals

- The override MUST NOT add the sidecar to the base file or a default profile.
- The bridge image and its `config.toml` MUST NOT change as part of this wiring.
