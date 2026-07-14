# Hardware Setup (Quectel EC20)

One-time preparation for each EC20 module, plus host-side prerequisites.
Do this before the first run — the bridge cannot see the modem's audio
until USB Audio Class is enabled.

## Host prerequisites

- One or more Quectel EC20 USB modems with active SIM cards.
- The `option` and `qcserial` kernel modules (normally auto-loaded; the
  modem should appear as `/dev/ttyUSB*` — check `dmesg | grep ttyUSB`).
- Your user in the `dialout` and `audio` groups (not needed for the
  privileged Docker deployment):

  ```bash
  sudo usermod -aG dialout,audio $USER
  ```

## Disable ModemManager

ModemManager probes `ttyUSB*` ports for modems, which corrupts AT
sessions. The bridge warns at startup if ModemManager is active. To fix
permanently:

```bash
sudo systemctl stop ModemManager
sudo systemctl disable ModemManager
```

## Enable USB Audio Class (UAC)

The EC20 does not expose its call audio over USB by default. Enable it
once per module:

```bash
minicom -D /dev/ttyUSB2 -b 115200

# Enable UAC (last parameter = 1)
AT+QCFG="USBCFG",0x2C7C,0x0125,1,1,1,1,1,0,1

# Reboot module
AT+CFUN=1,1
```

Verify the audio device appears after the module re-enumerates:

```bash
arecord -l    # Should show a card named "Android"
aplay -l      # Same card for playback
```

Repeat for each EC20 module. The setting persists across reboots.

## Enable VoLTE (optional, per carrier)

Out of the box the EC20's `ROW_Generic_3GPP` MBN profile locks VoLTE off,
so on LTE-only networks the module must fall back to 2G/CSFB for voice.
If you want calls to connect over LTE, follow the
[EC20 VoLTE setup guide](ec20-volte-setup.md).

## Next steps

- Deploy the bridge — see the [Quick Start in the README](../README.md#quick-start-docker-compose).
- Tune per-slot network mode (`2g`/`3g`/`4g`/`auto`) at runtime with
  `card set-mode` — see [operations.md](operations.md).
