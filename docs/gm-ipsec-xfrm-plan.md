# Plan: real Gm IPsec (kernel XFRM) for the Rust IMS-AKA client

**Status:** not started. Written as a handoff plan after `gsm-sip-bridge
ims-register --tcp --sec-agree` reached a real `401` + AKA challenge against
Airtel India (see `docker/epdg/README.md`, "Phase 2: IMS-AKA SIP REGISTER —
findings"). This is the one remaining piece to reach `200 OK`.

## Why this is needed

A wire capture of a working Asterisk registration (sysmocom's patched
PJProject, `res_pjsip_outbound_registration/volte.c`) against the same
Airtel SIM/tunnel shows: after the `401`+AKA challenge/response round, the
**second** (authenticated) REGISTER goes out on a **brand-new TCP connection
to the P-CSCF's negotiated `port-s`** (e.g. `6000`, from the `Security-Server`
response), not back to port `5060`. That only works because a real IPsec ESP
SA is already installed in the kernel by the time that connection is made —
the network is terminating actual IPsec on that port, not a plaintext
socket. Our Rust client currently resends the authenticated REGISTER over
the *same* plain socket to the *original* port, which the network correctly
treats as unprotected — hence a second `401` (fresh nonce) instead of `200`.

## Ground truth: what Asterisk actually does

Reference: `res/res_pjsip_outbound_registration/volte.c` +
`netlink_xfrm.c`/`.h` in sysmocom's Asterisk fork
(`https://gitea.sysmocom.de/sysmocom/asterisk.git`, branch `sysmocom/20.7.0`).

### 1. Key derivation (TS 33.203 Annex H) — no KDF, raw CK/IK

`volte_set_xfrm()` uses the AKA `CK`/`IK` (16 bytes each) **directly** as the
XFRM algorithm keys — no HMAC-based key derivation step:

- **Auth algorithm** (from the negotiated `alg=` in `Security-Server`):
  - `hmac-md5-96` → kernel `md5`, key = `IK` (16 bytes, 128 bits)
  - `hmac-sha-1-96` → kernel `sha1`, key = `IK` (16 bytes) + 4 zero bytes
    (160 bits total — sha1 wants a 20-byte key, IK is only 16)
- **Cipher algorithm** (from the negotiated `ealg=`):
  - `aes-cbc` → kernel `cbc(aes)`, key = `CK` (16 bytes, 128 bits)
  - `null` → kernel `cipher_null`, no key. **This is what Airtel actually
    negotiated in the captured trace** (`ealg=null` — integrity only, no ESP
    encryption) — so the first implementation only strictly needs the `null`
    cipher path; `aes-cbc` can follow once `null` works end-to-end.

### 2. SA/SP topology — 4 SAs + 4 policies, two logical tunnels

There are two independent logical "connections" carried over Gm IPsec, each
with its own SPI pair, matching what `Security-Client`/`Security-Server`
negotiate:

- **"c" pair** (`spi-c`/`port-c`): UE **client** role — UE sends requests to
  the P-CSCF's `port-c`.
- **"s" pair** (`spi-s`/`port-s`): UE **server** role — P-CSCF sends
  responses/requests back to the UE's `port-s`.

