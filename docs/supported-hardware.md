# Supported Hardware

Three physically distinct SIM sources work with this bridge today, and no
single one of them supports every call mode — the table below is the
quick answer; the sections after it cite where each cell's claim comes from.

## Modes, briefly

Three inbound call paths exist, corresponding to how the *carrier* chooses to
deliver a call (see `docs/architecture.md` for the full flows):

- **Circuit-switched (CS)** — classic 2G/3G GSM voice. The modem answers the
  call itself and exposes the audio over USB (ALSA); the bridge only relays
  RTP↔ALSA. This is the project's original mode (`specs/001`-`004`).
- **VoWiFi** — Wi-Fi Calling. The bridge dials an IKEv2/IPsec tunnel to the
  carrier's ePDG over the host's own IP connectivity (Wi-Fi/Ethernet/whatever
  route exists), then registers to IMS and answers calls over that tunnel
  (`specs/011`, `012`, `013`, `023`).
- **VoLTE** — the bridge performs its own IMS registration over the modem's
  **LTE data PDN** (the packet-switched/PS domain, in 3GPP terms — this is
  likely what "packet switched" refers to if you've seen that phrase used for
  this mode elsewhere) instead of relying on the modem's own internal VoLTE
  voice stack (`specs/015`-`020`). Confusingly, "packet-switched" is also
  sometimes used loosely to mean "not circuit-switched" generically, i.e. as
  a stand-in for either VoWiFi or VoLTE — this document always uses the three
  named modes above instead, to stay unambiguous.

