# VoWiFi ePDG tunnel (Phase 1)

Establishes an IKEv2/IPsec **VoWiFi tunnel to the mobile operator's ePDG**
(Evolved Packet Data Gateway) so we can later reach the operator's IMS core for
SIP voice calls — the same mechanism a phone uses for Wi‑Fi Calling.

- **Operator:** Vodafone (Vi) India — MCC `404`, MNC `43`.
- **SIM:** the USIM inside the modem (Quectel **EC200U**) on `/dev/ttyUSB6`.
- **Auth:** EAP-AKA is run against the real SIM via `AT+CSIM` — no Ki/OPc needed.
- **Tool:** [`fasferraz/SWu-IKEv2`](https://github.com/fasferraz/SWu-IKEv2)
  (osmocom foss-ims-client "Option 1"), vendored at build time.

This is **standalone** and does not touch the main `gsm-sip-bridge` service.

## How it works

The dialer authenticates with EAP-AKA (the SIM computes the response), brings up
`tun1` inside a dedicated network namespace (`ims`), gets an inner IP + the
**P‑CSCF** (IMS SIP registrar) address, and installs the tunnel's split-default
routes *inside that namespace* — so the container's own path to the ePDG is
untouched. The container is on its own Docker bridge network, isolating all of
this from the host.

**Status: working.** Verified end-to-end against the live Vodafone India ePDG —
EAP-AKA succeeds via the EC200U's SIM, an inner IPv6 address and P-CSCF are
assigned, and the P-CSCF's SIP port is reachable over the tunnel. Two
modem/card-specific fixes to the vendored dialer were required; see
`patches/0001-ec200u-at-csim-fixes.patch` and Troubleshooting below.

## Prerequisites

- The modem SIM must be provisioned for VoWiFi/IMS by the operator.
- `AT+CSIM` passthrough must work (verified on this EC200U: `AT+QCFG="ims" -> 1,1`,
  `AT+CSIM` STATUS returns a valid USIM FCP).
- `/dev/ttyUSB6` free (not held by the bridge or ModemManager).

## Run

```bash
cd docker/epdg
cp .env.example .env        # optional — defaults already target Vi India
docker compose -f docker-compose.epdg.yml up --build
```

### Running `gsm-sip-bridge ims-register` against the tunnel (Phase 2)

The `gsm-sip-bridge` binary needs to run *inside* the `ims` network namespace
that `entrypoint.sh` creates, since that's the only place the tunnel-assigned
address and route to the P-CSCF exist. The container's base image (Debian
bookworm-slim) doesn't have a matching glibc for a binary built on an
arbitrary host — build it in a matching environment first:

```bash
# from the repo root
docker run --rm -v "$PWD:/src" -w /src -e CARGO_TARGET_DIR=/tmp/bt rust:1-bookworm bash -c '
  apt-get update -qq && apt-get install -y -qq libasound2-dev pkg-config libudev-dev
  cargo build -p gsm-sip-bridge --bin gsm-sip-bridge
  cp /tmp/bt/debug/gsm-sip-bridge /src/gsm-sip-bridge-bookworm'
docker exec -u root epdg-tunnel bash -c "apt-get update -qq && apt-get install -y -qq libasound2 || apt-get install -y -qq libasound2t64"
docker cp gsm-sip-bridge-bookworm epdg-tunnel:/usr/local/bin/gsm-sip-bridge
rm gsm-sip-bridge-bookworm

PCSCF=$(docker exec epdg-tunnel cat /tmp/pcscf)
docker exec epdg-tunnel ip netns exec ims /usr/local/bin/gsm-sip-bridge -v ims-register \
  --modem /dev/ttyUSB6 --pcscf "$PCSCF" --mcc 404 --mnc 043 [--sec-agree] [--tcp]
```

See "Phase 2" findings below for what to expect.

## Verify (Phase 1 exit criteria)

- Logs show the EAP-AKA exchange **succeed** (progress past `STATE 2`, no
  "Unable to access serial port..." fallback-to-defaults message).
- Logs print `STATE CONNECTED` plus `P-CSCF IPV4/IPV6 ADDRESS [...]`.
- Tunnel iface is up in the namespace with an inner IP:
  ```bash
  docker exec epdg-tunnel ip netns exec ims ip addr show tun1
  ```
- P-CSCF reachable over the tunnel. **ICMP is commonly filtered by the
  operator** (confirmed on Vodafone India) — use a TCP connect to the SIP port
  instead of ping:
  ```bash
  docker exec epdg-tunnel ip netns exec ims bash -c \
    ">/dev/tcp/$(docker exec epdg-tunnel cat /tmp/pcscf)/5060 && echo OK"
  ```
- `docker ps` shows the container `healthy` (the healthcheck does the same
  TCP-connect check).
- Host `ip route` is unchanged before/after — the tunnel's split-default routes
  live only inside the container's nested `ims` netns.

## Troubleshooting

