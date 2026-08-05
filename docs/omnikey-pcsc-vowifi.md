# PC/SC Card-Reader-Backed VoWiFi Lines (OmniKey AG 3x21)

A VoWiFi line's SIM normally comes from a modem, bridged into pcscd's virtual
reader (`vpcd`) by `vowifi-usim-bridge` over `AT+CSIM`. This mode instead
seats the SIM directly in a physical PC/SC reader — validated against an
OmniKey AG 3x21 (USB `076b:3031`) — so strongSwan's `eap-sim-pcsc` plugin
talks to it straight through pcscd, no modem or bridge process involved at
all. See `specs/023-omnikey-pcsc-vowifi/` for the full spec/design.

This covers **both** halves of a VoWiFi line: the ePDG tunnel (strongSwan +
`eap-sim-pcsc`, spec 023's original scope) and IMS-AKA SIP registration
(`vowifi-ims-agent`, added 2026-07-28) — the latter needed its own PC/SC path
(`modules::pcsc_card::PcscTransport`, implementing the same
`modules::usim::ApduTransport` trait `AtCommander` does for a modem line)
since `ims::register_session` talked to the SIM only via `AT+CSIM` until
then. An IMEI is a device identity rather than card data, so there is
genuinely nothing to read one from here: one is auto-generated (deterministic
per IMSI, Luhn-valid per TS 23.003 Annex A — not a real registered device
identity) unless `imei_override` is set explicitly. Everything that *is* on
the card — the IMSI, and the MCC/MNC derived from it plus `EF_AD` — is read
from the card, so no PLMN needs configuring.

With more than one `pcsc_reader` line (each with its own real reader),
IMS-AKA registration picks the reader whose card's own `EF_IMSI` matches
that line's configured `imsi_override`, the same disambiguation
`eap-sim-pcsc` already does for the tunnel side — it does not simply grab
"the first" reader.

## Requirements

- `[vowifi].tunnel_engine = "strongswan"` (the default). The `swu` engine has
  no PC/SC support — `supervise` refuses to start if a `pcsc_reader` line is
  configured under it.
- The reader's `ccid` USB driver in the runtime image (`docker/Dockerfile`) —
  self-registering via udev hotplug, alongside the existing `vpcd` driver
  used by modem-backed lines. No `/etc/reader.conf.d` entry is needed for it
  (unlike `vpcd`, which has a static one).
- The SIM's PIN (CHV1) disabled — this project has no PIN-verification code
  anywhere; it has always relied on a modem's baseband having already
  unlocked the SIM, which a bare PC/SC reader has no equivalent of.

## Reading the IMSI once

`imsi_override` is the one identity field a card-reader line must configure.
Not because the IMSI is unreadable — `EF_IMSI` is read straight off the card
over PC/SC, and `PcscTransport::connect` does exactly that on every candidate
reader — but because it is the **reader-to-line binding key**: which physical
card a line owns has to be known before any card session is opened, and
strongSwan's `eap-sim-pcsc` needs it in the rendered NAI at orchestration
time for the same reason.

`mcc`/`mnc`, by contrast, are optional here, exactly as on a modem line: both
derive from the card's own files — `EF_IMSI` for the digits and `EF_AD`
(`6FAD`, TS 31.102 §4.2.18 byte 4) for whether the MNC is 2 or 3 digits long.
`vowifi-plmn --pcsc-imsi <IMSI>` reads both over PC/SC with no modem
involved, and both `supervise` (for the ePDG FQDN) and `vowifi-ims-agent`
(for the IMS realm) call that path when the line leaves them unset. Prefer
leaving them unset: a hand-written pair that gets the MNC length wrong
silently builds the wrong ePDG FQDN *and* the wrong IMS realm.

The one case that still needs them pinned is a card whose `EF_AD` omits the
MNC-length byte (some legacy 2G SIMs). The modem path falls back to the
serving PLMN from `AT+COPS` there; a reader has no radio and so no serving
network to ask, so the line fails at startup with an error saying to set
`mcc`/`mnc` explicitly.

To read the IMSI (and MCC/MNC/carrier) off every attached reader in one shot,
run `gsm-sip-bridge pcsc-list` **wherever pcscd is already running and can see
the reader** — same requirement as the `opensc-tool --list-readers` check
below, since `pcsc-list` opens a PC/SC context against the running pcscd
resource manager (`SCardEstablishContext`) rather than starting one of its
own. In this project's deployment, that's pcscd running inside the container
(`docker/Dockerfile`'s `pcsc-lite`/`ccid` packages), so the usual invocation
is:

```bash
docker exec <container> gsm-sip-bridge pcsc-list
# reader                                   status               imsi              mcc   mnc   carrier
# ----------------------------------------------------------------------------------------------------
# OMNIKEY AG SmartCard Reader 3x21 00 00   ok                   404940123456789  404   094   Vodafone Idea (India)
# OMNIKEY AG SmartCard Reader 3x21 01 00   no card              -                 -     -     -
```

(Running it directly on the bare host only works if you've separately set up
pcscd + the `ccid` driver there yourself — this project's own pcscd instance
lives in the container.) It needs no PIN or prior configuration — it's meant
to be the first thing you run against a new reader, before writing
`imsi_override` into `config.toml`. Each row's `mcc`/`mnc` come from the same
`EF_IMSI`/`EF_AD` read `vowifi-plmn --pcsc-imsi` does; `carrier` is a lookup
against `https://mcc-mnc-lookup.com`'s public API and is left blank (not
fatal) if that lookup fails or the container has no internet access. With
more than one reader attached, this is what tells you which reader's card is
which line's `imsi_override` — `Virtual PCD ...` (vpcd) slots are never
listed, since those are the modem-backed lines' virtual reader, not a card to
bind to.

`pySim-read.py -p 0` (per the osmocom wiki's "Getting IMSI" step) is the
fallback if `pcsc-list` isn't available (e.g. reading a card outside this
project entirely). If you're decoding `EF_IMSI` (`6F07`) by hand instead,
note the file's length byte and BCD encoding include a leading parity/oddness
nibble that is **not** part of the IMSI — `pySim`'s `dec_imsi` drops it
(`swapped[1:]`, not `swapped[0:]`), and so does `pcsc-list`/`PcscTransport`'s
own decoder (`modules::usim::read_imsi`). Dropping this step silently
produces an IMSI with a bogus leading digit.

## Configuration

```toml
[vowifi]
enabled = true
tunnel_engine = "strongswan"   # required — swu has no PC/SC support

[[vowifi.line]]
pcsc_reader = true
imsi_override = "404940123456789"
# mcc/mnc omitted on purpose — derived from the card's EF_IMSI + EF_AD.
# Pin them only if this card's EF_AD has no MNC-length byte.
```

To check what the card resolves to before starting anything:

```bash
gsm-sip-bridge vowifi-plmn --pcsc-imsi 404940123456789
# → 404 094
```

Existing `[[vowifi.line]]` entries for modem-backed lines, if any, are left
untouched and continue to work exactly as before — a card-reader line and
modem-backed lines share the same `[vowifi].max_lines` bound but otherwise
resolve, register, and fail independently (see
`specs/023-omnikey-pcsc-vowifi/spec.md` FR-005/FR-006/FR-007).

## Verification checklist

1. **pcscd sees the reader**: `docker exec <container> opensc-tool
   --list-readers` (Alpine has no `pcsc-tools`/`pcsc_scan` package — use
   `opensc-tool`, or a PC/SC client of your own) should list the OmniKey
   reader, alongside any `Virtual PCD ...` (vpcd) slots used by modem-backed
   lines. `docker exec <container> gsm-sip-bridge pcsc-list` covers the same
   check plus the card's IMSI/MCC/MNC in one command (vpcd slots are filtered
   out, so it only ever lists real readers).
