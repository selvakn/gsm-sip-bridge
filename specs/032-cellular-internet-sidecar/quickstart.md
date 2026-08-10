# Quickstart: Same-card internet + VoWiFi via the internet sidecar

Enable one EC20 SIM to carry **both** internet and VoWiFi calls, with the bridge
gated on a genuinely-reachable uplink.

## Prerequisites

- An EC20 (Qualcomm/QMI) modem with a SIM provisioned for **data** (internet APN)
  and for VoWiFi calling.
- The modem exposes `/dev/cdc-wdm0` (QMI) and `/dev/ttyUSB*` (AT). The sidecar
  uses only QMI; the bridge uses only AT for `AT+CSIM`.
- Docker + Compose plugin.

## 1. Configure the sidecar

In `docker/.env` (see `.env.example`):

```dotenv
INTERNET_APN=airtelgprs.com          # your carrier's INTERNET apn (not IMS)
INTERNET_QMI_DEV=/dev/cdc-wdm0
INTERNET_PROBE_HOST=one.one.one.one  # override if your carrier blocks it
INTERNET_PROBE_RESOLVER=1.1.1.1
```

Configure the bridge for VoWiFi as usual (see `sample_configs/ec20-internet-plus-vowifi.toml`
and `docs/ec20-internet-plus-vowifi.md`). No internet settings go in the bridge
`config.toml` — internet is the sidecar's job.

## 2. Bring the stack up WITH the override (opt-in)

```bash
docker compose \
  -f docker/docker-compose.yml \
  -f docker/docker-compose.cellular-internet.yml \
  up -d
```

Expected ordering:
1. `internet` dials QMI, gets an IPv4, installs the default route.
2. Its healthcheck starts probing DNS; within ~90s it goes **healthy**.
3. Only then does `gsm-sip-bridge` start (its `depends_on: service_healthy` gate).

## 3. Verify internet is up (sidecar)

```bash
docker compose ... ps            # `internet` shows (healthy)
docker exec <internet-ctr> cat /run/internet-status
#   state=up iface=wwan0 ipv4=... probe="ok one.one.one.one@1.1.1.1" ...
docker logs <internet-ctr> | tail
```

Confirm the AT port is NOT held by the sidecar (bridge needs it):

```bash
docker exec <internet-ctr> sh -c 'command -v fuser && fuser /dev/ttyUSB2 || echo "AT free"'
```

## 4. Verify the gate + both-at-once (Story 1 / SC-005)

```bash
# Bridge only started after internet was healthy:
docker inspect -f '{{.State.StartedAt}}' <bridge-ctr>   # later than internet healthy

# Internet still flowing:
docker exec <internet-ctr> nslookup one.one.one.one 1.1.1.1

# VoWiFi registered over the same card:
vowifi-status        # or the bridge's status port → registered: true
```

Place a live inbound/outbound VoWiFi call while `nslookup`/traffic succeeds —
both work on the one card at once.

## 5. Independent management (Story 2)

```bash
# Restart ONLY the sidecar; internet recovers without touching the bridge:
docker compose ... restart internet
docker exec <internet-ctr> cat /run/internet-status   # returns to state=up
```

## 6. Confirm default-off for other deployments (Story 3 / SC-003)

Bring the stack up **without** the override:

```bash
docker compose -f docker/docker-compose.yml up -d
docker compose -f docker/docker-compose.yml ps        # no `internet` container
```

The bridge starts with no added wait — identical to prior releases.

## Failure signatures

- Sidecar stuck **unhealthy**, bridge never starts → check `/run/internet-status`
  and `docker logs <internet-ctr>`: no signal, SIM not data-provisioned, wrong
  APN, or the probe target unreachable (try another `INTERNET_PROBE_RESOLVER`).
- Sidecar exits immediately with a config error → `INTERNET_APN` unset, or
  `/dev/cdc-wdm0` missing / not a QMI modem (non-QMI modems are out of scope).
