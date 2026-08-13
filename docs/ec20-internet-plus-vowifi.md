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

Start **both services together from a clean state**. The ePDG tunnel runs with
`mobike = no` (the carrier's ePDG misbehaves with MOBIKE), which means an
already-established tunnel *cannot* migrate to a new source address — so
switching the default route underneath a running bridge will strand its tunnel.
Bring the uplink up first and let the bridge bind to it, which is exactly what
the readiness gate does for you.

The sidecar image is published alongside the bridge, so it can be pulled instead
of built:

```bash
docker compose -f docker/docker-compose.yml \
  -f docker/docker-compose.cellular-internet.yml pull internet
```

Pin a version or use your own registry with `INTERNET_IMAGE`, e.g.
`INTERNET_IMAGE=ghcr.io/selvakn/gsm-sip-bridge-internet:8.9.1`.

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

## IPv6 reach-back (dual-stack)

Carrier IPv4 is usually CGNAT, so the host has no inbound reachability over IPv4.
When the carrier grants IPv6, the sidecar also brings up a **global IPv6 address +
default route** on the WWAN interface, making this host reachable inbound (e.g. SSH)
over IPv6. Dual-stack is **ON by default**; IPv4/VoWiFi is unchanged and stays the
health-gating uplink, so a v6 problem never blocks calls or the bridge.

Dual-stack is a **single IPv4v6 bearer**, not two sessions: modern carriers
(verified on Jio) refuse a second connection to the same APN
(`multiple-connection-to-same-pdn-not-allowed`), and QMI has no per-call "dual"
ip-type. So when v6 is enabled the sidecar provisions the data profile
(`INTERNET_IPV6_PROFILE`, default 1) as `pdp-type=IPv4v6` and dials one bearer,
reading both address families from it.

```bash
# One-off capability check (sidecar stopped so the QMI node is free). Jio grants a
# global v6 on an ip-type=6 session — a global address starts 2xxx:/3xxx:, not
# fe80:/fc00:/fd00:
qmicli -d /dev/cdc-wdm0 -p --wds-start-network="ip-type=6,apn=$INTERNET_APN" \
       --client-no-release-cid
qmicli -d /dev/cdc-wdm0 -p --wds-get-current-settings | grep -i 'IPv6'
# The sidecar itself instead provisions the profile IPv4v6 and dials ONE bearer:
qmicli -d /dev/cdc-wdm0 -p \
  --wds-modify-profile="3gpp,1,pdp-type=ipv4v6,apn=$INTERNET_APN"
qmicli -d /dev/cdc-wdm0 -p --wds-start-network="profile-index=1" --client-no-release-cid
qmicli -d /dev/cdc-wdm0 -p --wds-get-current-settings   # expect IPv4 AND IPv6

# Verify once enabled:
ip -6 addr show dev wwan0 | grep 'scope global'   # global v6 address present
ip -6 route show default                          # default v6 route present
docker exec <internet-ctr> cat /run/internet-status
#   ipv6=2409:...   ipv6_state=up
ssh user@2409:...                                  # reach the host from outside
```

Keep the current address discoverable with the change hook — point
`INTERNET_IPV6_HOOK` at a script run as `hook <new-addr>` on every appear/change
(wire up your own DDNS). The hook does **not** fire on loss, so give your AAAA
record a short TTL. The sidecar installs no firewall and no forwarding: the global
v6 address lands on the **host** (host-network mode), so you own the host firewall —
allow inbound SSH over IPv6. If the address is up but unreachable from outside, your
carrier may filter inbound v6 at its edge (outside the sidecar's control).

> Hardware status: on Jio, `ip-type=6` returns a global address and the parser's
> `IPv6 address: <addr>/<prefix>` format is confirmed correct. What still needs a
> live run is the single-bearer provisioning path above (`--wds-modify-profile`
> IPv4v6 → `profile-index` dial) actually yielding **both** families in one bearer —
> if your modem's internet profile isn't index 1, set `INTERNET_IPV6_PROFILE`.

See [`specs/035-dual-stack-ipv6/quickstart.md`](../specs/035-dual-stack-ipv6/quickstart.md)
for the full walkthrough.

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
