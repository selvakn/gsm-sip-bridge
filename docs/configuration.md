# Configuration Reference

The bridge reads a single TOML configuration file specified via `--config`.

## Sections

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
| `mcc` | string | `""` | Home network MCC, required when `enabled = true` |
| `mnc` | string | `""` | Home network MNC, required when `enabled = true` |
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
| `epdg_fqdn` | string | derived from `mcc`/`mnc` | ePDG FQDN to resolve via DNS |
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
