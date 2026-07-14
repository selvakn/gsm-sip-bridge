# Configuration Reference

The bridge reads a single TOML configuration file specified via `--config`.

## Sections

### `[logging]`

| Key | Type | Default | Description |
|---|---|---|---|
| `level` | enum | `info` | `trace`, `debug`, `info`, `warn`, or `error`. Applies to the daemon and both `vowifi-*-agent` subcommands. Overridden by the `RUST_LOG` env var if set, or by `-v`/`--verbose` on the CLI (always forces `trace`). |

### `[sip]` (required)

| Key | Type | Default | Description |
|---|---|---|---|
| `server` | string | (required) | PBX hostname or IP |
| `port` | integer | 5060 | SIP port |
| `username` | string | (required) | SIP account |
| `password` | string | (required) | Supports `env:VAR` syntax |
| `transport` | enum | `udp` | `udp`, `tcp`, or `tls` |
| `local_port` | integer | 5060 | Fixed local port |
| `display_name` | string | username | Callee display |
| `tls_verify` | enum | `strict` | `strict` or `skip` |

### `[bridge]`

| Key | Type | Default | Description |
|---|---|---|---|
| `sip_destination` | string | `""` | Empty = DID passthrough |
| `sip_dial_timeout_sec` | integer | 30 | Range: 5-120 |

### `[sms]`

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | true | Disable SMS monitoring |
| `discord_webhook_url` | string | `""` | Supports `env:VAR` syntax |
| `db_path` | string | `/var/lib/gsm-sip-bridge/store.db` | Store path |

### `[metrics]`

| Key | Type | Default | Description |
|---|---|---|---|
| `port` | integer | 9091 | Metrics HTTP server port |

### `[modules]`

| Key | Type | Default | Description |
|---|---|---|---|
| `retry_interval_sec` | integer | 30 | Range: 5-600 |
| `max_concurrent` | integer | 8 | Range: 1-8 |

### `[audio]`

Latency and audio-quality tuning for the circuit-switched (EC20 USB audio)
path. All keys are optional. The section is read at startup; changes
require a process restart. See `docs/audio-tuning-log.md` for the empirical
history behind the modem-side defaults.

| Key | Type | Default | Description |
|---|---|---|---|
| `profile` | enum | `lan` | Latency preset: `lan` or `wan` (see below) |
| `vad` | boolean | `true` | PJMEDIA voice activity detection / noise suppression on the capture path (GSM → SIP). Set `false` only to diagnose audio issues (raw passthrough). |
| `rx_gain` | integer | unset (firmware default) | EC20 downlink digital gain (`AT+QRXGAIN`, 0–65535), applied at module init. Controls how loud SIP audio sounds to the GSM caller. |
| `eec_mode` | integer | unset (firmware default, 12543) | EC20 echo-canceller mode word (`AT+QEEC=2,<val>`). `0` disables all EC — recommended for USB audio bridges, which have no acoustic echo path. |
| `tx_level` | float | `1.0` | PJSUA conference-bridge software gain on the GSM → SIP path (`1.0` = unity, `0.5` ≈ −6 dB, `2.0` = +6 dB). |
| `snd_rec_latency_ms` | integer | `150` | ALSA capture ring-buffer depth (GSM → SIP), 20–2000 ms. Raise if logs report `alsa_capture_overrun`. |
| `snd_play_latency_ms` | integer | `150` | ALSA playback ring-buffer depth (SIP → GSM), 20–2000 ms. Raise if logs report `alsa_playback_underrun`. |
| `rt_audio_prio` | integer | `0` (off) | `SCHED_FIFO` priority (1–99) for PJMEDIA's sound-device threads; prevents XRUNs/choppy audio under load. Requires `CAP_SYS_NICE`; best-effort. |

#### Latency profiles

The `profile` preset tunes two independent latency contributors:

- **Audio ring buffer** (`ring_capacity`) — the queue between the ALSA
  capture thread and the PJSIP media thread. Oversizing this lets stale
  audio queue up invisibly, adding hundreds of milliseconds of delay.
- **PJSIP jitter buffer** (`jb_init_ms`, `jb_min_pre`, `jb_max_ms`) —
  PJMEDIA's adaptive jitter buffer. Without a hard ceiling it ratchets
  upward on any CPU spike and never recovers.

| Setting | `lan` (default) | `wan` |
|---|---|---|
| `ring_capacity` | 4 frames (80 ms slack) | 16 frames (320 ms slack) |
| `jb_init_ms` | 20 ms | 60 ms |
| `jb_min_pre` | 1 frame | 2 frames |
| `jb_max_ms` | 40 ms (hard cap) | 200 ms (hard cap) |

**Use `lan`** when the SIP server is on the same machine or local network.
There is no packet jitter on this path, so the smallest ring and tightest
jitter buffer caps give the best end-to-end latency. Expected one-way delay
through the bridge: ~120–150 ms (dominated by the GSM air interface, which
is fixed at ~80–110 ms).

**Use `wan`** when pointing `sip.server` at an internet SIP trunk. The
wider ring and larger jitter buffer absorb burst packet loss and higher RTT
without causing audible glitches. Expected one-way delay: ~180–340 ms
depending on trunk latency.

### `[scheduled_restart]`

Preventive nightly card-restart cycle: the daemon walks every known slot in
ascending order and reboots each modem (`AT+CFUN=1,1`) with a randomized
gap between cards. Cards on an active call are deferred to the end of the
cycle; a manual `card restart` issued during a cycle takes precedence. See
`specs/010-scheduled-card-restart/quickstart.md` for full details.

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `true` | Master switch |
| `cron` | string | `0 1 * * *` | Standard 5-field cron expression in system local time. Invalid expressions disable the scheduler without aborting the daemon. |
| `start_jitter_seconds` | integer | `600` | Symmetric jitter on the cycle start time. Range 0–86400; 0 disables. |
| `inter_card_gap_seconds` | integer | `30` | Base wait between consecutive per-card restarts. Range 0–3600. |
| `inter_card_gap_jitter_seconds` | integer | `15` | Symmetric jitter on the inter-card gap; must be ≤ `inter_card_gap_seconds`. |

