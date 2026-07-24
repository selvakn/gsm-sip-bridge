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
| `agent_report_interval_seconds` | integer | 10 | How often each VoWiFi agent re-reports its call/SMS/health state over the control socket. Also sets the staleness threshold (3x this) after which an agent that stopped reporting is marked down. Ignored when `[vowifi].enabled` is false. |

### `[modules]`

| Key | Type | Default | Description |
|---|---|---|---|
| `retry_interval_sec` | integer | 30 | Range: 5-600 |
| `max_concurrent` | integer | 8 | Range: 1-8 |

### `[audio]`

Latency and audio-quality tuning **shared by every call path** — circuit-switched
(EC20 USB audio) AND VoWiFi/VoLTE IMS calls all run through the same jitter-buffer
pipeline. All keys are optional. The section is read at startup; changes require a
process restart. See `docs/audio-tuning-log.md` for the empirical history behind
the defaults.

| Key | Type | Default | Description |
|---|---|---|---|
| `profile` | enum | `lan` | Latency preset: `lan` or `wan` (see below) |
| `vad` | boolean | `true` | PJMEDIA voice activity detection / noise suppression on the capture path. Set `false` only to diagnose audio issues (raw passthrough). |
| `snd_rec_latency_ms` | integer | `150` | Capture ring-buffer depth (caller → SIP), 20–2000 ms. Raise if logs report `alsa_capture_overrun`. |
| `snd_play_latency_ms` | integer | `150` | Playback ring-buffer depth (SIP → caller), 20–2000 ms. Raise if logs report `alsa_playback_underrun`. |

### `[modem_audio]`

EC20 USB sound-device tuning for the circuit-switched path **only** — VoWiFi/VoLTE
never touch this modem's ALSA device (VoWiFi/VoLTE hard-codes its own gain and
never applies these knobs at all). All keys are optional.

| Key | Type | Default | Description |
|---|---|---|---|
| `rx_gain` | integer | unset (firmware default) | EC20 downlink digital gain (`AT+QRXGAIN`, 0–65535), applied at module init. Controls how loud SIP audio sounds to the GSM caller. |
| `eec_mode` | integer | unset (firmware default, 12543) | EC20 echo-canceller mode word (`AT+QEEC=2,<val>`). `0` disables all EC — recommended for USB audio bridges, which have no acoustic echo path. |
| `tx_level` | float | `1.0` | PJSUA conference-bridge software gain on the GSM → SIP path (`1.0` = unity, `0.5` ≈ −6 dB, `2.0` = +6 dB). |
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

The inbound VoWiFi-to-SIP bridge — a second, independent inbound call path
alongside `[sip]`/`[bridge]`. Only read by the `vowifi-*-agent`/`discover`
subcommands and `docker/entrypoint.sh`/`healthcheck.sh` (via
`gsm-sip-bridge config vowifi-shell-env`/`gsm-sip-bridge discover
--shell-env`), never by the normal daemon path. All ePDG-tunnel
configuration lives here — none of it is read from environment variables;
`.env` holds secrets only (specs/012-strongswan-epdg config consolidation).