VoWiFi and VoLTE share almost all of their registration/call-bridging code
(`ims::register_session`, `ims::agent`) — what differs is only *how the SIM
is reached* (a modem's `AT+CSIM`/LTE data path vs. a bare PC/SC reader) and
*how the network is reached* (an IPsec tunnel vs. an LTE data PDN).

## Compatibility matrix

| Hardware | Circuit-switched | VoWiFi | VoLTE |
|---|---|---|---|
| Quectel EC20 (`2c7c:0125`) | ✅ Validated (default role) | ⚙️ Supported by design, not independently live-verified on this exact model | ⚙️ Supported by design, not independently live-verified on this exact model |
| Quectel EC200/EC200U (`2c7c:0901`) | ❌ Not possible (no ALSA audio device) | ✅ Validated live | ✅ Validated live |
| PC/SC reader — OmniKey AG 3x21 (`076b:3031`) | ❌ Not possible (no modem/radio at all) | ✅ Validated live | ❌ Not possible (no LTE data path) |

✅ = actually run against real hardware and a live carrier network, cited
below. ⚙️ = the code path exists and isn't gated by model, but this
project's own hands-on testing happened on different hardware than this row.
❌ = structurally impossible given what the hardware exposes, not merely
untested.

## Quectel EC20 (`2c7c:0125`)

The original and still-default hardware target — [hardware-setup.md](hardware-setup.md)
covers one-time prep (USB audio enable, ModemManager disable).

- **Auto-discovery**: recognized by USB vendor/product ID `2c7c:0125`
  (`modules::discovery::KNOWN_DEVICES`), tagged `has_audio_capability: true`.
- **Default role**: circuit-switched. `RoleAssignment::from_probed` sends any
  audio-capable modem to the CS pool unless an explicit `[[vowifi.line]]`/
  `[[volte.line]]` override pins it elsewhere (audio capability decides the
  *default*, not a hard gate — an override always wins regardless of it).
- **Circuit-switched**: the primary validated mode — `specs/001-gsm-audio-echo`
  (single card) through `specs/004-multi-card-support` (concurrent multi-card),
  live-tested throughout this project's history. Also has `AT+QCFG="ims"`
  handling for the case where VoWiFi is enabled and needs the modem's own
  VoLTE/IMS stack forced off first (`docs/vowifi-bridge.md`'s "modem's own
  IMS/VoLTE stack" note, `v6.2.0` release notes).
- **VoWiFi/VoLTE**: nothing in the code prevents pinning an EC20 into either
  role via `[[vowifi.line]].modem_serial`/`modem_port` or the VoLTE
  equivalent — the shared `ims::register_session`/tunnel machinery only
  needs a working `AT+CSIM` passthrough and an LTE data path respectively,
  neither of which is EC20-specific. This project's own live VoWiFi/VoLTE
  validation, however, was done on EC200/EC200U (below) — if you run either
  mode on an EC20, you're the first to confirm it works on this exact model.

## Quectel EC200 / EC200U (`2c7c:0901`)

The hardware this project's VoWiFi and VoLTE work was actually developed and
live-tested against.

- **Auto-discovery**: USB vendor/product ID `2c7c:0901`, tagged
  `has_audio_capability: false` — this variant exposes no ALSA audio device
  at all (`modules::discovery::KNOWN_DEVICES`'s own comment: "the EC200
  tested here exposes no ALSA device, unlike the EC20").
- **Default role**: VoWiFi/VoLTE pool (audio-less modems never default to
  circuit-switched — there is no audio path to bridge).
- **Circuit-switched**: **not possible.** No ALSA device means no audio to
  relay; this model is never eligible for the CS pool regardless of config.
- **VoWiFi**: validated live and repeatedly — `docs/vowifi-epdg-research-notes.md`'s
  Phase 1-5 (the original protocol reverse-engineering, both Vodafone Idea
  and Airtel India SIMs), `specs/012-strongswan-epdg`'s "Live hardware
  results (Quectel EC200 + Airtel SIM)", and `specs/013-multi-card-vowifi`'s
  live-verification log (an EC200-class modem alongside others). The
  `docker/patches/0001-ec200u-at-csim-fixes.patch` fixes this model's
  specific `AT+CSIM` quirks (a stricter SELECT `P2` requirement, card-specific
  USIM AIDs, auto-chained `GET RESPONSE`, and a slow-AUTHENTICATE retry race)
  — needed for the `swu` tunnel engine's dialer; the default `strongswan`
  engine's `vowifi-usim-bridge` handles the same quirks natively in Rust.
- **VoLTE**: validated live — `specs/015-volte-host-ims` through
  `specs/020-volte-line-netns` were built and verified against a real
  EC200U with a Vodafone Idea (Vi) India SIM
  (`specs/017-volte-inbound-bridge/quickstart.md`: "Hardware: Quectel
  EC200U, Vi India SIM"). Notable hardware constraint found along the way:
  this model's `AT+CGDCONT` caps out at 8 parameters, so the
  P-CSCF-discovery/IM-CN-signalling parameters (TS 27.007 positions 9-10) a
  real VoLTE handset would set can't be requested this way — the bridge
  falls back to reading the P-CSCF from other sources (`--pcscf`,
  `AT+QCFG="pcscf"`) instead (`specs/017-volte-inbound-bridge/research.md`).

## PC/SC reader — OmniKey AG 3x21 (`076b:3031`)

Not a modem at all — a bare smart-card reader with no radio, added so a
VoWiFi line's SIM can sit directly in a card reader instead of a physical
modem (`specs/023-omnikey-pcsc-vowifi`, `docs/omnikey-pcsc-vowifi.md`).

- **Discovery**: not part of `modules::discovery`'s USB modem scan at all —
  configured explicitly per line (`[[vowifi.line]] pcsc_reader = true`,
  with mandatory `mcc`/`mnc`/`imsi_override`, since there's no modem to
  derive them from). Requires the runtime image's `ccid` USB driver
  (`docker/Dockerfile`) alongside the existing `vpcd` virtual reader used by
  modem-backed lines.
- **Circuit-switched**: **not possible.** No modem, no radio, no audio path
  — there is nothing for a CS call to be delivered over.
- **VoLTE**: **not possible.** VoLTE needs an LTE *data* PDN attach, which
  requires an actual cellular radio; a PC/SC reader is SIM-access only and
  depends entirely on the host's own Wi-Fi/Ethernet connectivity to reach
  anything.
- **VoWiFi**: validated live against a real OmniKey AG 3x21 and a Vodafone
  Idea India SIM, card-reader-only (no modem attached at all): ePDG tunnel
  established, IMS-AKA `REGISTER` got `200 OK`, the network's own `NOTIFY`
  confirmed an active registration, and a real inbound call was signaled and
  dialed into the PBX. Also validated in a *mixed* deployment (a modem-backed
  Airtel line running alongside this reader's Vodafone line): `eap-sim-pcsc`
  correctly discriminated between the two SIMs by IMSI rather than
  misusing either for the other's line — see
  `specs/023-omnikey-pcsc-vowifi/checklists/requirements.md`'s T026 entries
  for the full account, including two real bugs this testing found and
  fixed (a `READ RECORD` wrong-length case a real PC/SC reader hits that
  `AT+CSIM` never did, and `pcscd`'s virtual `vpcd` reader being wrongly
  required even when no modem-backed line exists).
- With more than one `pcsc_reader` line configured, each is matched to its
  own physical reader by its card's own IMSI (not "whichever reader is
  first") — see `docs/omnikey-pcsc-vowifi.md`'s troubleshooting section.

## Adding a new modem model

`modules::discovery::KNOWN_DEVICES` is the single place a new Quectel (or
other) USB modem's vendor/product ID and audio capability get registered —
add an entry there, note whether it exposes an ALSA device, and its default
CS-vs-VoWiFi/VoLTE role assignment follows automatically. Whether VoWiFi/VoLTE
actually *work* on it, though, is a hardware fact only real testing can
confirm — the AT+CSIM quirks the EC200U patch above fixes are the kind of
thing that varies per model/firmware, not something the code can predict.
