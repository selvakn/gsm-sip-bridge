# Quickstart: Dual-Stack IPv6 for the Cellular-Internet Sidecar

Audience: operator enabling inbound IPv6 reach-back on a host whose only uplink is
the cellular card (CGNAT IPv4). Assumes the feature-032 sidecar is already working
for IPv4.

## 1. Confirm the carrier actually grants IPv6 (one-off, on hardware)

Before enabling, verify the modem+carrier hand out a global IPv6 address:

```sh
# On the host, with the sidecar temporarily stopped (so the QMI node is free).
# Capability check — does the carrier grant a global v6 at all?
qmicli -d /dev/cdc-wdm0 -p \
  --wds-start-network="ip-type=6,apn=$YOUR_APN" --client-no-release-cid
qmicli -d /dev/cdc-wdm0 -p --wds-get-current-settings | grep -i 'IPv6'
```

- If you see an `IPv6 address:` line with a **global** address (starts `2xxx:`/`3xxx:`,
  not `fe80:` or `fc00:`/`fd00:`), the carrier grants IPv6.
- The sidecar itself dials ONE IPv4v6 bearer (not a separate v6 session — carriers
  like Jio refuse that). To mimic it: `--wds-modify-profile="3gpp,1,pdp-type=ipv4v6,apn=$YOUR_APN"`
  then `--wds-start-network="profile-index=1"` and check `--wds-get-current-settings`
  shows **both** IPv4 and IPv6.
- If no v6 address appears at all, the carrier/plan does not offer IPv6; the sidecar
  stays IPv4-only and healthy (nothing else to do).

## 2. Enable dual-stack in the sidecar

In `docker/.env` (see `.env.example`):

```sh
INTERNET_APN=your.carrier.apn      # required (unchanged)
INTERNET_ENABLE_IPV6=1             # default; dual-stack on
INTERNET_IPV6_PROFILE=1            # profile provisioned IPv4v6 + dialed (default 1)
# Optional: notify your own tooling when the v6 address changes
INTERNET_IPV6_HOOK=/opt/ddns/update-aaaa.sh
INTERNET_IPV6_HOOK_TIMEOUT=10s
```

Bring the stack up as usual:

```sh
docker compose \
  -f docker/docker-compose.yml \
  -f docker/docker-compose.cellular-internet.yml \
  up -d
```

## 3. Verify

```sh
# Global v6 address + default route on the wwan interface:
ip -6 addr show dev wwan0 | grep 'scope global'
ip -6 route show default

# Sidecar status shows the reach-back address:
docker compose ... exec internet cat /run/internet-status
#   ipv6=2401:4900:...:1
#   ipv6_state=up

# From an external host on the IPv6 internet:
ssh user@2401:4900:...:1
```

IPv4 and VoWiFi behavior is unchanged — the container is healthy as soon as IPv4 is
reachable, whether or not IPv6 came up.

## 4. The address-change hook (optional DDNS)

The carrier's v6 prefix usually changes on each redial/reattach, so the reachable
address is not stable. Point `INTERNET_IPV6_HOOK` at a script that publishes the new
address (e.g. updates an AAAA record):

```sh
#!/usr/bin/env sh
# /opt/ddns/update-aaaa.sh — $1 is the new global IPv6 address
new_addr="$1"
curl -fsS -X PATCH "https://api.example-dns.tld/records/host-aaaa" \
     -H "Authorization: Bearer $DDNS_TOKEN" \
     -d "{\"type\":\"AAAA\",\"content\":\"$new_addr\"}" >/dev/null
```

- Fires once when the address first appears and once each time it changes.
- Does **not** fire when v6 is lost — set a short TTL on your AAAA record so a stale
  address expires on its own. Loss still shows as `ipv6_state=unavailable` in the
  status file.
- Runs backgrounded with a timeout; a broken hook never disturbs connectivity.
- If you don't want a hook, leave `INTERNET_IPV6_HOOK` unset and read the current
  address from `/run/internet-status` (`ipv6=`) yourself.

## 5. Host firewall / reachability notes

- The container runs host-network + privileged, so the global v6 address lands on
  the **host**; the host itself becomes reachable. The sidecar installs **no**
  firewall rules and enables **no** forwarding.
- You are responsible for the host firewall — ensure inbound is allowed on the ports
  you need (e.g. SSH) over IPv6.
- Some carriers filter inbound IPv6 at their edge. If the address is up but
  unreachable from outside, confirm with your carrier — that filtering is outside the
  sidecar's control.

## 6. Turn it off

Set `INTERNET_ENABLE_IPV6=0` (or unset `INTERNET_IPV6_HOOK` to drop just the hook)
and restart the sidecar. Behavior reverts to feature-032 IPv4-only.

## Tests

```sh
make test-shell        # runs the sidecar shell tests incl. new v6 lifecycle + hook
make lint              # shellcheck -x over the sidecar scripts + tests
```
