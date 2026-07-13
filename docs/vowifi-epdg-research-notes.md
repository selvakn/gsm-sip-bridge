# VoWiFi ePDG tunnel — research notes (historical)

> **Superseded as a deployment guide.** The standalone `docker/epdg/`
> container this document originally described has been merged into the
> main `docker/` image — one `docker compose up --build` from `docker/`
> now builds and runs both the circuit-switched daemon and (when
> `[vowifi].enabled = true` in the mounted `config.toml`) this VoWiFi/ePDG
> tunnel + bridge agents together. See `docker/docker-compose.yml` and
> `docker/entrypoint.sh` for the current setup, and `docs/vowifi-bridge.md`
> for the inbound-bridge architecture. This file is kept as-is for its
> engineering findings below (the AKA/Gm-IPsec debugging history, per-carrier
> behavior, and the EC200U patch rationale) — the specific `docker exec`/
> `docker cp` commands in the walkthroughs below are no longer how this is
> actually run.

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
docker cp gsm-sip-bridge-bookworm epdg-tunnel:/usr/local/bin/gsm-sip-bridge
rm gsm-sip-bridge-bookworm

PCSCF=$(docker exec epdg-tunnel cat /tmp/pcscf)
docker exec epdg-tunnel ip netns exec ims /usr/local/bin/gsm-sip-bridge -v ims-register \
  --modem /dev/ttyUSB6 --pcscf "$PCSCF" --mcc 404 --mnc 043 [--sec-agree] [--tcp]
```

(`libasound2`/`libvo-amrwbenc0`/`libopencore-amrwb0` are already installed in
the `epdg-tunnel` image itself — see the Dockerfile — so no `docker exec ...
apt-get install` step is needed here anymore.)

See "Phase 2" findings below for what to expect.

### Running `gsm-sip-bridge ims-call` (Phase 3: an actual test call)

Same binary, but built with AMR-WB support (`--features
gsm-sip-bridge/amr-linked`) — a live test call against Airtel found VoWiFi
requires AMR-WB and rejects a PCMU-only offer with `488 Not Acceptable
Here`; see "Phase 3" findings below. Needs the AMR-WB *build-time* dev
headers in the build container (the *runtime* shared libs are already in
the `epdg-tunnel` image, same as above):

```bash
docker run --rm -v "$PWD:/src" -w /src -e CARGO_TARGET_DIR=/tmp/bt rust:1-bookworm bash -c '
  apt-get update -qq && apt-get install -y -qq \
    libasound2-dev pkg-config libudev-dev \
    libvo-amrwbenc-dev libopencore-amrwb-dev
  cargo build -p gsm-sip-bridge --bin gsm-sip-bridge --features gsm-sip-bridge/amr-linked
  cp /tmp/bt/debug/gsm-sip-bridge /src/gsm-sip-bridge-bookworm'
docker cp gsm-sip-bridge-bookworm epdg-tunnel:/usr/local/bin/gsm-sip-bridge
rm gsm-sip-bridge-bookworm

PCSCF=$(docker exec epdg-tunnel cat /tmp/pcscf)
docker exec epdg-tunnel mkdir -p /tmp/recordings
docker exec epdg-tunnel ip netns exec ims /usr/local/bin/gsm-sip-bridge -v ims-call \
  --modem /dev/ttyUSB6 --pcscf "$PCSCF" --mcc 404 --mnc 094 --tcp --sec-agree \
  --to "+91XXXXXXXXXX" --record /tmp/recordings/test-call.wav \
  --ring-timeout-secs 30 --call-duration-secs 15
