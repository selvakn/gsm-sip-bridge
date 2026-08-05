# Sample configurations

Starting points for `config.toml`, one per feature. Each is kept minimal —
only `[sip]` (required, no defaults) plus whatever keys differ from the
built-in default for that variant — with placeholder values
(`pbx.example.com`, `/dev/ttyUSBn`) to swap for your own. Copy the one
closest to your setup:

```bash
cp sample_configs/ec20-vowifi.toml config.toml
```

Every key is documented in full in
[`docs/configuration.md`](../docs/configuration.md); `config.toml.example`
at the repo root is the exhaustive, all-sections-commented reference these
samples are trimmed down from.

### Call path — pick one

| File | Scenario |
|---|---|
| [`ec20-circuit-switching.toml`](ec20-circuit-switching.toml) | Baseline: EC20 USB modem, circuit-switched (2G/3G/4G CSFB) calls bridged to a SIP/PBX trunk. No VoWiFi/VoLTE. |
| [`ec20-vowifi.toml`](ec20-vowifi.toml) | Single EC20 modem registering Wi-Fi Calling (VoWiFi) over an ePDG tunnel, bridged to the PBX. See [`docs/vowifi-bridge.md`](../docs/vowifi-bridge.md). |
| [`vowifi-only-cs-disabled.toml`](vowifi-only-cs-disabled.toml) | `[cs].enabled = false` — no plain circuit-switched calls at all, so the circuit-switched daemon does no background modem probing. Mixes an auto-discovered modem line with a PC/SC card-reader line, live-verified together. See `docs/configuration.md#cs` and `specs/026-disable-circuit-switched`. |
| [`ec200-volte.toml`](ec200-volte.toml) | Host-side VoLTE: the bridge runs its own IMS registration over an EC200(U)'s LTE data PDN and answers calls directly, bypassing the modem's internal VoLTE stack. See [`docs/ec20-volte-setup.md`](../docs/ec20-volte-setup.md) and [`docs/operations.md#host-side-ims-over-lte-volte`](../docs/operations.md#host-side-ims-over-lte-volte). |
| [`pcsc-vowifi.toml`](pcsc-vowifi.toml) | VoWiFi line backed by a SIM sitting in a PC/SC card reader (e.g. OmniKey AG 3x21) instead of a modem. See [`docs/omnikey-pcsc-vowifi.md`](../docs/omnikey-pcsc-vowifi.md). |
| [`multi-vowifi.toml`](multi-vowifi.toml) | Multiple VoWiFi lines behind one PBX registration: two modem-backed lines plus a card-reader line, each isolated in its own network namespace. See [`docs/vowifi-bridge.md`](../docs/vowifi-bridge.md) and `specs/013-multi-card-vowifi`. |
| [`sip-server-mode.toml`](sip-server-mode.toml) | No PBX: the bridge itself is the SIP registrar, and IP phones REGISTER to it directly. See `docs/configuration.md#sip_server` and `specs/024-sip-server-mode`. |

### Notifications — layer onto any call path above

| File | Scenario |
|---|---|
| [`sms-notifications.toml`](sms-notifications.toml) | Forward incoming SMS text to a Discord webhook. |
| [`critical-alerts.toml`](critical-alerts.toml) | Discord alerts for registration loss, tunnel failure, module lifecycle problems, and missed calls. See `specs/022-discord-critical-alerts`. |

These sections are independent of the call path — merge the relevant
section(s) into whichever call-path sample you started from.

Common to all:

- Secrets use the `env:VAR_NAME` syntax — set the referenced environment
  variable rather than writing the plaintext value into the file.
- Everything not shown (`[logging]`, `[bridge]`, `[metrics]`, `[modules]`,
  `tunnel_engine`, `max_lines`, `listen_addr`, `min_expires`, ...) is left at
  its built-in default; add a key only once you need to change it — see
  `config.toml.example` for the full, all-defaults-commented reference.
- None of these enable `[outbound]` (dial-out from the SIP side) — see
  `config.toml.example` for that, which pairs with `sip-server-mode.toml` or
  a PBX-fed `[sip]`.
