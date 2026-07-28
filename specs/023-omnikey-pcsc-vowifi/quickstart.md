# Quickstart: Verify a PC/SC Card-Reader-Backed VoWiFi Line

Prerequisites: a PC/SC-visible smart-card reader (validated: OmniKey AG 3x21,
USB `076b:3031`) with a VoWiFi-provisioned SIM seated in it, PIN (CHV1)
disabled. `[vowifi].tunnel_engine = "strongswan"` (the default).

## 1. Read the SIM's identity once

```bash
lsusb | grep -i omnikey      # confirm the reader is visible to the host
pySim-read.py -p 0           # per the osmocom wiki's "Getting IMSI" step
# → IMSI: 404940123456789 (example)
```

The first 5-6 digits are MCC+MNC (India: MCC is always 404/405; MNC is the
remaining 2-3 digits — pad to 3 digits, e.g. Vodafone Idea `43` → `"043"`).

## 2. Add the line to `config.toml`

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
untouched (spec FR-005).

## 3. Bring the container up

```bash
docker compose -f docker/docker-compose.yml up --build
```

## 4. Confirm pcscd sees both the real reader and any vpcd slots

```bash
docker exec gsm-sip-bridge pcsc_scan -n
# or: docker exec gsm-sip-bridge opensc-tool --list-readers
```

Expect the OmniKey reader listed alongside any `Virtual PCD ...` (vpcd) slots
used by modem-backed lines, if configured.

## 5. Confirm registration succeeded

```bash
docker logs gsm-sip-bridge --tail 100 | grep -i "line.*tunnel UP\|EAP.*AKA\|IKE_SA established"
docker exec gsm-sip-bridge cat /var/log/strongswan.log | grep -E "EAP_AKA succeeded|IKE_SA .* established"
```

Cross-check against `vowifi-status` — the card-reader line must appear with
no visible difference from a modem-backed line's entry (spec FR-010/SC-005):

```bash
curl -s http://localhost:5076/status | jq '.lines'
```

## 6. Place/receive a test call

Same as any existing VoWiFi line — dial the SIM's number from another phone,
or use the bridged PBX extension, and confirm two-way audio.

## 7. Verify auto-recovery (spec FR-011)

With the line up, briefly remove and reseat the card (or unplug/replug the
reader):

```bash
docker exec gsm-sip-bridge cat /var/log/strongswan.log | tail -30   # watch for EAP-AKA retry
```

Expect the line to re-establish on its own within the existing
establish/steady-state supervision window (`line_supervisor.rs`), with no
container restart — this is existing, unmodified behavior (research.md §4),
not new code, so this step is about *confirming* it holds for the physical
reader, not exercising a new code path.

## 8. Mixed-deployment regression check

If a modem-backed line is also configured, confirm it still registers
independently and its behavior/logs are unchanged from before this feature
(spec SC-004) — restart the container once with both lines configured and
check both reach a registered state.
