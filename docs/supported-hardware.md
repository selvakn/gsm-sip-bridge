# Supported Hardware

## At a glance

| Hardware | Circuit-switched calls | VoWiFi | VoLTE |
|---|---|---|---|
| Quectel EC20 | ✅ | ✅ | ✅ |
| Quectel EC200 / EC200U | ❌ (no audio output) | ✅ | ✅ |
| PC/SC card reader (e.g. OmniKey AG 3x21) | ❌ | ✅ | ❌ |

- **Circuit-switched** — classic GSM voice; the modem answers the call and
  sends audio over USB.
- **VoWiFi** — Wi-Fi Calling, over an IPsec tunnel to the carrier.
- **VoLTE** — voice over the modem's LTE data connection.

A card reader has no modem or radio at all, so it can only ever provide
VoWiFi (its SIM reaches the carrier over your host's own network
connection, not a cellular one) — circuit-switched calls and VoLTE both
need an actual modem.

This table is about how calls *arrive*. Where they go afterwards — an
external PBX, or an IP phone registered directly to the bridge
(`[sip_server].enabled`) — is device-independent: every mode above works with
either. See
[architecture.md](architecture.md#two-sip-side-topologies).

## Quectel EC20

The default choice — works for all three modes on the same device. See
[Setting up a modem](#setting-up-a-modem) below.

## Quectel EC200 / EC200U

Same setup as the EC20, minus the USB-audio step (this model has no audio
output, so it's always used for VoWiFi/VoLTE, never circuit-switched
calls).

## PC/SC card reader (OmniKey AG 3x21 and similar)

Lets a VoWiFi line's SIM sit in a card reader instead of a modem — useful
if you'd rather not dedicate a whole modem to one line, or don't have one
free. Setup: [docs/omnikey-pcsc-vowifi.md](omnikey-pcsc-vowifi.md).

## Setting up a modem

One-time preparation — do this before the first run.

**1. Host prerequisites**

- The modem should appear as `/dev/ttyUSB*` (check with `dmesg | grep
  ttyUSB`) — the `option`/`qcserial` kernel modules that provide this are
  normally auto-loaded.
- If running outside Docker, add your user to the `dialout` and `audio`
  groups: `sudo usermod -aG dialout,audio $USER`.

**2. Disable ModemManager** — it probes serial ports and corrupts AT
sessions:

```bash
sudo systemctl stop ModemManager
sudo systemctl disable ModemManager
```

**3. Enable USB audio** (EC20 only — needed for circuit-switched calls):

```bash
minicom -D /dev/ttyUSB2 -b 115200

AT+QCFG="USBCFG",0x2C7C,0x0125,1,1,1,1,1,0,1   # enable UAC
AT+CFUN=1,1                                     # reboot the module
```

After it reconnects, confirm with `arecord -l` / `aplay -l` — you should
see a card named "Android". Repeat for each module; the setting persists
across reboots.

**4. VoLTE (optional, per carrier)** — some modems ship with VoLTE locked
off. If calls aren't connecting over LTE, see the
[VoLTE unlock guide](ec20-volte-setup.md).

## Next steps

- Deploy the bridge — see the [Quick Start](../README.md#quick-start-docker-compose).
- Manage modems at runtime with `card list` / `restart` / `set-mode` — see
  [operations.md](operations.md).
