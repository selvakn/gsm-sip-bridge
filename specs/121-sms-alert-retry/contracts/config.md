# Contract: `[sms]` configuration

Three new optional keys in `[sms]` (all strict-parsed; unknown keys still error).

| Key | Type | Default | Range | Description |
|---|---|---|---|---|
| `retry_enabled` | boolean | `true` | – | Retry undelivered Discord notifications. `false` restores today's behaviour (status recorded, never retried). |
| `retry_interval_sec` | integer | `3600` | 60–86400 | Interval of the background sweep, per process that owns an SMS Discord client. |
| `retry_max_age_sec` | integer | `604800` (7 d) | 3600–2592000 | A text still undelivered this long after it was received is marked `abandoned`. |

Validation errors follow the existing `BridgeError::Config` messages naming `[sms].<key>`.
`config.toml.example`, `docs/configuration.md` and `test_config_docs.rs` must list all three.
Existing configs with none of the keys behave as above (retry on) — a deliberate default change, called out in CHANGELOG.
