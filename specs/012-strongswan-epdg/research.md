# Phase 0 Research: strongSwan-Based ePDG Tunnel (Option 2)

Sources: the osmocom foss-ims-client wiki ("VoWiFI with Asterisk", Option 2 section), this
repo's prior findings (`docs/vowifi-epdg-research-notes.md`, the EC200U AT+CSIM patch in
`docker/patches/0001-ec200u-at-csim-fixes.patch`), and the current deployment surface
(`docker/Dockerfile`, `docker/entrypoint.sh`). Items marked **verify at implementation** are
assumptions to be confirmed against the real fork source / real hardware before being relied
on — the same discipline used for feature 011's research.

## 1. IKEv2 engine: the osmocom strongswan-epdg fork, `jolly/work` branch

**Decision**: Build the `strongswan-epdg` fork
(`https://gitea.osmocom.org/ims-volte-vowifi/strongswan-epdg.git`, branch `jolly/work`) from
source, configured with `--enable-eap-aka --enable-eap-sim --enable-eap-sim-pcsc
--enable-p-cscf` (plus `--enable-openssl` for the crypto backend). This is exactly the recipe
the osmocom wiki documents for ePDG client use.

**Rationale**:
- Upstream strongSwan cannot do this job alone: it has no P-CSCF configuration-attribute
  request support (the fork's `p-cscf` plugin adds `CPRQ(... PCSCF6/PCSCF4)`) and its
  `eap-sim-pcsc` plugin does not do USIM/AKA against a card — the wiki is explicit that "USIM
  authentication with a PCSC card reader is not supported by the official repository yet, so a
  fork with a special branch must be used."
- The fork is the community-proven path: the wiki's Option 2 walkthrough (IKE_SA established,
  P-CSCF received, EAP-AKA succeeded, MOBIKE negotiated, scheduled rekeying) is a transcript of
  this exact build against a live carrier.
- charon natively provides everything Option 1 lacks and this feature exists for: scheduled
  rekeying, IKE re-authentication (re-running EAP-AKA), DPD, MOBIKE, NAT-T keepalives, and
  infinite reconnect (`keyingtries = 0` + `charon.retry_initiate_interval`).

**Alternatives considered**:
- *Upstream strongSwan + out-of-tree plugins*: strongSwan plugins are not practically buildable
  out-of-tree, and we'd be re-deriving the fork's patches. Rejected.
- *Harden the SWu-IKEv2 Python dialer* (add rekey/reauth/DPD to it): reimplementing IKEv2
  lifecycle management in a single-file emulator script is strictly more work and less trustworthy
  than adopting the engine that already does it. Rejected.
- *IKEv2 in Rust* (e.g. building on existing crates): no mature EAP-AKA/ePDG-capable IKEv2 Rust
  stack exists; this would dwarf the rest of the project. Rejected.

**Verify at implementation**: the fork's current branch state (the wiki was last updated ~a year
ago; confirm `jolly/work` still carries the eap-sim-pcsc AKA patch and builds), and the exact
configure flag set it needs.

## 2. SIM access: vsmartcard `vpcd` virtual reader + a Rust APDU bridge subcommand

This is the feature's critical unknown (spec US2). The SIM is inside the EC200U modem — only
reachable via `AT+CSIM` — while the fork's `eap-sim-pcsc` plugin talks PC/SC to pcscd.

**Decision**: Run `pcscd` in the container with the **vpcd** virtual reader driver (from the
`vsmartcard` project, `frankmorgner/vsmartcard`), and implement a new `gsm-sip-bridge`
subcommand — working name `vowifi-usim-bridge` — that connects to vpcd as the "virtual card"
and services APDUs by forwarding them to the modem via the existing `AtCommander` /
`modules/usim.rs` machinery. Chain: `charon (eap-sim-pcsc) → libpcsclite → pcscd → vpcd (TCP,
default port 35963) → vowifi-usim-bridge → AT+CSIM → SIM`.