docker cp epdg-tunnel:/tmp/recordings/test-call.wav .
```

This places a **real call** over the live network — make sure the callee
knows to expect and answer it.

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
(`WWW-Authenticate` + populated `Security-Server`) and runs RAND/AUTN through
the SIM.

**Update — full `200 OK` reached, with real Gm IPsec (kernel XFRM), no
Asterisk/PJProject needed.** Implemented in `gsm-sip-bridge/src/ims/gm_ipsec.rs`
per the plan in `docs/gm-ipsec-xfrm-plan.md` (derived from sysmocom's
`volte.c` and a wire capture of a working Asterisk registration):

1. **Key derivation (TS 33.203 Annex H)**: the AKA `IK`/`CK` are used
   *directly* as the XFRM auth/cipher keys — no KDF. `hmac-sha-1-96` needs a
   160-bit key but `IK` is only 128 bits, so it's zero-padded to 20 bytes.
2. **Topology**: two logical tunnels (`local_c<->remote_s`,
   `local_s<->remote_c`), each needing an outbound + inbound XFRM state (4
   total) and a matching policy (4 total) — see the plan doc for the full
   derivation of which SPI/port goes where.
3. **Shells out to `ip xfrm`** (not raw netlink) to stay consistent with this
   crate's zero-`unsafe` policy. Two non-obvious syntax quirks found only by
   testing against the real kernel/iproute2 build in the container:
   - `proto esp` **requires** an `enc`/`aead` clause even for a null cipher
     (`ALGO-TYPE value "enc" or "aead" is required with XFRM-PROTO value
     "esp"`) — but the keymat must be a **truly empty string** (`""`), not
     `0x` or a dummy zero byte (both get `EINVAL`, since `cipher_null`
     expects exactly a zero-length key).
   - The policy selector's `proto` must be a **numeric** IP protocol number
     (`6` for TCP, `17` for UDP) — the literal names `tcp`/`udp` are rejected
     as `"PROTO value is invalid"` on this iproute2 build, despite being
     valid per `ip xfrm policy help`'s own grammar. Selector order also
     matters: `proto` before `sport`/`dport`.
4. **Reconnecting over the negotiated port** (once SAs are installed, the
   authenticated REGISTER must go out on a *new* connection from our
   proposed `port-c` to the network's negotiated `port-s`, not the original
   socket/port) needs the old connection's exact local port back — and a
   plain `SO_REUSEADDR` bind wasn't enough: closing a TCP socket via a
   normal (graceful, FIN-based) `drop()` immediately before rebinding the
   same port raced with the kernel's `TIME_WAIT`/`FIN_WAIT` teardown and hit
   `Address already in use`. Fixed by forcing an abortive close
   (`SO_LINGER` 0, sends `RST`) right before dropping the old connection —
   see `SipTransport::force_close()`.
5. **`Security-Verify` header** (RFC 3329 §2.4): the retry sent over the new
   Gm-protected connection must echo the network's own `Security-Server`
   value back verbatim in a `Security-Verify` header, confirming which
   negotiated SA is in use. Missing this got a *different*, later-stage
   `406 User Unknown` (structurally distinct from the earlier blanket
   rejections — it came with a `Date` header and a differently-formatted
   `To` tag, i.e. from a different network element than the SBC/P-CSCF that
   issues the generic ones) even with the SAs correctly installed and the
   connection successfully reconnected over the negotiated port.

With all five, a real `200 OK` comes back from Airtel's IMS core, complete
with `P-Associated-URI` (the actual MSISDN), `Path`, and `Service-Route`
headers — the same outcome Asterisk reaches, achieved here without Asterisk
or PJProject at all. Verified reproducible across repeated runs; XFRM state
is torn down at the end of each run (success or failure) so repeat
invocations don't collide with stale SAs from a previous one.

**Re-tested against Vi with the now-complete header set: still blocked, and
the block got *more* aggressive.** With the Vodafone SIM swapped back in and
the full Airtel-derived recipe (`Require`/`Proxy-Require: sec-agree`,
enriched `Contact`, `P-Access-Network-Info`, etc. — everything that reaches
`200 OK` on Airtel):
- `--tcp --sec-agree` (the full recipe): instant `403 Forbidden`, no
  `WWW-Authenticate`, no `Security-Server`, no `Date` header — a
  content-free rejection matching the earlier "blanket SBC block" signature.
- `--tcp` alone (no sec-agree headers): `421 Extension Required` with an
  **empty** `Security-Server: ipsec-3gpp ; q=0.1` (no counter-proposed
  spi/port/alg) — matching the *original* Vi finding from before any of
  this session's fixes, byte-for-byte the same shape.

Notably the *plainer* request gets the more informative response (`421`
with a real, if empty, `Security-Server`, plus a `Date` header and a
differently-formatted `To` tag suggesting it reached a real network element)
while the *fuller*, spec-compliant request gets slapped down harder and
earlier (`403`, no information at all). That's the opposite of what
happened on Airtel, where the fuller request is what's required to get
anywhere — reinforcing that Vi's block is a deliberate network-side policy
(most likely fingerprinting `Require: sec-agree` + the full header
combination as non-partner traffic) rather than a protocol gap on our end.

**Ruled out: IMPI-vs-IMPU identity in To/From/Contact.** A plausible
alternative theory (raised externally) was that using the IMSI-derived
temporary IMPU (`sip:<IMSI>@<realm>`) in To/From/Contact — rather than an
MSISDN-based Public User Identity — could itself trigger the block, since
strictly that IMSI-based URI is the *private* identity (IMPI) and some HSS
implementations reject binding a Contact to it directly. Tested directly
with `--msisdn +91XXXXXXXXXX` (added as a CLI flag specifically for this:
see `ImsRegisterConfig::msisdn` in `gsm-sip-bridge/src/ims/mod.rs` — it only
changes To/From/Contact, the `Authorization` header's username stays
IMSI-based per TS 33.203 regardless). Result: **byte-identical** `421`/`403`
responses whether the identity is MSISDN- or IMSI-based, in both the plain
and full-`sec-agree` cases. This is expected in hindsight — `421 Extension
Required` and this `403` both fire before any identity/HSS lookup would
happen at all — but it's now empirically ruled out rather than assumed. (Our
own Airtel `200 OK` was already evidence this mechanism works on real
networks — TS 23.003 §13.4's "temporary Public User Identity" — but Vi could
have been stricter; it isn't, at least not at this stage of the exchange.)

See `gsm-sip-bridge/src/ims/mod.rs` module docs for the full design rationale
(including why this bypasses PJSIP's built-in digest auth entirely), and
`gsm-sip-bridge/src/ims/gm_ipsec.rs` for the Gm IPsec implementation.

## Phase 3: a real, answered call with recorded audio — findings

Implemented as `gsm-sip-bridge ims-call` (`gsm-sip-bridge/src/ims/call.rs`),
reusing Phase 2's registration and Gm IPsec session, then sending an INVITE,
exchanging RTP audio for a fixed duration, and recording what's received to
a WAV file (`gsm-sip-bridge/src/ims/rtp.rs`'s `WavWriter`).

**First attempt: PCMU-only offer got `488 Not Acceptable Here`.** VoWiFi/
VoLTE networks mandate AMR-WB and most won't fall back to G.711 — this
client only had PCMU (μ-law, no codec library needed) implemented at that
point.

**Second finding: the INVITE never rang the phone at all (`487 Request
Terminated` after ~23s, twice, no `180 Ringing`) until the Request-URI/To
carried `;user=phone`** (RFC 3261 §19.1.1 / TS 24.229) — without it, a bare
`sip:+91XXXXXXXXXX@realm` URI reached a terminating application server that
apparently couldn't resolve it to a real destination and just timed out
silently. Adding `;user=phone` fixed this immediately (response time for
the *next* rejection dropped from ~23s to under 1s, and it started
producing meaningful status codes instead of a blind timeout).

**Added real AMR-WB support** (`amr-sys`/`amr-safe` crates, FFI-wrapping
the system `vo-amrwbenc` (encode) and `opencore-amrwb` (decode) libraries —
opencore's own encoder was stripped for patent reasons years ago, hence two
separate libraries) rather than attempting a hand-written ACELP codec from
scratch, which would be an enormous undertaking with a real risk of not
interoperating correctly with a live network's bit-exact decoder. Confirmed
against the real library rather than assumed from the spec:
- `E_IF_encode`'s output is 1 TOC/header byte + packed speech data, and that
  TOC byte is bit-for-bit identical to one RFC 4867 octet-aligned ToC entry
  (`F` + `FT` + `Q` + padding) — so the RTP payload for one frame per packet
  is just `[CMR byte (0xF0, "no request")] + E_IF_encode's output, verbatim`.
  No manual bit-packing needed.