### `[resilience]`

Controls automatic card recovery behavior. All keys are optional; defaults cover typical homelab use.

| Key | Type | Default | Description |
|---|---|---|---|
| `initial_backoff_sec` | integer | 5 | Delay before the first recovery retry (seconds). Range: 1-600 |
| `max_backoff_sec` | integer | 120 | Maximum backoff delay after repeated failures (seconds). Range: 1-3600 |
| `max_retries` | integer | 10 | Give-up threshold: stop retrying a slot after this many consecutive failures. Range: 1-1000 |
| `network_loss_timeout_sec` | integer | 60 | Seconds of failed network registration before recovery is triggered. Range: 10-600 |
| `network_poll_interval_sec` | integer | 30 | How often to poll the modem for network registration status (seconds). Range: 5-300 |

### `[control]`

Configures the Unix domain socket used by `card` CLI subcommands to communicate with the running daemon.

| Key | Type | Default | Description |
|---|---|---|---|
| `socket_path` | string | `/tmp/gsm-sip-bridge.sock` | Filesystem path for the control socket. Must be writable by the bridge process and readable by CLI users. |

### `[vowifi]`

The inbound VoWiFi-to-SIP bridge (specs/011-vowifi-sip-bridge) — a second,
independent inbound call path alongside `[sip]`/`[bridge]`. Only read by the
`vowifi-*-agent` subcommands and `docker/entrypoint.sh`/`healthcheck.sh`
(via `gsm-sip-bridge config vowifi-shell-env`), never by the normal daemon
path. All ePDG-tunnel configuration lives here — none of it is read from
environment variables; `.env` holds secrets only (specs/012-strongswan-epdg
config consolidation).

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `false` | Master switch |
| `mcc` | string | `""` (auto-derive) | Home network MCC; empty means derive from the SIM (IMSI + EF_AD, `AT+COPS` fallback). Must be set together with `mnc` or not at all |
| `mnc` | string | `""` (auto-derive) | Home network MNC, zero-padded to 3 digits; empty means derive from the SIM |
| `modem_port` | string | `/dev/ttyUSB6` | AT port for the modem whose SIM authenticates |
| `use_tcp` | boolean | `true` | SIP transport to the P-CSCF |
| `sec_agree` | boolean | `true` | Advertise `Require: sec-agree` / negotiate Gm IPsec |
| `pcscf_source_path` | string | `/tmp/pcscf` | Path Agent A reads the tunnel-assigned P-CSCF from |
| `veth_local_addr` | string | `10.99.0.1` | Agent A's (ims-netns end) veth address |
| `veth_peer_addr` | string | `10.99.0.2` | Agent B's (default-netns end) veth address |
| `control_port` | integer | 7050 | Agent A↔B control channel TCP port |
| `wideband` | boolean | `true` | Carry AMR-WB/G.722 end-to-end instead of narrowing to 8 kHz |
| `apn` | string | `ims` | APN used by the `swu` engine's dialer |
| `netns` | string | `ims` | Network namespace the ePDG tunnel lives in |
| `epdg_fqdn` | string | derived from `mcc`/`mnc` (configured or SIM-derived) | ePDG FQDN to resolve via DNS |
| `epdg_ip` | string | unset (resolve `epdg_fqdn`) | Skip DNS and dial this ePDG IP directly |
| `src_addr` | string | unset (auto-select) | Force the tunnel's local source address |
| `keepalive_interval_sec` | integer | 20 | Idle-tunnel TCP keepalive interval |
| `veth_sip_iface` | string | `veth-sip` | veth end in the default netns |
| `veth_ims_iface` | string | `veth-ims` | veth end inside `netns` |
| `tunnel_engine` | enum | `strongswan` | `strongswan` or `swu` (specs/012-strongswan-epdg) |
| `strongswan_tun_iface` | string | `tun23` | strongswan engine's XFRM interface name |
| `strongswan_if_id` | integer | 23 | strongswan engine's XFRM interface `if_id` |
| `vpcd_host` | string | `127.0.0.1` | pcscd's vpcd virtual reader host (strongswan engine) |
| `vpcd_port` | integer | 35963 | pcscd's vpcd virtual reader port (strongswan engine) |
| `imsi_override` | string | unset (read via AT+CIMI) | Diagnostic escape hatch (strongswan engine) |

## Examples

### Single-card development

```toml
[sip]
server = "127.0.0.1"
port = 5060
username = "test"
password = "test"
transport = "udp"

[sms]
enabled = false
```

### Production multi-card with TLS

```toml
[sip]
server = "pbx.example.com"
port = 5061
username = "gsm-bridge"
password = "env:SIP_PASSWORD"
transport = "tls"
tls_verify = "strict"
display_name = "GSM Bridge"

[bridge]
sip_destination = ""
sip_dial_timeout_sec = 30

[sms]
enabled = true
discord_webhook_url = "env:DISCORD_WEBHOOK_URL"
db_path = "/data/store.db"

[metrics]
port = 9091

[modules]
retry_interval_sec = 30
max_concurrent = 8

[resilience]
initial_backoff_sec = 5
max_backoff_sec = 120
max_retries = 10
network_loss_timeout_sec = 60
network_poll_interval_sec = 30

[control]
socket_path = "/run/gsm-sip-bridge/control.sock"
```
