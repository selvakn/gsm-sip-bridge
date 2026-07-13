# Quickstart: strongSwan ePDG Tunnel — live verification runbook

Operator-run checks against real carriers (CI cannot cover these; same model as 011's
quickstart). Prerequisites: EC200U attached with a VoWiFi-provisioned SIM, `.env` (repo root,
see `.env.example`) configured with `TUNNEL_ENGINE=strongswan` plus MCC/MNC per SIM, PBX
reachable for the call test.

## 1. Build & start with the strongSwan engine

```bash
cd docker
# repo-root .env:
#   TUNNEL_ENGINE=strongswan
docker compose up --build -d
docker logs -f gsm-sip-bridge   # container name per compose
```

Expect, in order (log prefixes as actually observed against a real image build — see
`docs/vowifi-epdg-research-notes.md`'s "Phase 5" section for the build/entrypoint issues found
and fixed while confirming this sequence):
1. `[entrypoint] created netns ims` + `created XFRM interface tun23 (if_id=23) in netns ims`
   (idempotent on restart — a second run instead logs `already exists, reusing` for both)
2. `[entrypoint] rendered swanctl connection for mcc=... mnc=... epdg=...`
3. `[entrypoint] started pcscd (pid ...)` then `starting vowifi-usim-bridge ... supervised...`;
   the bridge itself logs `connected to vpcd addr=127.0.0.1:35963`
4. `[entrypoint] started charon (pid ...)`, followed by `[charon]`-prefixed lines (charon's own
   filelog, tailed to `docker logs`): `loaded plugins: charon random nonce openssl fips-prf
   hmac kernel-netlink resolve socket-default stroke vici updown eap-identity eap-sim
   eap-sim-pcsc eap-aka p-cscf counters`, then IKE_SA_INIT negotiation, then an `IKE_AUTH`
   request whose payload list includes `CPRQ(... PCSCF4 PCSCF6)` — confirms the P-CSCF request
   went out — then (on a real network path) `EAP method EAP_AKA succeeded` and
   `CHILD_SA ims{1} established`
5. `[entrypoint] tunnel UP. P-CSCF: <addr>` → `/tmp/pcscf` written. If the CHILD_SA doesn't
   establish within 90×2s, `[entrypoint] WARNING: could not confirm P-CSCF assignment; leaving
   charon running (inspect /tmp/charon.log)` instead — charon keeps running/retrying, matching
   the SWu engine's equivalent fallback shape
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
| `critical plugin 'charon' has unmet dependency: NONCE_GEN`/`HASH_SHA1` at charon startup | `load_modular` reverted to the fork's own default (`yes`) somehow, or `charon-extra.conf` isn't loaded — see docs/vowifi-epdg-research-notes.md's Phase 5 build findings; confirm `/etc/strongswan.d/charon-extra.conf` is present and `charon --version`'s `loaded plugins:` line lists `random nonce` |
| `swanctl --load-conns` reports nothing loaded despite a real `conf.d/epdg.conf` | `/etc/swanctl/swanctl.conf` missing its `include conf.d/*.conf` line — confirmed builds ship it empty by default |
| Second entrypoint run tries `ip netns add` on an already-existing `ims` and errors | pre-fix symptom of the `ip netns list`-formatting bug (docs/vowifi-epdg-research-notes.md Phase 5) — should not reproduce on current code, which checks `/var/run/netns/$NETNS` instead |
