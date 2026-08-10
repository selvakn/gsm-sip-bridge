# One EC20 card: internet **and** VoWiFi calls

**Use case:** a site with no wired/Wi-Fi uplink, where a single EC20 + SIM must
carry both general internet and inbound/outbound calls.

This works because **VoWiFi and internet are decoupled**: the bridge uses the
modem only as a SIM/APDU reader (`AT+CSIM`), and its ePDG tunnel rides the host's
ordinary default route — it never touches the modem's data bearer. So the modem's
data path is free to carry internet at the same time.

Internet is brought up by a small, **opt-in sidecar container**
(specs/032-cellular-internet-sidecar) over the modem's **QMI** data path
(`/dev/cdc-wdm0`). QMI-only means the modem's **AT port stays free** for the
bridge's `AT+CSIM` — zero contention. The bridge is **gated** on the sidecar: it
does not start until a DNS probe proves the internet is actually reachable.

> **VoWiFi only.** This shares a card because VoWiFi treats the modem as a card
> reader. The bridge's **VoLTE** path instead seizes the modem's single data
> path and cannot coexist with internet on one card. VoWiFi and VoLTE are also
> mutually exclusive on one SIM. See
> [`ec20-volte-setup.md`](ec20-volte-setup.md).

## Architecture

```text
 ┌───────────────────────── host (docker --network host) ─────────────────────┐
 │                                                                             │
 │  EC20 modem                                                                 │
 │   ├─ /dev/cdc-wdm0 (QMI)  ─── internet sidecar ──► default route (internet) │
 │   └─ /dev/ttyUSB2  (AT)   ─── gsm-sip-bridge ────► AT+CSIM (SIM auth only)   │
 │                                        │                                    │
 │  gsm-sip-bridge: ePDG tunnel + IMS ────┴──► rides the host default route ───┼─► carrier ePDG
 │  (starts only after the sidecar is healthy)                                 │
 └─────────────────────────────────────────────────────────────────────────────┘
```

## Prerequisites

- EC20 (Qualcomm/QMI) modem exposing `/dev/cdc-wdm0` and `/dev/ttyUSB*`.
- A SIM provisioned for **data** (internet APN) **and** for VoWiFi calling.
- Docker + the Compose plugin.

## 1. Configure

**Bridge** (`config.toml`): a plain VoWiFi config — start from
[`sample_configs/ec20-internet-plus-vowifi.toml`](../sample_configs/ec20-internet-plus-vowifi.toml).
Nothing about internet goes here.

**Sidecar** (`docker/.env`, see [`.env.example`](../.env.example)):

```dotenv
INTERNET_APN=airtelgprs.com          # your carrier's INTERNET apn (not IMS)
INTERNET_QMI_DEV=/dev/cdc-wdm0
INTERNET_PROBE_HOST=one.one.one.one  # override if your carrier blocks it
INTERNET_PROBE_RESOLVER=1.1.1.1
```

## 2. Start the stack WITH the overlay (opt-in)

```bash
docker compose \
  -f docker/docker-compose.yml \
  -f docker/docker-compose.cellular-internet.yml \
  up -d
```

Startup order:
1. `internet` dials QMI, gets an IPv4, installs the default route.
2. Its healthcheck probes DNS; within ~90s it goes **healthy** (grace 90s, probe
   every 10s).
3. Only then does `gsm-sip-bridge` start — its `depends_on: service_healthy` gate.

Deployments **without** same-card internet simply omit the overlay: no sidecar
runs and the bridge starts with no added wait (identical to prior releases).

## 3. Verify

```bash
# Sidecar healthy + status:
docker compose ... ps                       # `internet` shows (healthy)
docker exec <internet-ctr> cat /run/internet-status
#   state=up iface=wwan0 ipv4=... probe=ok one.one.one.one@1.1.1.1 ...

# AT port is NOT held by the sidecar (bridge needs it):
docker exec <internet-ctr> sh -c 'fuser /dev/ttyUSB2 2>/dev/null || echo "AT free"'

# Bridge only started after internet was healthy:
docker inspect -f '{{.State.StartedAt}}' <bridge-ctr>

# Both at once: internet flows AND a live VoWiFi call connects on the one card.
docker exec <internet-ctr> nslookup one.one.one.one 1.1.1.1
vowifi-status                               # registered: true
```

See [`specs/032-cellular-internet-sidecar/quickstart.md`](../specs/032-cellular-internet-sidecar/quickstart.md)
for the full validation (independent restart, drop recovery, default-off).

## Troubleshooting

- **Sidecar stuck unhealthy, bridge never starts** — read
  `docker exec <internet-ctr> cat /run/internet-status` and `docker logs
  <internet-ctr>`: no signal, SIM not data-provisioned, wrong `INTERNET_APN`, or
  the probe target unreachable (try another `INTERNET_PROBE_RESOLVER`).
- **Sidecar exits immediately** — `INTERNET_APN` unset, or `/dev/cdc-wdm0`
  missing / not a QMI modem (non-QMI modems are out of scope; the sidecar never
  falls back to the AT port).
- **Internet up but VoWiFi won't register** — the tunnel rides the host default
  route the sidecar installed; confirm `ip route` shows a default via the wwan
  interface, then see [`vowifi-bridge.md`](vowifi-bridge.md).

## Why VoWiFi (not VoLTE) for the shared card

VoWiFi's ePDG tunnel is independent of the internet APN and sidesteps the LTE
IMS-PDN quirks (RA/`addr_gen_mode`) that the VoLTE path handles in
`src/volte/netcfg.rs`. It also leaves the modem's single data path entirely to
internet. This matches production experience that VoWiFi is more stable than
VoLTE on Airtel/Vodafone.
