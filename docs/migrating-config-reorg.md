# Migrating to the reorganized `config.toml`

This guide covers a breaking restructuring of `config.toml` — no backward
compatibility is preserved, so update your config file using the mapping
below before upgrading.

## Overview

Two problems with the config file that grew organically across 18 features:

1. **`[audio]` mixed two different scopes.** Most of its keys only ever applied to
   the EC20's own USB sound device (circuit-switched calls); a few were genuinely
   shared with VoWiFi/VoLTE. The section name didn't distinguish them.
2. **`[vowifi]`/`[volte]` mixed "global" and "single-line-only" fields.** Several
   keys were silently ignored the moment more than one line was discovered, with no
   way to tell from the file alone whether a given key still did anything.

This release splits `[audio]` into `[audio]` (shared) and `[modem_audio]`
(circuit-switched only), and moves every line-specific VoWiFi/VoLTE setting into
`[[vowifi.line]]`/`[[volte.line]]` array entries — the top-level `[vowifi]`/
`[volte]` sections now hold only fields that are genuinely global. Pure
infrastructure that was always mechanically derived per line (veth interface
names/addresses, the ePDG network namespace name, the strongswan XFRM interface
name/id) is no longer configurable at all — it never needed to be.

## Configuration Mapping

### `[audio]` → `[audio]` / `[modem_audio]`

| Old key | New location | Notes |
|---|---|---|
| `[audio].profile` | `[audio].profile` | Unchanged — shared by every call path |
| `[audio].vad` | `[audio].vad` | Unchanged — shared |
| `[audio].snd_rec_latency_ms` | `[audio].snd_rec_latency_ms` | Unchanged — shared |
| `[audio].snd_play_latency_ms` | `[audio].snd_play_latency_ms` | Unchanged — shared |
| `[audio].rx_gain` | `[modem_audio].rx_gain` | Moved — circuit-switched only |
| `[audio].eec_mode` | `[modem_audio].eec_mode` | Moved — circuit-switched only |
| `[audio].tx_level` | `[modem_audio].tx_level` | Moved — circuit-switched only |
| `[audio].rt_audio_prio` | `[modem_audio].rt_audio_prio` | Moved — circuit-switched only |

### `[vowifi]`

| Old key | New location | Notes |
|---|---|---|
| `[vowifi].mcc` | `[[vowifi.line]].mcc` | Per line now. Omit entirely to keep auto-deriving from the SIM |
| `[vowifi].mnc` | `[[vowifi.line]].mnc` | Per line now |
| `[vowifi].modem_port` | `[[vowifi.line]].modem_port` | Per line now — pins that line to a specific modem |
| `[vowifi].imsi_override` | `[[vowifi.line]].imsi_override` | Per line now |
| `[vowifi].netns` | *(removed)* | Always derived internally from the line index — no replacement needed |
| `[vowifi].veth_local_addr` | *(removed)* | Always derived internally |
| `[vowifi].veth_peer_addr` | *(removed)* | Always derived internally |
| `[vowifi].veth_sip_iface` | *(removed)* | Always derived internally |
| `[vowifi].veth_ims_iface` | *(removed)* | Always derived internally |
| `[vowifi].strongswan_tun_iface` | *(removed)* | Always derived internally |
| `[vowifi].strongswan_if_id` | *(removed)* | Always derived internally |
| `[vowifi].vpcd_port` | `[vowifi].vpcd_port` | **Unchanged** — this is the one per-line infra field that stays a real, global config key (the base port of the shared vpcd reader; see `docs/configuration.md`) |
| everything else (`enabled`, `use_tcp`, `sec_agree`, `pcscf_source_path`, `control_port`, `wideband`, `apn`, `epdg_fqdn`, `epdg_ip`, `src_addr`, `keepalive_interval_sec`, `tunnel_engine`, `vpcd_host`, `max_lines`) | Unchanged | These were already global/shared across every line |

If you had a single working VoWiFi line via the old top-level `mcc`/`mnc`/
`modem_port`, move those exact values into one `[[vowifi.line]]` block:

```toml
# Before
[vowifi]
enabled = true
mcc = "404"
mnc = "094"
modem_port = "/dev/ttyUSB6"

# After
[vowifi]
enabled = true

[[vowifi.line]]
modem_port = "/dev/ttyUSB6"
mcc = "404"
mnc = "094"
```

Omitting the `[[vowifi.line]]` block entirely also works, if you're fine with
full auto-discovery (the SIM's mcc/mnc are then auto-derived and any
SIM-ready modem becomes a line).

### `[volte]`

| Old key | New location | Notes |
|---|---|---|
| `[volte].modem_port` | `[[volte.line]].modem_port` | Per line now |
| `[volte].iface` | `[[volte.line]].iface` | Per line now |
| `[volte].cid` | `[[volte.line]].cid` | Per line now — defaults to 3 when omitted |
| `[volte].apn` | `[[volte.line]].apn` | Per line now — defaults to `"ims"` when omitted |
| `[volte].pcscf` | `[[volte.line]].pcscf` | Per line now |
| `[volte].pcscf_port` | *(removed)* | Was already applied uniformly to every line; use `volte-bridge --pcscf-port` if you need a non-default value |
| `[volte].use_tcp` | *(removed)* | Was parsed but never actually consumed by the bridge (`volte::bridge` already hard-coded `true`) — a dead setting, not a behavior change |
| `[volte].sec_agree` | *(removed)* | Same as `use_tcp` above — dead, always `true` |
| everything else (`enabled`, `bridge_inbound`, `max_lines`, `pcscf_source_path`, `status_path`, `lock_path`) | Unchanged | These were already global/shared across every line |

Same migration pattern as `[vowifi]` — a single pinned line moves into one
`[[volte.line]]` block:

```toml
# Before
[volte]
enabled = true
bridge_inbound = true
modem_port = "/dev/ttyUSB2"
cid = 4
apn = "ims"

# After
[volte]
enabled = true
bridge_inbound = true

[[volte.line]]
modem_port = "/dev/ttyUSB2"
cid = 4
apn = "ims"
```

**`docker/entrypoint.sh`'s registration-only path** (`[volte].enabled = true`,
`bridge_inbound = false` — the legacy "just hold the registration open"
mode) is still single-line only: it honors at most the *first*
`[[volte.line]]` entry, not an arbitrary number of them. `volte-register`
(and `volte-pdn`, for teardown) resolve that one line — modem, cid, apn,
pcscf, iface, msisdn — from config the same way `volte-bridge`'s
auto-discovery does, as long as `--modem` is not passed explicitly. Passing
`--modem` on either command opts back out of config entirely, for manual
diagnostic use.

## Roll-back

The old binary/config shape is not preserved anywhere by this change — roll
back by restoring your pre-upgrade `config.toml` from version control and
reverting to the previous release.
