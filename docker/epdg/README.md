# VoWiFi ePDG tunnel (Phase 1)

Establishes an IKEv2/IPsec **VoWiFi tunnel to the mobile operator's ePDG**
(Evolved Packet Data Gateway) so we can later reach the operator's IMS core for
SIP voice calls — the same mechanism a phone uses for Wi‑Fi Calling.

- **Operators tested:** Vodafone Idea (Vi) India (MCC `404`, MNC `43`) and
  Airtel India (MCC `404`, MNC `94`) — set `MCC`/`MNC` env vars for either
  (see `.env.example`).
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

**Status: working.** Verified end-to-end against two live operators —
Vodafone Idea (Vi) India and Airtel India — on two different SIMs. EAP-AKA
succeeds via the EC200U's SIM, an inner IPv6 address and P-CSCF are
assigned, and the P-CSCF's SIP port is reachable over the tunnel. The
vendored dialer needed several modem/card-specific fixes, including
discovering the USIM AID at runtime instead of hardcoding it — the two SIMs
tested have *different* AIDs; see `patches/0001-ec200u-at-csim-fixes.patch`
and Troubleshooting below.

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

Found by probing `/dev/ttyUSB6` directly against the Quectel EC200U with two
different operators' USIMs (Vi India and Airtel India):
1. `SELECT` with `P2=0x00` (the upstream script's hardcoded value) is rejected
   by these cards as "wrong P1/P2" (`SW=6B00`). It needs `P2=0x0C`.
2. The upstream script's hardcoded generic USIM AID doesn't match real cards
   — and **different SIMs have different AIDs** (confirmed: Vi India is
   `A0000000871002FFF605FF89000001FF`, Airtel India is
   `A0000000871002FF49FFFF89030900FF`). The patch discovers the real AID at
   runtime by reading `EF_DIR` instead of hardcoding either one.
3. The EC200U auto-chains `GET RESPONSE` internally and returns the full
   `AUTHENTICATE` result with `SW=9000` directly, with no `61XX` "more data"
   marker — the upstream script only knows the classic two-step flow and
   crashes (`UnboundLocalError`) when that marker is absent, silently falling
   back to default (non-authenticated) values.
4. The upstream script's per-command retry fires a duplicate `AT+CSIM` send
   if no `OK`/`ERROR` appears within 0.5s. A *real* network AKA challenge's
   SIM computation can take several seconds — comfortably longer than a
   local/dummy test — so 0.5s spuriously retransmits mid-response, corrupting
   the buffer with two overlapping `+CSIM` replies. Raised to 5s, with the
   response parser also now validating that a candidate fragment is actual
   hex before accepting it, as a second line of defense.

Other modems/cards may not need this patch, or may need different fixes for
the same underlying quirks (SELECT P2 conventions, AID, and GET RESPONSE
chaining all vary by card/modem firmware).

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

**Status on Vodafone Idea (Vi) India: blocked at the sec-agree/Gm-IPsec layer.**
Vi's P-CSCF rejects a plain digest REGISTER outright:
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

**Update — confirmed working on Airtel India, via a real IMS stack.** On the
sibling `feature/epdg-asterisk-ims` branch, a full Asterisk + PJProject build
(the wiki's "Option 2") reached a real `401` + AKA challenge + populated
`Security-Server` + Gm-IPsec setup + **`200 OK`** on Airtel India (MCC
404/MNC 94), using the *same* AKA-over-`AT+CSIM` SIM authentication as this
Rust client. That proves Vi's block is specific to Vi's network, not a bug in
this approach.

**Update — this Rust client now reaches a real `401` + AKA challenge on
Airtel too.** Ported from a wire capture of the working Asterisk registration
(`pjsip set logger on` against the same tunnel/SIM). A plain digest REGISTER,
and even the old verbose `Security-Client` proposal, got an instant `406 User
Unknown` on Airtel — byte-identical regardless of header content, the same
signature as Vi's blanket rejection. The actual fixes, all confirmed against
the captured ground truth:
1. **Request-URI must be the literal P-CSCF address**, not the home-network
   realm. PJSIP's outbound registration (`pjsip_regc_init`'s `srv_url`, fed
   from `server_uri`) uses the literal address as the Request-URI, and
   apparently so must we — a realm Request-URI got the instant `406`.
2. **`Security-Client` in the exact minimal format** real implementations
   use: no spaces, no `prot=`/`mod=`/`q=`, one `ipsec-3gpp;alg=..;ealg=..;
   spi-c=..;spi-s=..;port-c=..;port-s=..` tuple per integrity algorithm
   (`hmac-md5-96`/`hmac-sha-1-96`), each with `ealg=null` (no ESP encryption)
   — matches sysmocom's `volte.c` and the captured wire traffic exactly.
3. **`Require: sec-agree` + `Proxy-Require: sec-agree`**, not just
   `Supported: sec-agree` — Airtel requires the extension be mandated, not
   merely advertised. Also send `Supported: path, sec-agree`.
4. **A placeholder empty `Authorization` header on the very first,
   pre-challenge REGISTER** (`response=""`, `nonce=""`) — Asterisk always
   attaches one once `sec-agree` is in play; omitting it was one of the
   things that produced the `406`.
5. **`P-Access-Network-Info: 3GPP-WLAN`** and an **`Allow`** header listing
   supported methods — both TS 24.229/real-UE staples that were simply
   missing.
6. An enriched **`Contact`**: `;transport=TCP` URI parameter plus the 3GPP
   feature tags real UEs send (`+g.3gpp.icsi-ref`, `audio`,
   `+sip.instance`).
7. **Via transport token must match the actual transport** (`SIP/2.0/TCP`
   over a TCP socket, not a hardcoded `SIP/2.0/UDP`) — a latent bug in this
   client found while chasing the above; harmless-looking but
   protocol-incorrect and worth fixing regardless.

With all of the above, `gsm-sip-bridge ims-register --tcp --sec-agree`
against Airtel gets a real `401 Unauthorized` with a genuine AKA challenge
(`WWW-Authenticate` + populated `Security-Server`), runs RAND/AUTN through
the SIM, and resends a correct digest `Authorization`. That resend still
gets a fresh `401` rather than `200 OK` — expected, and not a bug: Asterisk's
own captured wire trace shows its *second* REGISTER goes out on a **new TCP
connection to the P-CSCF's negotiated `port-s` (6000, not 5060)**, which only
works because it has already set up a real Gm IPsec SA (kernel XFRM, via
`libmnl` in `volte.c`) deriving ESP keys from the AKA `CK`/`IK` (TS 33.203
Annex H). Until this client does the same — install XFRM SAs in the `ims`
netns and resend the authenticated REGISTER over that protected transport to
the negotiated port — it cannot get past this second challenge. This is a
substantial, separate piece of work (kernel netlink/XFRM from Rust, e.g. via
the `rtnetlink`/`nix` crates or shelling out to `ip xfrm`), not yet
implemented here.

See `gsm-sip-bridge/src/ims/mod.rs` module docs for the full design rationale
(including why this bypasses PJSIP's built-in digest auth entirely).

See `specs`/the plan for the full design and rationale.