- Frame sizes per mode (e.g. mode 8 @ 23.85kbps = 61 bytes) were measured
  directly rather than trusted from memory/spec tables.
- Gated behind an `amr-linked` Cargo feature (default off, mirroring
  `pjsua-safe`'s `pjsip-linked`) so the workspace keeps building without
  `libvo-amrwbenc`/`libopencore-amrwb` installed; the `epdg-tunnel` image
  now installs the runtime shared libs unconditionally (see Dockerfile).
- SDP offers **both** PCMU and AMR-WB (`m=audio <port> RTP/AVP 0 96`,
  `a=fmtp:96 octet-align=1` — RFC 4867's default is bit-packed
  "bandwidth-efficient" mode, which isn't implemented, so this must be
  explicit) and the answer's chosen payload type picks which codec/framing
  `ims::call`'s RTP loop uses.

**Result: a real, answered call with real recorded audio**, against Airtel
India, to a second phone under the user's control:
```
180 Ringing (phone audibly rang)
200 OK (answered after ~16s)
call answered — recorded 120480 samples to /tmp/recordings/test-call.wav
```
The far end (interestingly) chose **PCMU**, not AMR-WB, despite both being
offered — plausibly because the far end wasn't itself on VoWiFi (e.g.
answered over regular cellular voice, with the network transcoding), or
simply preferred the first-listed codec. The recorded WAV
(120480 samples @ 8kHz = 15.06s, matching `--call-duration-secs 15`) has
genuinely varying RMS energy (~900–5200) for the first ~10 seconds — real
audio, not silence or decode noise — before dropping to near-silence for
the last ~5 seconds. AMR-WB's encode/decode path itself is implemented and
unit-tested against the real library (`amr-safe`'s tests, run with
`--features amr-linked`), but hasn't yet been exercised end-to-end against
a network that actually selects it — worth re-testing with a callee whose
own path is VoWiFi/VoLIP-only, if the "which codec gets picked" question
matters for future work.