2. **Tunnel established**: `docker logs <container> | grep -i "tunnel UP"`
   and `/var/log/strongswan.log`'s `IKE_SA established`/`EAP_AKA succeeded`
   for this line's IMSI.
2b. **IMS registration succeeds**: `docker logs <container> | grep -i
   "registered, listening for inbound calls"` (from `vowifi-ims-agent`) —
   this is a second, independent SIM access path from the tunnel (see the
   note at the top of this doc); a tunnel UP does not by itself mean this
   line can carry calls.
3. **Status parity**: `vowifi-status` and `curl http://localhost:9091/metrics
   | grep gsm_sip_bridge_vowifi` — the card-reader line's `card_id` (e.g.
   `pcsc0`) is just another `module` label value; no field or label
   distinguishes it from a modem-backed line (spec FR-010/SC-005).
4. **Test call**: dial the SIM's number, confirm two-way audio.
5. **Auto-recovery**: briefly remove and reseat the card (or unplug/replug
   the reader) with the line up; it should re-establish on its own within the
   existing establish/steady-state supervision window
   (`gsm-sip-bridge/src/supervise/line_supervisor.rs`) — this is existing,
   unmodified behavior, not new code (see
   `specs/023-omnikey-pcsc-vowifi/research.md` §4).
6. **Mixed-deployment regression**: if a modem-backed line is also
   configured, confirm it still registers independently and its
   behavior/logs are unchanged from before this feature.

## Troubleshooting

**Never test by starting a second bridge container alongside a running one.**
The compose deployment uses `network_mode: host`, so a second instance shares
the host's UDP 500/4500 (IKE), the vpcd port, the metrics port and the per-line
SIP ports. The failures this produces look nothing like port conflicts and
will send you chasing the carrier instead:

- IKE_SA_INIT appears to go unanswered (retransmits, then silence) because the
  ePDG's reply is delivered to the *other* container's charon, which discards
  it as an unknown SPI. Verified on 2026-07-28: an ePDG that looked completely
  silent answered a standalone IKEv2 probe from the same host instantly, and
  the line established normally once run on its own.
- `pjsua_transport_create returned 120098` — PJ's error base plus `EADDRINUSE`;
  the SIP transport port is already taken.
- `pcscd's vpcd reader never came up on 127.0.0.1:15963 (DriverBindFailed)`.

Stop the running container first, or give the test instance its own
`[vowifi].vpcd_port`, `[metrics].port` and SIP ports.

To confirm an ePDG is reachable independently of this stack, send it a bare
IKE_SA_INIT on UDP/500 — any reply (even `NO_PROPOSAL_CHOSEN` or
`INVALID_KE_PAYLOAD`) proves reachability and rules out the carrier.

A card-reader-only deployment (no modem-backed lines at all) needs no vpcd
virtual reader, and `supervise` no longer provisions or waits for one — expect
`started shared pcscd; no vpcd reader (all N line(s) are card-reader-backed)`.

Note that `epdg.epc.mnc043.mcc404.pub.3gppnetwork.org` resolves to more than
one address and DNS rotates the order; the line uses whichever `dig` returns
first, so successive restarts may legitimately connect to different ePDG IPs.

Full walkthrough: `specs/023-omnikey-pcsc-vowifi/quickstart.md`.