This was already the anticipated design: `docs/vowifi-epdg-research-notes.md` lists "the
strongSwan path (osmocom 'Option 2'), which needs a virtual PC/SC reader bridging the modem's
SIM to eap-sim-pcsc" as the known next step.

**Rationale**:
- Zero new C code owned by us: vpcd is maintained upstream; the bridge is pure-Rust `std::net` +
  the AT+CSIM code that already exists and is already proven against both carriers' SIMs
  (`usim.rs`: AID discovery via EF_DIR, SELECT, AUTHENTICATE, response parsing).
- Zero `unsafe`: the bridge is sockets + serial via existing crates, keeping the
  `gsm-sip-bridge/src` no-`unsafe` gate trivially satisfied.
- Testable without hardware: the vpcd wire protocol is a simple framed TCP protocol we can
  round-trip test in-process against a fake reader-side (constitution Principle I — real
  sockets, no transport mocks).

**The bridge is also the quirk adapter.** The EC200U/SIM quirks that broke the stock SWu script
(patch items 1–4) will hit any PC/SC client that assumes a well-behaved reader, including
`eap-sim-pcsc`. Rather than patching the strongSwan fork the way we patched `swu_emulator.py`,
the bridge normalizes at the APDU boundary, where all the knowledge already lives in Rust:
- **GET RESPONSE emulation**: the EC200U auto-chains GET RESPONSE internally and returns full
  data with `SW=9000`. A PC/SC client may drive classic T=0 (`61xx` then `00 C0 00 00 xx`). The
  bridge caches the last response and answers `GET RESPONSE` / `61xx` semantics locally.
- **SELECT parameter tolerance**: if the plugin issues `SELECT` with `P2=0x00` (rejected by
  these cards with `SW=6B00`), the bridge may rewrite to `P2=0x0C` — decided at implementation
  based on what the plugin actually sends (**verify at implementation** by tracing the plugin's
  APDU sequence).
- **AID correctness**: the bridge already knows how to discover the real USIM AID at runtime;
  if the plugin hardcodes a generic AID, the bridge can transparently redirect the SELECT to
  the discovered AID.
- **ATR synthesis**: `AT+CSIM` cannot fetch the card's real ATR, and vpcd requires one at
  power-on. The bridge serves a canned USIM ATR constant. **Verify at implementation** that
  `eap-sim-pcsc` treats the ATR as opaque (expected — it selects by AID, not by ATR parsing).

**vpcd protocol** (to be confirmed against vsmartcard source at implementation, but stable for
years): pcscd loads the vpcd IFD handler, which listens on TCP `35963`; the virtual card
connects to it. Messages are length-prefixed (2-byte big-endian). Single-byte control messages:
`0x00` power off, `0x01` power on, `0x02` reset, `0x04` request ATR; anything longer is a
command APDU expecting a response APDU back. Full details captured in
`contracts/vpcd-bridge-protocol.md`.

**Alternatives considered**:
- *Custom strongSwan plugin speaking AT+CSIM directly* (an `eap-simaka-atcsim` card backend):
  removes pcscd/vpcd from the stack but means writing and maintaining C inside a security
  daemon, duplicating the serial/APDU logic that exists in Rust, and re-fixing the EC200U
  quirks in a second language. Rejected — more moving parts *we own*, even if fewer processes.
- *Custom pcsc-lite IFD handler in C or as a Rust cdylib*: same C-maintenance objection; the
  Rust-cdylib variant would push FFI/`unsafe` into a new crate for no gain over vpcd's existing
  TCP boundary. Rejected.
- *`eap-simaka-sql`/static credentials*: AKA challenges are network-chosen at connect time;
  quintuplets cannot be precomputed without Ki/OPc, which cannot be extracted from a real SIM.
  Rejected (impossible).

## 3. Namespace and tunnel-interface plumbing: XFRM interface with a fixed `if_id`

