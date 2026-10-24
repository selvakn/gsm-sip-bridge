# Contract: Configuration additions

All of these keys are new. If they're omitted, the behaviour equals today's, except that the signal gauges appear and events are recorded (research R12).

## `[webui]` (new table)

```toml
[webui]
# Serve the read-only management UI (specs/122). Off unless set.
enabled = false
# Its own listener, separate from [metrics].port. Must be an IP address.
listen_addr = "0.0.0.0"
port = 8080
# Required when enabled; the bridge refuses to start otherwise.
# Prefer the env: indirection so the secret stays out of this file.
password = "env:WEBUI_PASSWORD"
# Optional. When set, the UI links here for history and graphs.
# grafana_url = "https://grafana.example/d/abc"
```

| Key | Type | Default | Validation |
|---|---|---|---|
| `enabled` | bool | `false` | |
| `listen_addr` | string (IP) | `"0.0.0.0"` | Must parse as `IpAddr` (same rule as `[sip_server].listen_addr`) |
| `port` | u16 | `8080` | Non-zero. Must differ from `[metrics].port`, `[sip].port` and `[sip_server].listen_port` when those are set |
| `password` | Secret<string> | `""` | Non-empty when `enabled = true`; otherwise a config error naming the key (FR-003) |
| `grafana_url` | string | `""` | Must start with `http://` or `https://` when set |

## `[metrics]` (one new key)

| Key | Type | Default | Notes |
|---|---|---|---|
| `signal_poll_interval_seconds` | u64 | `60` | `0` disables signal polling. Values below 20 are raised to 20 (one sweep interval) with a startup warning |

## `[alerts]` (one new key)

| Key | Type | Default | Notes |
|---|---|---|---|
| `event_retention_days` | u32 | `30` | Must be at least 1 |

## Documentation obligations

- `config.toml.example` gets the commented `[webui]` block above and the two new keys.
- `tests/test_config_docs.rs` must keep passing; it checks that every key is documented.
- The `docker/` compose `.env` example gets a `WEBUI_PASSWORD=` placeholder line. Compose already uses `network_mode: host`, so no port mapping is needed.