See `specs`/the plan for the full design and rationale.

## Phase 4: always-on inbound VoWiFi-to-SIP bridge

Implemented per `specs/011-vowifi-sip-bridge/`. Where Phase 3's `ims-call` is
a one-shot, manually-invoked diagnostic that *places* a single outbound call
and exits, this phase adds two long-running, supervised processes that
*receive* inbound VoWiFi calls continuously and bridge them to the existing
SIP/PBX destination — the actual point of combining the GSM-SIP bridge with
the VoWiFi tunnel work above:

- **`vowifi-ims-agent`** ("Agent A") — runs inside the `ims` network
  namespace alongside the tunnel/Gm-IPsec state. Keeps the IMS-AKA
  registration (Phase 2) alive indefinitely (re-registering before it
  expires), answers inbound `INVITE`s from the carrier, and relays audio to
  Agent B over a dedicated `veth` link `entrypoint.sh` creates automatically
  once the tunnel is up.
- **`vowifi-sip-agent`** ("Agent B") — runs in the container's default
  namespace (LAN-reachable to the PBX). Registers to the configured `[sip]`
  destination and, for each call Agent A signals, places a matching call to
  the PBX plus a second call back to Agent A across the veth link, then
  bridges them via PJSIP's conference bridge.

Both are launched and supervised automatically by `entrypoint.sh` once the
tunnel is up — **if** a `gsm-sip-bridge` binary and a config file with
`[vowifi] enabled = true` are present in the container (see
`config.toml.example`'s `[vowifi]` section). Since the binary isn't baked
into this image, get it there the same way as Phase 2/3's manual build
(see above), just at a path the entrypoint checks for automatically:

```bash
docker run --rm -v "$PWD:/src" -w /src -e CARGO_TARGET_DIR=/tmp/bt rust:1-bookworm bash -c '
  apt-get update -qq && apt-get install -y -qq libasound2-dev pkg-config libudev-dev
  cargo build -p gsm-sip-bridge --bin gsm-sip-bridge
  cp /tmp/bt/debug/gsm-sip-bridge /src/gsm-sip-bridge-bookworm'
docker cp gsm-sip-bridge-bookworm epdg-tunnel:/usr/local/bin/gsm-sip-bridge
docker cp your-config.toml epdg-tunnel:/etc/gsm-sip-bridge/config.toml
rm gsm-sip-bridge-bookworm
docker restart epdg-tunnel
```

Check both agents came up:

```bash
docker exec epdg-tunnel pgrep -a -f vowifi-ims-agent
docker exec epdg-tunnel pgrep -a -f vowifi-sip-agent
docker logs epdg-tunnel --tail 50
```

If either binary/config isn't present when the tunnel comes up, the
entrypoint logs a clear note and skips agent supervision rather than
crash-looping — the tunnel itself still comes up normally, so Phase 2/3's
manual diagnostic commands keep working either way.

**Verification status**: the SIP/SDP protocol logic, the Agent A↔B control
protocol, the RTP relay, and the two-call PJSIP conference-bridging (via
`pjsua-safe`'s `Endpoint::pair_calls`) are unit- and integration-tested
without live hardware — see `specs/011-vowifi-sip-bridge/tasks.md`. A full
live inbound call (real carrier network, real PBX, real audio both ways)
has not yet been exercised end-to-end against a live network — do that per
`specs/011-vowifi-sip-bridge/quickstart.md` before relying on this in
production, the same way Phase 2/3's findings above were only trusted once
verified against a real network.

## Phase 5: strongSwan tunnel engine (specs/012-strongswan-epdg)

Phase 4's SWu-IKEv2 dialer proved the end-to-end pipeline works, but it has
no rekeying, no re-authentication, no dead-peer detection, and — worst of
all — deletes and recreates network namespace `ims` on every reconnect,
severing the veth link the Phase 4 agents depend on. This phase replaces
just the tunnel layer with **strongSwan** (the osmocom `strongswan-epdg`
fork, `jolly/work` branch — the wiki's "Option 2"), selectable at deploy
time via `TUNNEL_ENGINE` (`swu` default during the proving period,
`strongswan` to opt in) so the proven SWu path stays available as a
fallback. See `specs/012-strongswan-epdg/` for the full spec/plan/research.

### Architecture

```
charon (strongSwan)  ──IKEv2/IPsec──►  carrier's ePDG
     │  eap-sim-pcsc plugin
     ▼
pcscd  ──vpcd protocol (TCP :35963)──►  vowifi-usim-bridge (gsm-sip-bridge)
                                              │  AT+CSIM
                                              ▼
                                        SIM inside the EC200U modem
```

`vowifi-usim-bridge` is the piece that doesn't exist in any upstream
project: it implements vpcd's virtual-card wire protocol so `eap-sim-pcsc`
can run EAP-AKA against a SIM that's only reachable via `AT+CSIM`, with no
physical PC/SC reader. It also absorbs the same EC200U/SIM quirks the SWu
patch (above) had to work around — GET RESPONSE auto-chaining, SELECT
`P2=0x00` rejection, per-operator USIM AID differences — at the APDU
boundary, rather than patching strongSwan itself. The XFRM tunnel interface
(`tun23`, `if_id 23`) is pre-created by `docker/entrypoint.sh` inside netns
`ims` *before* charon ever starts, and only its address changes across
rekeys/reconnects — the namespace and interface themselves are never
deleted, which is the actual fix for Phase 4's biggest weakness.

### Build findings (Alpine/musl)

Both the fork and vsmartcard's `vpcd` build cleanly on Alpine/musl — no
patches needed, unlike the SWu dialer. Two build-time and one runtime
surprise, none anticipated by the original research:

- The fork needs `gperf` (generates a keyword-lookup table) beyond the
  plugin/crypto dev packages research anticipated — without it, `configure`
  fails outright (`GNU gperf required to generate ...`).
- vsmartcard's `ifd-vpcd` driver needs `src/vpcd` built first
  (`libvpcd.la` is a link dependency) — `make -C src/ifd-vpcd` alone fails.
  Explicit `--enable-serialdropdir=/usr/lib/pcsc/drivers/serial
  --enable-serialconfdir=/etc/reader.conf.d` avoids a doubled-prefix
  install path (`/usr/usr/lib/...`) that the auto-detected defaults produce
  when `--prefix` and `pkg-config`'s already-absolute `usbdropdir` combine.
- **The fork's own default `strongswan.conf` sets `load_modular = yes`**,
  which silently loads *zero* plugins — none of the fork's own generated
  per-plugin conf files set an explicit `load = yes`, and modular mode's
  default is "don't load" rather than upstream's usual "load unless
  disabled". Charon aborted at startup (`critical plugin 'charon' has
  unmet dependency: NONCE_GEN`) with no plugins actually active. Fixed by
  overriding `load_modular = no` in our own `charon-extra.conf` — the
  plugin set is already curated at `configure` time
  (`--disable-defaults` + explicit `--enable-*`), so the classic
  load-everything-unless-disabled model is the right one here regardless.
- `swanctl.conf` itself needs an explicit `include conf.d/*.conf` line
  (the fork's default is an empty file) or `swanctl --load-conns` finds
  nothing no matter what's dropped into `conf.d/`.
- Also needed beyond `--enable-openssl`: `--enable-random --enable-nonce
  --enable-hmac` — without them the same `NONCE_GEN`/`HASH_SHA1` unmet
  dependencies fire even with `load_modular = no`, since those primitives
  simply aren't compiled in at all.

Image size: 113MB → 116MB (+3MB) for both engines side by side — in line
with the "single-digit MB" estimate in research.md, nowhere near the
629MB→113MB scale of the original Alpine migration.

### Entrypoint findings

`ip netns list` appends `(id: N)` to a namespace's name once *any*
interface has ever lived in it (e.g. `ims (id: 1)`, not just `ims`) — an
exact-match `grep -qx "$NETNS"` existence check silently breaks the moment
the XFRM interface is created, tried `ip netns add` again on every
subsequent entrypoint run, and got a "File exists" error. Fixed by
checking `[ -e "/var/run/netns/$NETNS" ]` instead — a plain file-existence
check that doesn't depend on `ip netns list`'s variable formatting. Caught
by actually running the entrypoint's netns setup twice in a row, not by
reading the `ip-netns` man page.

### Live validation (no SIM/no live network egress from this environment)

With no physical modem/SIM available, an `IMSI` env var override was used
to exercise everything downstream of SIM auth. The rendered swanctl
connection was real enough that charon completed a genuine **IKE_SA_INIT**
negotiation with Airtel India's actual ePDG (resolved via the real
`epdg.epc.mnc094.mcc404.pub.3gppnetwork.org` FQDN) — DH group
renegotiation (peer rejected `ECP_256`, requested `MODP_8192`, matching the
same class of renegotiation the wiki's own transcript shows for a
different group pair), proposal selection, and an `IKE_AUTH` request
correctly carrying `CPRQ(... PCSCF4 PCSCF6)` — confirming the `p-cscf`
plugin is wired and requesting exactly what this feature needs. `IKE_AUTH`
then retransmitted with no reply from the ePDG, consistent with this
project's prior documented finding that ePDGs geoblock/rate-limit
non-operator source IPs (the same signature noted in Phase 1's
troubleshooting table above) — expected from a generic cloud egress IP,
not a defect. The 90×2s readiness-wait loop correctly timed out into its
`WARNING: could not confirm P-CSCF assignment` fallback (charon left
running, not treated as fatal), identical in shape to the SWu engine's own
equivalent fallback.

**Still needed before recommending `strongswan` as the default engine**
(specs/012-strongswan-epdg's LIVE-tagged tasks — real modem/SIM/carrier
access required, none of it automatable) — as things stood before the real
hardware became available:
- Trace `eap-sim-pcsc`'s real APDU sequence against the vpcd bridge and
  confirm/refute the ATR-is-opaque and SELECT-`P2` assumptions (T018).
- Full EAP-AKA success on both carriers via the bridge, no PC/SC reader
  (T019, SC-004).
- A forced-outage recovery drill and a 24h rekey soak proving the
  namespace/veth/agents genuinely survive unattended (T024/T025,
  SC-001/SC-002).
- An end-to-end inbound call bridged over the strongSwan tunnel (T027,
  SC-003) and one live spot-check of the unchanged `swu` path (T029).
- An engine-switch drill, `strongswan` → `swu` → `strongswan` on one image
  (T030, SC-005).

### Live hardware results (Quectel EC200 + Airtel SIM) and the default switch

All of the above except the 24h soak and the Vi-carrier check were run
against a real, connected Quectel EC200 modem and a live Airtel India SIM
(the tunnel's own actual traffic, not a sandbox stand-in). Full detail —
including 7 real bugs found and fixed along the way (EAP-AKA rejection
root-caused to strongSwan's own hardcoded 61xx expectation; lazy AID
discovery; the supervisor's permanent-give-up gap; `healthcheck.sh`'s
hardcoded `tun1`; a P-CSCF-regex/timestamp collision; `ims.updown`'s
unset-`$1` crash; a `pipefail`/`SIGPIPE` false-negative in the supervisor's
own SA check) — is in `specs/012-strongswan-epdg/tasks.md`'s per-task
notes and the corresponding commit history. Summary:

- **T018/SC-004 (Airtel)** — PASS, reproduced 3×: real IKE_SA + CHILD_SA
  established against Airtel's actual ePDG via the SIM inside the EC200U,
  zero PC/SC hardware.
- **T024/SC-002** — PASS, exceeding the bar: a genuine 60s outage (DPD
  activity confirmed in charon's log) was weathered via IKEv2
  retransmission with the IKE_SA never even torn down; netns/veth/agent
  PIDs identical before/during/after.
- **T027/SC-003 (immediate case)** — PASS: a real inbound call was
  answered and bridged well under 5s with two-way audio (AMR-NB↔PCMU
  transcoding), clean BYE teardown.
- **T030/SC-005** — PASS: a real `swu` → `strongswan` → `swu` round trip
  across one session, each direction a plain `.env` edit + `docker compose
  up --build`, nothing else.
- **Still outstanding**: SC-001 (the 24h soak) and the Vi-carrier half of
  SC-004 — no Vi SIM was available in that session; the soak needs its own
  dedicated window since it holds the container down for a full day.

**Given this, `TUNNEL_ENGINE` now defaults to `strongswan`** (previously
`swu`) — an explicit decision made with SC-001 and the Vi-carrier check
still open, not a claim that every proving criterion in spec.md has
passed. `swu` remains fully supported as an explicit fallback
(`TUNNEL_ENGINE=swu`) for exactly that reason. Revisit whether to retire
`swu` entirely once the soak and the Vi check are run.
