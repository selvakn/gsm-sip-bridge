# Quickstart: live verification of the underlay guard

Rigs:
- **Vodafone:** local host, container `gsm-sip-bridge-test`, host networking, privileged.
- **Jio:** remote Pi `pi@192.168.100.2`, container `gsm_gsm-sip-bridge_1`.

Run a build of this branch with the probe forced off, the same configuration as the 098 baseline in research R1, so the numbers are comparable. Record everything in `live-verification.md`.

Find the line's ePDG address with `docker exec <c> swanctl --list-sas | grep remote`.

## 0. Watch

```sh
docker exec <c> tail -n0 -F /tmp/charon.log | grep --line-buffered -E \
  "DPD|retransmit|giving up|initiating IKE_SA|established|deleting|destroying|INFORMATIONAL" &
docker logs -f --since 1s <c> 2>&1 | grep --line-buffered -E "\[supervise\]|registered" &
```

## 1. The DPD command works (FR-004a)

```sh
docker exec <c> swanctl --dpd --ike ims0; echo "exit=$?"
docker exec <c> grep -E "vici dpd|INFORMATIONAL (request|response) [0-9]+ \[ \]" /tmp/charon.log | tail -3
```

Expect `exit=0`, then within 2s a `generating INFORMATIONAL request N [ ]` and a `parsed INFORMATIONAL response N [ ]`. Against a half-open or missing IKE_SA, expect a non-zero exit with `dpd failed: …`.

## 2. No-route outage (US1, SC-001/002/003)

```sh
EPDG=<epdg-ip>
docker exec <c> sh -c "trap 'ip route del blackhole $EPDG/32 2>/dev/null' EXIT; \
  echo START \$(date +%T); ip route add blackhole $EPDG/32 && sleep 330; \
  ip route del blackhole $EPDG/32; echo RESTORED \$(date +%T)"
```

Pass if:
- `uplink down (no_route …)` appears within 60s of START;
- between that line and RESTORED there are 0 `recovered from`, 0 `restarting agent`, 0 `tunnel flapping` and 0 supervisor-issued `initiating IKE_SA`, except at most one logged as the held initiation (FR-010a);
- `vowifi-status` shows `uplink: no_route (held …)` mid-outage;
- `uplink restored after …` appears, and `registered` follows within 30s of RESTORED.

On the Pi there is no `iptables`, but `ip route` works the same way inside the container (host networking).

## 3. Upstream-dead outage (US1, SC-001/002/003)

The bridge image has no firewall tools, so use a throwaway container with its own, auto-removed table:

```sh
docker run --rm --privileged --network host alpine:3.20 sh -c "apk add -q nftables >/dev/null; \
  trap 'nft delete table inet spike100 2>/dev/null' EXIT INT TERM; \
  nft add table inet spike100 && \
  nft add chain inet spike100 input '{ type filter hook input priority -300; }' && \
  nft add rule inet spike100 input ip saddr $EPDG counter drop && echo START \$(date +%T); \
  sleep 330; nft list table inet spike100 | grep counter; nft delete table inet spike100; echo RESTORED \$(date +%T)"
```

Pass if:
- `checking the ePDG is alive … (DPD sent)` appears, then `uplink down (no_response …)` within 120s of START;
- the same zero counts and recovery bounds as step 2 hold.

## 4. A real carrier fault is still repaired (US2, SC-004)

1. **Single P-CSCF, Vodafone:** black-hole the P-CSCF only, inside the line's netns. Use a /32 route to the veth peer, never a blackhole route; see the Pi black-holing note from 097. Expect `checking the ePDG is alive`, then `ePDG answered; proceeding with PcscfPoolExhausted`, and recovery within the 097 measured time + 30s. No `uplink down` line.
2. **Namespace route:** `ip netns exec ims0 ip route del default` (and `-6`). Expect `DefaultRouteMissing` repaired on the next tick, and no hold.

## 5. Long outage (SC-005)

Repeat step 2 with `sleep 1000` (about 16.5 minutes). Expect exactly one `uplink hold reached 15m … running one recovery`, a new `uplink down` right after, and recovery within 30s of RESTORED.

## 6. Guard off (SC-008)

Set `[vowifi].underlay_guard = false`, recreate the container (do not restart it; see the memory on switching transports), and check that startup says `underlay guard: off`. Repeat steps 2 and 3. Expect the R1 baseline, a teardown at about 80s followed by rebuilds, except that the half-open IKE_SA is no longer terminated while charon is still retrying.

## 7. Idle traffic unchanged (SC-006)

On a healthy line, count outer packets to and from the ePDG for one idle hour, before and after this build. Use the 098 method: `tcpdump -c` on UDP 4500 in the privileged container. Expect the same within normal variance, and no `swanctl --dpd` calls in the log.

## 8. Metrics

After steps 2 and 3:

```sh
# 9091 is [metrics].port in the example config
curl -s localhost:9091/metrics | grep -E "vowifi_uplink_(holds|held)"
```

Expect:
- `uplink_holds_total{cause="no_route"}` and `{cause="no_response"}` each at least 1;
- `uplink_held_seconds_total` each at least 250;
- `uplink_held 0` after restore.