| Symptom | Likely cause / fix |
|---|---|
| Dialer stops at `STATE 2` | EAP-AKA auth failed — wrong MCC/MNC padding, or the SIM isn't authorized for ePDG (subscription). |
| `Unable to access serial port/smartcard reader/server. Using DEFAULT RES, CK and IK` | The card/modem-specific `AT+CSIM` quirks below aren't patched — check `patches/0001-ec200u-at-csim-fixes.patch` was applied. |
| `cannot create network namespaces` | Missing `cap_add: SYS_ADMIN`. |
| `mount --make-shared /run/netns failed: Permission denied` | The default docker-default AppArmor profile blocks `mount` even with `CAP_SYS_ADMIN`. Needs `security_opt: [apparmor:unconfined]` (already set in the compose file). |
| No `A` record resolved | Set `EPDG_IP` explicitly (see `.env.example`). |
| IKE_SA_INIT gets no/rejected response | ePDG may geoblock non-operator source IPs. Fallback: route ePDG egress over a mobile bearer. |
| `ping` to P-CSCF gets 100% loss | Expected — the operator filters ICMP over the tunnel. Use the TCP-connect check above instead. |
| Millions of `Press q to quit...` log lines, high CPU | The dialer's post-connect loop does `select()` on stdin; if stdin is closed/EOF it busy-spins. `entrypoint.sh` feeds it a never-closing pipe (`tail -f /dev/null`) to prevent this — make sure that's still in place if you modify the launch command. |
| Tunnel drops after minutes/an hour | Keepalive not running — check `KEEPALIVE_INTERVAL` and the keepalive log line. |

### Card/modem-specific fix (`patches/0001-ec200u-at-csim-fixes.patch`)

Found by probing `/dev/ttyUSB6` directly against the Quectel EC200U + Vodafone
India USIM:
1. `SELECT` with `P2=0x00` (the upstream script's hardcoded value) is rejected
   by this card as "wrong P1/P2" (`SW=6B00`). It needs `P2=0x0C`. The script's
   hardcoded generic USIM AID also doesn't match this card — the real AID
   (read from `EF_DIR`) is `A0000000871002FFF605FF89000001FF`.
2. The EC200U auto-chains `GET RESPONSE` internally and returns the full
   `AUTHENTICATE` result with `SW=9000` directly, with no `61XX` "more data"
   marker — the upstream script only knows the classic two-step flow and
   crashes (`UnboundLocalError`) when that marker is absent, silently falling
   back to default (non-authenticated) values.

Other modems/cards may not need this patch, or may need different fixes for
the same underlying quirks (SELECT P2 conventions and GET RESPONSE chaining
vary by card/modem firmware).

## Next (not in this phase)

- **Reliability fallback:** the strongSwan path (osmocom "Option 2"), which needs
  a virtual PC/SC reader bridging the modem's SIM to `eap-sim-pcsc`.

## Phase 2: IMS-AKA SIP REGISTER — findings

Implemented as `gsm-sip-bridge ims-register` (see `gsm-sip-bridge/src/ims/`,
`modules/usim.rs`). It reads the IMSI, discovers/selects the USIM ADF, opens a
UDP/TCP connection to the P-CSCF (learning the real tunnel-assigned local
address *before* building the first REGISTER — an unspecified Contact gets
silently dropped by some networks), sends an unauthenticated REGISTER, and on
a 401 challenge runs the RAND/AUTN through the SIM (`AT+CSIM` AUTHENTICATE,
reusing the Phase 1 EC200U fixes) to compute an RFC 3310 AKAv1-MD5 digest
response (RES used as the raw-byte "password" in H(A1), not hex-encoded).
AKA sync-failure (AUTS) resync is implemented per RFC 3310 §4.4.

**Status: blocked, precisely diagnosed, real Gm IPsec required.** Vodafone
India's P-CSCF rejects a plain digest REGISTER outright:
- No `Security-Client` header → `421 Extension Required` / `Require: sec-agree`.
- With a `Security-Client: ipsec-3gpp` proposal (tried `ealg=null`, then the
  spec's only other option `ealg=des-ede3-cbc` with `alg=hmac-sha-1-96`,
  `prot=esp`, `mod=trans`, correct RFC 3329 syntax, distinct `port-c`/`port-s`)
  → `494 Security Agreement Required`, every time, with an **empty**
  `Security-Server: ipsec-3gpp ; q=0.1` (no counter-proposed spi/port/alg).
- All three structurally different proposals produced a **byte-identical**
  response in ~200ms — strong evidence the server isn't evaluating our
  specific header values at all, i.e. this isn't a "wrong algorithm" problem.
- Subscription/provisioning was ruled out: VoWiFi works on a real phone (Moto)
  with this same SIM, so the network does support this IMSI for VoWiFi.
- This means the real client sends something in its REGISTER we aren't
  replicating (header set, ordering, or content beyond what RFC 3329/TS 24.229
  document). **Closing this gap needs ground truth** — a packet capture of a
  real phone's actual VoWiFi REGISTER on this network, to diff header-by-header
  against ours. Guessing further at the wire format without that capture has
  demonstrated ~zero yield (3 attempts, identical canned rejection each time).
- Reaching an actual `200 OK` also requires implementing the real Gm IPsec SA
  once negotiation succeeds: deriving ESP keys from the AKA `CK`/`IK` (TS
  33.203 Annex H) and installing kernel XFRM SAs in the `ims` netns — not yet
  attempted, since negotiation itself doesn't yet succeed.

See `gsm-sip-bridge/src/ims/mod.rs` module docs for the full design rationale
(including why this bypasses PJSIP's built-in digest auth entirely).

See `specs`/the plan for the full design and rationale.