**Multi-line (specs/013-multi-card-vowifi)**: VoWiFi auto-discovers *every*
attached VoWiFi-capable modem by default. Each discovered SIM becomes its
own **line**: its own tunnel, IMS registration, network namespace, and
inbound call path, running concurrently, up to `max_lines`. Every per-line
resource (netns, XFRM `if_id`/interface name, veth interface names/
addresses) is *derived* from each line's position in the discovered line
table — there is no config knob for any of it. Line-specific settings
(which modem, its home network identity) live only in `[[vowifi.line]]`
below; everything else in this section is a genuinely global/shared value.
Run `gsm-sip-bridge --config config.toml discover --shell-env` to see what
gets discovered/resolved.

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `false` | Master switch |
| `use_tcp` | boolean | `true` | SIP transport to the P-CSCF |
| `sec_agree` | boolean | `true` | Advertise `Require: sec-agree` / negotiate Gm IPsec |
| `pcscf_source_path` | string | `/tmp/pcscf` | Path Agent A reads the tunnel-assigned P-CSCF from. Shared with `[volte].pcscf_source_path` so a captured address is picked up there automatically too |
| `control_port` | integer | 7050 | Agent A↔B control channel TCP port — shared across every line; lines are told apart by their (internally derived) veth address, not this port |
| `wideband` | boolean | `true` | Carry AMR-WB/G.722 end-to-end instead of narrowing to 8 kHz |
| `apn` | string | `ims` | APN used by the `swu` engine's dialer — shared across every line |
| `epdg_fqdn` | string | unset (derived per line from that line's `mcc`/`mnc`) | Override the derived 3GPP ePDG FQDN — shared across every line if set |
| `epdg_ip` | string | unset (resolve `epdg_fqdn`) | Skip DNS and dial this ePDG IP directly — shared across every line if set |
| `src_addr` | string | unset (auto-select) | Force the tunnel's local source address — shared across every line if set |
| `keepalive_interval_sec` | integer | 20 | Idle-tunnel TCP keepalive interval |
| `tunnel_engine` | enum | `strongswan` | `strongswan` or `swu` (specs/012-strongswan-epdg) |
| `vpcd_host` | string | `127.0.0.1` | pcscd's vpcd virtual reader host (strongswan engine) |
| `vpcd_port` | integer | 15963 | Base TCP port pcscd's shared vpcd reader listens on — one reader serves every line, at `base + line-index`. Unlike the other per-line fields, this **is** a genuine config key: keep the base **below** the kernel's ephemeral range (`net.ipv4.ip_local_port_range`, 32768-60999 by default) — see [operations.md](operations.md#vowifi-no-smart-card-reader--vpcd-connection-refused) |
| `max_lines` | integer | 8 | Upper bound on concurrently supported VoWiFi lines (specs/013-multi-card-vowifi FR-016); modems discovered beyond this count are reported and skipped |

#### `[[vowifi.line]]`

Optional array of tables — omit entirely for full auto-discovery (every
SIM-ready modem becomes its own line, network identity auto-derived from the
SIM). Add one entry per line to pin it to a specific modem and/or fix its
network identity. Every field is optional except the matcher.

| Key | Type | Default | Description |
|---|---|---|---|
| `modem_serial` | string | none | Match a modem by its USB hardware serial (preferred — survives device-path changes) |
| `modem_port` | string | none | Match (or pin) a modem by its AT serial device path directly |
| `mcc` | string | unset (auto-derive) | This line's home network MCC. Must be set together with `mnc` |
| `mnc` | string | unset (auto-derive) | This line's home network MNC, zero-padded to 3 digits |
| `imsi_override` | string | unset (read via AT+CIMI) | Diagnostic escape hatch: use this IMSI instead of reading it from the SIM |

### `[volte]`

Host-side IMS over LTE (specs/015-volte-host-ims) — the bridge runs its OWN IMS
registration over an LTE IMS PDN, instead of delegating to the modem's internal
IMS stack. **Do not enable this alongside `[vowifi]` on the same SIM** — both
register the same IMPU with the same IMEI-derived `+sip.instance`, so the network
tears one binding down (`volte-register` refuses to start while a VoWiFi agent is
running unless `--force` is given).

Like `[vowifi]`, auto-discovers every SIM-ready modem as its own line by default
(specs/018-volte-multi-modem), each with sane defaults — `cid` 3, `apn` `"ims"`,
P-CSCF auto-discovered. Line-specific settings live only in `[[volte.line]]`.
With `bridge_inbound`, each line also runs in its own network namespace/veth
pair (specs/020-volte-line-netns) — the same isolation `[vowifi]` lines get,
and just as with `[vowifi]`'s equivalent fields, this is pure internal
infrastructure with no config knob at all.

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | boolean | `false` | Master switch. Off by default; the `volte-*` subcommands work without it as diagnostics |
| `bridge_inbound` | boolean | `false` | Answer incoming calls over this registration and bridge them to the PBX (specs/017-volte-inbound-bridge), instead of only holding the registration open. Turning this on makes every bridged card EXCLUSIVE to this service — the circuit-switched daemon will not drive it |
| `max_lines` | integer | 8 | Upper bound on auto-discovered LTE lines (specs/018-volte-multi-modem). Only meaningful with `bridge_inbound` — a single PBX registration then serves every line, exactly as the VoWiFi path does |
| `pcscf_source_path` | string | `/tmp/pcscf` | Where the VoWiFi/ePDG path deposits the P-CSCF it learned from the IKEv2 config payload. Shared with `[vowifi].pcscf_source_path` so a captured address is picked up automatically |
| `status_path` | string | `/tmp/volte-registration-status` | Where `volte-register` publishes registration state for `volte-status` |
| `lock_path` | string | `/tmp/volte-registration.lock` | Lock file preventing two concurrent VoLTE registrations on one SIM |

#### `[[volte.line]]`

Optional array of tables — omit entirely for full auto-discovery. Add one entry
per line to pin it to a specific modem and/or override its PDN/P-CSCF settings.
Every field is optional except the matcher.

| Key | Type | Default | Description |
|---|---|---|---|
| `modem_serial` | string | none | Match a modem by its USB hardware serial (preferred — survives device-path changes) |
| `modem_port` | string | none | Match (or pin) a modem by its AT serial device path directly |
| `cid` | integer | 3 | This line's PDP context id. Must not collide with the contexts the modem uses for general internet access |
| `apn` | string | `ims` | This line's APN |
| `pcscf` | string | unset (auto-discover) | This line's explicit P-CSCF address. Unset falls back to `pcscf_source_path`, then on-modem discovery — which does not work on every carrier |
| `iface` | string | unset (auto-detect) | Host data interface bound to this line's IMS PDN. Unset auto-detects from the modem's own USB device |
| `msisdn` | string | none | This line's own MSISDN, advertised in the P-Preferred-Identity |

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
