# Contract — Configuration File (`config.toml`)

**Format**: TOML 1.0
**Loader**: `serde` + `toml` crate
**Source of truth**: `gsm-sip-bridge/src/config/mod.rs`
**Spec link**: FR-070, FR-075..078, R-06

## Top-level layout

```toml
[sip]
[bridge]
[sms]
[metrics]
[modules]
```

`[sip]` is required. The other sections are optional and fall back to documented defaults.

## Secret reference syntax

For any field marked **(secret-bearing)** in the tables below, the value MUST be either:

- a literal string: `password = "p4ssw0rd"`
- an environment-variable reference: `password = "env:SIP_PASSWORD"` (the bridge reads `$SIP_PASSWORD` at startup)

Behaviour when an `env:VAR_NAME` reference cannot be resolved:
- the variable is unset, OR
- the variable is set to the empty string

→ the bridge refuses to start with the error `secret variable VAR_NAME is unset or empty (referenced from <config-key>)`. (FR-077)

CLI flags are not accepted for secret-bearing fields (FR-076).

## `[sip]` — SIP server credentials and transport

| Key | Type | Default | Required | Notes |
|---|---|---|---|---|
| `server` | string | — | yes | PBX hostname or IP. |
| `port` | integer 1..65535 | `5060` | no | |
| `username` | string | — | yes | |
| `password` | string | — | yes | **(secret-bearing)** |
| `transport` | enum: `udp` \| `tcp` \| `tls` | `udp` | no | |
| `local_port` | integer 1..65535 | `5060` | no | Fixed local port, prevents stale registrations. |
| `display_name` | string | value of `username` | no | Shown to callees. |
| `tls_verify` | enum: `strict` \| `skip` | `strict` | no | Only meaningful when `transport = "tls"`. `skip` triggers a startup `WARN`. (R-03) |

## `[bridge]` — Call-bridging behaviour

| Key | Type | Default | Required | Notes |
|---|---|---|---|---|
| `sip_destination` | string | `""` | no | Empty = DID passthrough (use the GSM caller's number as the SIP request DID). Otherwise, the fixed extension to dial. |
| `sip_dial_timeout_sec` | integer 5..120 | `30` | no | Seconds to wait for a SIP answer before treating the call as failed. |

## `[sms]` — SMS monitoring and forwarding

| Key | Type | Default | Required | Notes |
|---|---|---|---|---|
| `enabled` | boolean | `true` | no | Set `false` to disable SMS monitoring entirely. |
| `discord_webhook_url` | string | `""` | no | **(secret-bearing)** Empty = persist only, do not forward. |
| `db_path` | string (file path) | `/var/lib/gsm-sip-bridge/store.db` | no | The persisted store file. Created on first run. |

## `[metrics]` — Prometheus exposition

| Key | Type | Default | Required | Notes |
|---|---|---|---|---|
| `port` | integer 1..65535 | `9091` | no | The `METRICS_PORT` environment variable, if set, takes precedence. |

## `[modules]` — Module pool tuning

| Key | Type | Default | Required | Notes |
|---|---|---|---|---|
| `retry_interval_sec` | integer 5..600 | `30` | no | Seconds between retries of failed module initialisation. (FR-017) |
| `max_concurrent` | integer 1..8 | `8` | no | Informational ceiling on the number of modules the bridge will accept. Hard limit per FR-014; setting >8 is rejected at load. |

## Example: minimum viable config

```toml
[sip]
server   = "pbx.example.com"
username = "bridge-account"
password = "env:SIP_PASSWORD"
```

## Example: full config

```toml
[sip]
server       = "pbx.internal.example.com"
port         = 5060
username     = "gsm-bridge"
password     = "env:SIP_PASSWORD"
transport    = "tls"
local_port   = 5060
display_name = "GSM Bridge"
tls_verify   = "strict"

[bridge]
sip_destination      = ""        # DID passthrough
sip_dial_timeout_sec = 30

[sms]
enabled             = true
discord_webhook_url = "env:DISCORD_WEBHOOK_URL"
db_path             = "/var/lib/gsm-sip-bridge/store.db"

[metrics]
port = 9091

[modules]
retry_interval_sec = 30
max_concurrent     = 8
```

## Validation rules (enforced at load)

1. All required `[sip]` keys present and non-empty.
2. `transport` is one of the enumerated values.
3. `tls_verify` is set only when `transport = "tls"`; otherwise its value is ignored (with a `WARN` if explicitly set to `skip` under non-TLS).
4. `port`, `local_port`, `metrics.port` in `1..=65535`.
5. `sip_dial_timeout_sec` in `5..=120`.
6. `retry_interval_sec` in `5..=600`.
7. `max_concurrent` in `1..=8`.
8. Every secret-bearing value referencing `env:VAR_NAME` resolves to a non-empty string.
9. Unknown top-level sections or keys produce a `WARN` but do not fail the load.

## Failure modes summary

| Condition | Behaviour |
|---|---|
| File path passed via `--config` does not exist | Exit 1, message names the path. |
| File is not valid TOML | Exit 1, message includes line number from `toml` parser. |
| Required field missing | Exit 1, message names the field. |
| Out-of-range value | Exit 1, message names the field and shows the valid range. |
| `env:VAR_NAME` reference fails to resolve | Exit 1, message names the variable. |
| Unknown top-level section or key | Log `WARN`, continue. |
