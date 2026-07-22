# EC20 VoLTE Setup Guide

> **This guide is about the modem's *internal* VoLTE stack — not the bridge's
> own.** It configures the EC20 so the *module* registers to IMS and the bridge
> receives decoded PCM audio over the AT/audio path.
>
> Since `specs/015-volte-host-ims`, there is a second, independent path: the
> bridge runs **its own** IMS registration over an LTE IMS PDN
> (`volte-register`), keeping SIP and media under software control. The two are
> mutually exclusive on one SIM — see [Operations →
> Host-side IMS over LTE](operations.md#host-side-ims-over-lte-volte).
>
> This guide was written for an **EC20 with an Airtel India SIM**; the
> host-side path was developed on an **EC200U with a Vodafone India SIM**,
> which is UNISOC-based and behaves differently (no QMI, AT only).

The Quectel EC20 module does not enable VoLTE by default. The `ROW_Generic_3GPP`
MBN profile locks the IMS voice flag to `0`, preventing VoLTE registration.
This guide documents the steps to enable VoLTE on an EC20 module with an
Airtel India SIM.

## Prerequisites

- Quectel EC20 LTE Cat 4 module
- AT command access via `/dev/ttyUSB2` (or the appropriate AT port)
- SIM card from a VoLTE-capable carrier (tested with Airtel India, MCC/MNC 40494)

## Problem

With the default `ROW_Generic_3GPP` MBN profile active, `AT+QCFG="ims"` returns
`+QCFG: "ims",1,0` — IMS framework enabled but VoLTE disabled. Attempting
`AT+QCFG="ims",1,1` returns `ERROR` and the second field remains `0`.

Without VoLTE, the module cannot place voice calls on LTE-only networks. It must
fall back to 2G (GSM) via `AT+QCFG="nwscanmode",1` for circuit-switched calls,
or rely on CSFB if 2G/3G coverage exists.

## Solution

### Step 1 — Deactivate the MBN Profile

```
AT+QMBNCFG="Deactivate"
```

This removes the carrier profile constraints that lock the IMS voice flag.

### Step 2 — Reboot the Module

```
AT+CFUN=1,1
```

Wait approximately 20 seconds for the module to re-enumerate on USB.

### Step 3 — Disable Echo (after reboot)

```
ATE0
```

### Step 4 — Switch to LTE Only

```
AT+QCFG="nwscanmode",3
```

Restricts the module to LTE. Verify registration and signal:

```
AT+CREG?        -> +CREG: 0,1  (registered)
AT+CSQ           -> +CSQ: 30,3  (good signal)
AT+QNWINFO      -> +QNWINFO: "FDD LTE","40494","LTE BAND 1",390
```

### Step 5 — Enable VoLTE

```
AT+QCFG="ims",1,1
```

The command may return `ERROR`, but verify the setting took effect:

```
AT+QCFG="ims"   -> +QCFG: "ims",1,1
```

Both fields should now be `1`.

### Step 6 — Verify

Place a test call. The call should connect over LTE without CSFB to 2G/3G.

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `AT+QCFG="ims",1,1` returns ERROR and second field stays `0` | MBN profile is still active | Run `AT+QMBNCFG="Deactivate"` and reboot |
| "Out of coverage area" on incoming calls | No VoLTE and no 2G/3G fallback | Follow this guide to enable VoLTE, or set `nwscanmode=1` for GSM-only |
| `+CREG: 0,2` (searching) after switching to LTE | No LTE coverage | Switch to auto mode: `AT+QCFG="nwscanmode",0` |
| Serial output garbled after reboot | Echo re-enabled on boot | Send `ATE0` after each reboot before issuing commands |

## Quick Reference

```
AT+QMBNCFG="Deactivate"
AT+CFUN=1,1
# (wait 20s for reboot)
ATE0
AT+QCFG="nwscanmode",3
AT+QCFG="ims",1,1
AT+QCFG="ims"              # verify: should show 1,1
```

## Notes

- Tested on EC20 with firmware exposing 4 MBN profiles: `ROW_Generic_3GPP`,
  `OpenMkt-Commercial-CU`, `OpenMkt-Commercial-CT`, `Volte_OpenMkt-Commercial-CMCC`.
- The CMCC VoLTE profile did **not** enable VoLTE for Airtel — it is
  carrier-specific to China Mobile.
- These settings persist across reboots. The procedure only needs to be run once.
- Carrier: Airtel India (40494), LTE Band 1 (EARFCN 390).