**Decision**: The entrypoint (not the engine) owns the namespace lifecycle. At startup, before
initiating the tunnel: create netns `ims` (idempotently), create an XFRM interface (`ip link
add tun23 type xfrm if_id 23`), move it into the netns, bring it and `lo` up, install
`default`/`::/0` routes via it, and set `disable_policy=1` on the interface (the wiki-documented
requirement for received IPsec traffic not to be dropped). The swanctl connection pins
`if_id_in = if_id_out = 23`, `install_virtual_ip = no` in charon.conf, and an updown script
(`ims.updown` equivalent) flushes/adds `$PLUTO_MY_CLIENT` on the interface inside the netns on
`up-client`/`down-client` events.

**Rationale**:
- This is the structural fix for the biggest Option 1 defect: the SWu dialer deletes and
  recreates the netns on every reconnect, destroying the agents' veth end (`entrypoint.sh` has
  a dedicated half-pair rebuild workaround). With an XFRM interface, rekeys and even full
  reconnects only swap SAs/addresses — the netns, tun interface, and veth pair persist,
  directly satisfying FR-005 and US1.
- charon stays in the container's default namespace (where it can reach the ePDG over the LAN)
  while the encrypted payload surfaces only inside `ims` — the same inside/outside split as
  today, achieved with kernel XFRM routing instead of a userspace TUN pump. This also removes
  the SWu script's userspace ESP encapsulation from the datapath (kernel ESP instead).
- Both address families are handled by the same interface (`vips = ::` and/or `0.0.0.0` request
  both; remote_ts `::/0` + `0.0.0.0/0`), matching the current dual-family behavior.

**Alternatives considered**:
- *kernel-libipsec (userspace ESP) + TUN device*: works unprivileged-ish, but slower, and
  reintroduces a userspace packet pump exactly where Option 1 was weakest. Rejected.
- *Plain XFRM policies without an interface* (classic strongSwan): traffic selection by policy
  can't be confined to a namespace cleanly; the XFRM-interface + `if_id` pattern is what the
  osmocom wiki itself uses for this exact purpose. Rejected.

**Verify at implementation**: the wiki's `updown` script keys on `up-client-v6`; ours must
handle both `up-client` (IPv4) and `up-client-v6` since carriers differ in assigned family.
Also confirm Alpine's `iproute2` supports `type xfrm` (it does on mainline kernels ≥ 4.19;
container inherits the host 7.x kernel).

## 4. P-CSCF handoff to the agents: parse charon's filelog, write `/tmp/pcscf`