Each pair needs SAs in *both* directions (the local one we install and its
mirror so the kernel's outbound policy matches), so there are 4 `xfrm_sa_add`
calls total and 4 `xfrm_policy_add` calls (2 outbound `dir_in=false`, 2
inbound `dir_in=true`), keyed by `(src, dst, spi)`. See
`volte_set_xfrm()`/`volte_alloc_spi()` for the exact call sequence — it's
mechanical once the four `(local_addr, remote_addr, local_spi, remote_spi)`
tuples are assembled from the `Security-Client` values we sent and the
`Security-Server` values the network echoed back.

`xfrm_spi_alloc()` is called first (reqid `2342`, an arbitrary but fixed
constant reused across the codebase) to let the kernel pick our local SPIs
rather than us generating them ourselves — worth confirming whether picking
our own random SPIs (as `mod.rs`'s `SaProposal` already does) is equally
valid, since RFC 3329 doesn't mandate kernel-assigned SPIs. Simpler to keep
generating our own SPIs client-side (current behavior) unless testing shows
the network or kernel objects.

### 3. Header exchange already implemented

`Security-Client`/`Security-Server` parsing and the placeholder/real
`Authorization` header logic already exist in `gsm-sip-bridge/src/ims/mod.rs`
(see the current `run_register()` and `build_security_client_headers()`).
What's missing is *acting* on the `Security-Server` response instead of just
logging it.

## Proposed Rust implementation

### Approach: shell out to `ip xfrm`, not raw netlink

This codebase has a zero-unsafe policy for `gsm-sip-bridge/src` (enforced by
`make lint`'s unsafe-ratio check) and Phase 1's `entrypoint.sh` already
manages routing/netns state by shelling out to `ip`/`route` rather than
linking netlink bindings. Shelling out to `ip xfrm state add ...` / `ip xfrm
policy add ...` (both available via `iproute2`, already a Dockerfile
dependency) keeps that pattern and avoids pulling in raw netlink FFI or an
`unsafe`-heavy crate just for this one feature. All commands run inside the
`ims` netns (`ip netns exec ims ip xfrm ...`), consistent with how Phase 1
already runs its `ip`/`route` commands there.

Fallback if the CLI proves too fragile to script reliably (quoting binary
keys, error handling): the `rtnetlink` crate can drive XFRM netlink messages
in pure Rust without `unsafe`, at the cost of a new async dependency (the
`ims-register` path is currently synchronous, so this would need its own
small tokio runtime just for the netlink calls, or a sync netlink socket
opened directly). Prefer the `ip xfrm` shell-out first; only reach for this
if that's insufficient.

### New module: `gsm-sip-bridge/src/ims/gm_ipsec.rs`

- `parse_security_server(header: &str) -> SecurityServerParams` — extend the
  existing ad-hoc logging of `Security-Server` into real parsing (`alg`,
  `ealg`, `spi-c`, `spi-s`, `port-c`, `port-s`), mirroring
  `parse_digest_challenge`'s style in `sip_client.rs`.
- `derive_xfrm_keys(alg: &str, ealg: &str, ck: &[u8; 16], ik: &[u8; 16]) ->
  XfrmKeys` — pure function implementing the Annex H derivation above (easy
  to unit test without any kernel/netlink access, same pattern as
  `ims::digest`'s tests).
- `install_gm_sas(netns: &str, local_addr, remote_addr, our_proposal:
  &SaProposal, their_params: &SecurityServerParams, keys: &XfrmKeys) ->
  BridgeResult<()>` — shells out to `ip netns exec <netns> ip xfrm state
  add ...` (×4) and `ip xfrm policy add ...` (×4), matching the topology in
  §2. Needs `CAP_NET_ADMIN` in the container, already granted (Phase 1's
  compose file has `NET_ADMIN`).
- `remove_gm_sas(...)` for cleanup on error/shutdown, mirroring
  `xfrm_sa_del`/`xfrm_policy_del`.

### Changes to `run_register()` in `mod.rs`

After the first `401` + AKA success (current code around the
`AkaResult::Success` match arm):
1. Parse `Security-Server` from the response (already extracted as a string
   for logging — needs real parsing).
2. Call `install_gm_sas(...)`.
3. Open a **new** `SipTransport` connection to `(pcscf_addr, negotiated
   port-s)` instead of reusing the existing one to `pcscf_port` — this is
   the step our current code is missing entirely.
4. Build and send the authenticated REGISTER (already implemented) over
   *that* new transport.
5. On success, `RegisterOutcome::Success` as today. On failure, call
   `remove_gm_sas(...)` before returning the error (avoid leaking kernel SA
   state across repeated CLI invocations during testing).

## Testing strategy

- **Unit-testable without hardware/network:** `derive_xfrm_keys()` against
  known CK/IK test vectors (same style as `ims::digest`'s
  `ha1_uses_raw_res_bytes_not_hex` etc.) and `Security-Server` parsing.
- **Not unit-testable:** the actual `ip xfrm` invocations need
  `CAP_NET_ADMIN` and a real netns — this only gets exercised against the
  live tunnel + live Airtel network, same as the rest of Phase 2. No mocking
  planned; validate the same way the AKA challenge work was validated in
  this session (live run against `docker/epdg`'s tunnel container).

## Open risks

- **`ealg=null` means no encryption, but `alg=hmac-md5-96`/`hmac-sha-1-96`
  auth is still mandatory** — get the auth-only (ICV) path working first;
  don't block on cipher key derivation since the observed working exchange
  used `ealg=null`.
- **SPI ownership**: unclear whether the network cares if we generate our
  own SPIs (current `SaProposal` behavior) vs. letting the kernel allocate
  them (`xfrm_spi_alloc`, what Asterisk does). Should be safe either way
  since SPI is just an opaque identifier we control on our own SAs, but
  worth a note if SA installation is rejected.
- **Binding a new local port for the "s" pair**: the UE-side `port-s` we
  proposed in `Security-Client` needs an actual listening socket for the
  P-CSCF to connect back to (or send from) — Asterisk's transport layer
  handles this implicitly via PJSIP's multi-transport support; our client
  will need to explicitly bind/listen on that port inside the `ims` netns.
- **Root/capabilities**: `ip xfrm` needs `CAP_NET_ADMIN`, already granted to
  the Phase 1 container — but confirm it also covers `ip xfrm state`/`policy`
  subcommands specifically (should be the same capability, not verified in
  this session).

## References

- `docker/epdg/README.md` — "Phase 2: IMS-AKA SIP REGISTER — findings"
  (parent context, what's already working)
- `gsm-sip-bridge/src/ims/mod.rs` — current REGISTER flow,
  `SaProposal`/`build_security_client_headers`
- sysmocom Asterisk fork, `sysmocom/20.7.0` branch:
  - `res/res_pjsip_outbound_registration/volte.c` — `volte_set_xfrm()`,
    `volte_alloc_spi()`, `add_security_client_hdr()`
  - `res/res_pjsip_outbound_registration/netlink_xfrm.c`/`.h` — the
    libmnl-based netlink calls (`xfrm_sa_add`, `xfrm_policy_add`,
    `xfrm_spi_alloc`)
- TS 33.203 Annex H — Gm IPsec key derivation from AKA CK/IK
- RFC 3329 — `Security-Client`/`Security-Server`/`Security-Verify` header
  syntax (note: real implementations deviate from the strict grammar — see
  the minimal wire format already ported into `build_security_client_headers`)
