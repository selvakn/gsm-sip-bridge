# Quickstart: strongSwan ePDG Tunnel — live verification runbook

Operator-run checks against real carriers (CI cannot cover these; same model as 011's
quickstart). Prerequisites: EC200U attached with a VoWiFi-provisioned SIM, `docker/epdg/.env`
configured (MCC/MNC per SIM), PBX reachable for the call test.

## 1. Build & start with the strongSwan engine

```bash
cd docker
# .env / compose environment:
#   TUNNEL_ENGINE=strongswan
docker compose up --build -d
docker logs -f gsm-sip-bridge   # container name per compose
```

Expect, in order:
1. `[entrypoint]` netns `ims` + XFRM interface created (idempotent on restart)
2. pcscd + `vowifi-usim-bridge` started; bridge logs vpcd connection
3. swanctl conf rendered (IMSI read via `vowifi-imsi`, NAI logged with IMSI visible)
4. charon: `EAP method EAP_AKA succeeded`, `CHILD_SA ims{1} established`
5. `received P-CSCF server IP …` → `[entrypoint] tunnel UP. P-CSCF: <addr>` → `/tmp/pcscf`
6. veth pair + both agents supervised (unchanged 011 log lines)

Spot checks:

```bash
docker exec <ctr> swanctl --list-sas                 # IKE + CHILD SA present
docker exec <ctr> ip netns exec ims ip addr          # inner addr on tun iface, veth-ims
docker exec <ctr> cat /tmp/pcscf
docker exec <ctr> ip netns exec ims sh -c '>/dev/tcp/'"$(docker exec <ctr> cat /tmp/pcscf)"'/5060 && echo OK'
```

## 2. SC-004 — EAP-AKA on both carriers

Repeat step 1 once per SIM: Vi India (404/043 — tunnel establishment is the pass bar; IMS
registration stays blocked per prior findings) and Airtel India (404/094 — full bar).

## 3. SC-002 — forced-outage recovery

With the tunnel up and agents registered:

```bash
# interrupt WAN for <60s (pull uplink / down the host's egress iface), restore
```

Pass: within 90 s of restoration `swanctl --list-sas` shows a re-established SA, **netns and
veth untouched** (`ip netns exec ims ip link show veth-ims` never disappears), agents did not
restart (`docker exec <ctr> pgrep -af vowifi-` PIDs unchanged), line callable again.

## 4. SC-001 — 24 h rekey soak

Leave running ≥ 24 h. Then:

```bash
docker exec <ctr> grep -cE 'rekey|reauth' /tmp/charon.log     # ≥ 1 scheduled rekey seen
docker exec <ctr> pgrep -af vowifi-                            # same PIDs as at start
```

Pass: ≥ 1 IKE rekey (and any re-auth) completed; zero agent restarts attributable to the
tunnel; `/tmp/pcscf` still valid.

## 5. SC-003 — long-idle inbound call (Airtel)

≥ 12 h after startup, call the SIM's MSISDN from another phone. Pass: PBX extension rings
within 5 s, two-way audio after answer — identical to 011's inbound-call check.

## 6. SC-005 / SC-006 — engine switch & legacy regression

```bash
# flip TUNNEL_ENGINE=swu (no rebuild), restart the container
docker compose up -d
```

Pass: SWu path behaves exactly as feature 011 today (STATE CONNECTED, P-CSCF file, agents
bridge a call). Flip back to `strongswan` and confirm the tunnel returns. Both directions are
config-only changes on the same image.

## Troubleshooting entry points

| Symptom | Look at |
|---|---|
| EAP fails at IKE_AUTH | bridge logs (APDU trace) — quirk normalization misfire vs. carrier rejection; compare with the SWu engine on the same SIM |
| No P-CSCF line in charon log | `p-cscf.conf` loaded? connection name must be `ims` (plugin keys on it) |
| Inner packets dropped | `disable_policy=1` sysctl on the tun iface inside the netns (known kernel gotcha, see research.md item 3) |
| Port busy loops in bridge | concurrent AKA from vowifi-ims-agent re-registration or CS daemon — expected transient; sustained ⇒ consider the `flock` escalation (research.md item 6) |
| Fork won't build on musl | research.md item 8 fallback: carried patch or Debian-built static stage |