**Decision**: Configure charon's `filelog` to a known path (e.g. `/tmp/charon.log`, `ike = 1`,
`cfg = 1`, `flush_line = yes`). The entrypoint watches for the fork's `received P-CSCF server
IP <addr>` lines after initiating, prefers IPv4 (matching current agent behavior), and writes
the address to `/tmp/pcscf` — the exact file `vowifi-ims-agent` already reads
(`VowifiConfig.pcscf_source_path`). Refresh the file (and re-signal readiness) whenever the
connection re-establishes.

**Rationale**: keeps FR-006/FR-007 satisfied with zero agent changes, and is structurally
identical to how the entrypoint already extracts `P-CSCF IPV4 ADDRESS` from the SWu dialer's
stdout log — same technique, different regex. Readiness detection changes from grepping
`STATE CONNECTED` to grepping charon's `CHILD_SA ... established` line (or `swanctl
--list-sas` polling as a robustness backstop).

**Alternatives considered**:
- *`swanctl --list-sas` / VICI for P-CSCF attributes*: the P-CSCF config attribute is not
  reliably exposed through VICI in the fork; log parsing is what the community setup does.
  Chosen only as a readiness backstop, not the P-CSCF source. Re-evaluate at implementation if
  VICI turns out to expose it (**verify at implementation** — a VICI query would be sturdier
  than log regex).
- *Patch the fork to write the attribute to a file*: another carried patch to maintain;
  unnecessary. Rejected.

## 5. IMSI → EAP identity rendering at container start

**Decision**: The swanctl connection needs the NAI
`0<IMSI>@nai.epc.mnc<MNC>.mcc<MCC>.3gppnetwork.org` (and remote id `ims`) — IMSI is only known
at runtime from the SIM. Add a trivial read-only CLI helper (working name `gsm-sip-bridge
vowifi-imsi --modem <port>`) that prints the IMSI via the existing `AtCommander::query_imsi()`,
and have the entrypoint render `/etc/swanctl/conf.d/epdg.conf` from a template using it plus
`MCC`/`MNC` env vars (already padded to 3 digits by the existing entrypoint conventions).

**Rationale**: `query_imsi()` and its EC200U quirks already exist and are tested; parsing
`AT+CIMI` in bash would duplicate them. A print-only subcommand follows the existing
`config vowifi-enabled` precedent of "ask the binary instead of hand-parsing in shell".

**Alternatives considered**: bash `AT+CIMI` via `/dev/ttyUSB6` redirection (fragile, duplicated
parsing — rejected); baking IMSI into `.env` (breaks "SIM decides", wrong on SIM swap —
rejected, though an `IMSI` env override is kept as a debugging escape hatch, mirroring the
`imsi` override the `ims-*` subcommands already have).

## 6. Modem AT-port contention between charon's EAP-AKA and the rest of the system

**Decision**: The bridge opens the serial port **per powered session** (vpcd power-on →
power-off), not permanently, and retries with backoff when the port is busy (the `serialport`
crate opens with `TIOCEXCL`, so a concurrent holder fails fast and cleanly). Initial connect,
rekey-with-reauth, and AUTS resync are the only times APDUs flow, and each is a
seconds-long transaction — the same duty cycle the SWu dialer's `get_res_ck_ik()` had.

**Rationale**: FR-008 requires no deadlock/starvation with the CS daemon and the IMS agent's
own registration AKA (which uses the same port). All three users hold the port only for short
transactions and all already tolerate transient open failures with retries; adding a
cross-process lock manager would be new complexity for a collision window measured in seconds
per hour. If soak testing shows real collisions (SC-001 run), an advisory `flock` on the device
path inside `AtCommander::open` is the escalation path — a one-place change shared by all
in-repo users, still requiring no agent-code changes.

**Risk noted for the plan**: charon imposes an EAP timeout per round; if the port is held just
then, authentication fails and charon retries the whole IKE_AUTH — acceptable (retry converges)
but observable; log it explicitly in the bridge.

## 7. Engine selection and fallback

**Decision**: A `TUNNEL_ENGINE` environment variable consumed by `entrypoint.sh`:
`swu` (default during the proving period) or `strongswan`. Everything downstream of tunnel
establishment (P-CSCF file, netns name, veth creation, agent supervision, keepalive) is shared
code in the entrypoint; only the "establish tunnel and detect readiness" block branches. The
default flips to `strongswan` — and Option 1 removal becomes possible — only after SC-001..004
pass on live carriers.

**Rationale**: satisfies FR-001/US4 with a deploy-time toggle (SC-005: no rebuild), and keeps
the diff to the proven 011 behavior reviewable: with `TUNNEL_ENGINE=swu` the script path is
byte-for-byte today's flow.

**Alternatives considered**: config.toml `[vowifi].engine` key — rejected for now: the engine is
deployment infrastructure (like `MCC`/`MNC`/`EPDG_IP`, which are already env vars), not
application config the Rust binary needs to know about; keeping the binary engine-agnostic is
what makes FR-007 trivially true.

## 8. Building the fork and vpcd on Alpine/musl

**Decision**: Two new Docker build stages in `docker/Dockerfile`:
- **strongswan-builder** (alpine base): `build-base autoconf automake libtool pkgconf gettext-dev
  openssl-dev gmp-dev pcsc-lite-dev linux-headers`, clone the fork, `autoreconf -if`,
  `./configure --prefix=/usr --sysconfdir=/etc` + the plugin flag set from item 1 (plus
  disabling unneeded default plugins to keep the footprint down), `make install DESTDIR=...`,
  copy the install tree into the runtime stage.
- **vpcd-builder**: build vsmartcard's `virtualsmartcard` (autotools, needs `pcsc-lite-dev`),
  producing the vpcd IFD handler `.so` + its `/etc/reader.conf.d/vpcd` snippet.

Runtime stage additions: `pcsc-lite` (pcscd daemon + libs — `pcsc-lite-libs` is already
installed for pyscard), the charon/swanctl install tree, the vpcd driver, plus the config files
(`strongswan.d/` drop-ins: `charon-logging.conf`, `p-cscf.conf`, `install_virtual_ip = no`,
`osmo-epdg.conf load = no`) and the updown script. The SWu Python stage stays untouched while
the fallback exists (FR-009); its removal is a follow-up feature once strongSwan is proven.

**Rationale**: strongSwan builds cleanly on musl (Alpine has packaged it for years); building
from the fork's git tree needs the autotools bootstrap the wiki shows (`autoreconf -if`).
Keeping both engines honors the fallback requirement; image growth is bounded (charon + plugins
is single-digit MB, far below the 516 MB shaved off in commit 9b6a830).

**Verify at implementation**: fork build on musl specifically (the fork lags upstream; if it
trips on a musl-ism, options are a small carried patch — precedent: the SWu patch — or building
that one stage on Debian and shipping static; try musl first). Also pcscd + vpcd behavior under
musl (Alpine packages pcsc-lite, so low risk).

## 9. charon reliability configuration

**Decision** (initial values, tuned during soak):
- `keyingtries = 0` (retry forever) on the connection; `charon.retry_initiate_interval` for
  backoff between failed initiations; `start_action` via explicit `swanctl --initiate` from the
  entrypoint (so the entrypoint knows when to re-extract P-CSCF), with a supervisor loop
  re-initiating if the SA disappears.
- `dpd_delay = 30s` with default `dpd_timeout` semantics (IKEv2 retransmit-based).
- MOBIKE left enabled (default; the wiki transcript shows the ePDG supports it).
- NAT-T keepalives at charon's default 20 s (matches the current `KEEPALIVE_INTERVAL=20`
  duty cycle for the UDP path).
- Rekey margins at charon defaults — the ePDG dictates lifetimes via its own scheduling; charon
  rekeys before expiry, which is precisely the capability Option 1 lacked.
- Keep the existing entrypoint TCP keepalive to the P-CSCF port unchanged: it exercises the
  *inner* path (operator idle-timeout of the tunnel payload is a separate mechanism from IKE
  liveness, and ICMP is operator-filtered — prior finding).

**Alternatives considered**: `trap` policies for on-demand initiation — wrong model for an
always-on line. Rejected.

## 10. Testing strategy (constitution I/II alignment)

**Decision**:
- **Rust, no hardware**: vpcd framing codec (length-prefix encode/decode, control message
  parsing) unit-tested; full bridge loop integration-tested over a real in-process TCP socket
  pair — the test plays the pcscd/vpcd side (power on → ATR → SELECT → AUTHENTICATE → power
  off) against the bridge backed by the existing scripted-`AtCommander` test transport
  (`MockStream` precedent in `modules/at_commander.rs`, justified there already: the modem is
  hardware unavailable in CI). GET RESPONSE emulation and quirk rewrites get table-driven tests
  with real APDU byte fixtures (mirroring `sip_client.rs`'s wire-format fixture style).
- **`vowifi-imsi` subcommand**: reuses `query_imsi()`'s existing tests; the subcommand itself is
  argument plumbing tested via the existing CLI test pattern.
- **Shell/deploy**: `TUNNEL_ENGINE` branching kept small enough to review; live verification is
  operator-run per `quickstart.md` (tunnel up on both carriers, 24 h soak spanning a rekey,
  forced-outage recovery, end-to-end call on Airtel) — the same live-verification boundary every
  prior carrier-facing feature used. CI never needs charon/pcscd/a modem; `cargo test
  --workspace` stays green with no new feature gates (the bridge is pure std + existing deps).

No NEEDS CLARIFICATION items remain.
