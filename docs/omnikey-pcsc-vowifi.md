# PC/SC Card-Reader-Backed VoWiFi Lines (OmniKey AG 3x21)

A VoWiFi line's SIM normally comes from a modem, bridged into pcscd's virtual
reader (`vpcd`) by `vowifi-usim-bridge` over `AT+CSIM`. This mode instead
seats the SIM directly in a physical PC/SC reader — validated against an
OmniKey AG 3x21 (USB `076b:3031`) — so strongSwan's `eap-sim-pcsc` plugin
talks to it straight through pcscd, no modem or bridge process involved at
all. See `specs/023-omnikey-pcsc-vowifi/` for the full spec/design.

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

A card-reader line has no modem to read `mcc`/`mnc`/`imsi_override` from at
startup, so they're mandatory config overrides instead — pin them once:

```bash
lsusb | grep -i omnikey        # confirm the reader is visible to the host
pySim-read.py -p 0             # per the osmocom wiki's "Getting IMSI" step
# → IMSI: 404940123456789 (example)
```

The first 5-6 digits are MCC+MNC (India: MCC is always 404/405; MNC is the
remaining 2-3 digits — pad to 3 digits, e.g. Vodafone Idea `43` → `"043"`).

If you're decoding `EF_IMSI` (`6F07`) by hand instead of using `pySim-read.py`,
note the file's length byte and BCD encoding include a leading parity/oddness
nibble that is **not** part of the IMSI — `pySim`'s `dec_imsi` drops it
(`swapped[1:]`, not `swapped[0:]`). Dropping this step silently produces an
IMSI with a bogus leading digit.

## Configuration

```toml
[vowifi]
enabled = true
tunnel_engine = "strongswan"   # required — swu has no PC/SC support

[[vowifi.line]]
pcsc_reader = true
imsi_override = "404940123456789"
mcc = "404"
mnc = "043"
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
   lines.
2. **Registration succeeds**: `docker logs <container> | grep -i "tunnel UP"`
   and `/var/log/strongswan.log`'s `IKE_SA established`/`EAP_AKA succeeded`
   for this line's IMSI.
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

Full walkthrough: `specs/023-omnikey-pcsc-vowifi/quickstart.md`.
