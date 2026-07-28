# Contract: `[alerts]` config.toml schema

New top-level section, added to `TOP_LEVEL_SECTIONS` in `config/mod.rs`
alongside `[sms]` (unchanged).

```toml
[alerts]
# Shared default webhook for any category below that doesn't override it.
# Optional — if empty and no category overrides it, that category's alerts
# are skipped (logged/metered only). Treated as a secret (FR-016).
discord_webhook_url = "https://discord.com/api/webhooks/..."

[alerts.sms]
enabled = true                 # default true (unchanged from today's [sms].enabled)
# discord_webhook_url = "..."  # optional override

[alerts.module_lifecycle]
enabled = false                       # default false (FR-007)
# discord_webhook_url = "..."         # optional override
at_worker_unresponsive_sec = 60       # FR-003, default 60

[alerts.registration_loss]
enabled = false                       # default false
# discord_webhook_url = "..."
unhealthy_sec = 300                   # FR-006, default 300

[alerts.tunnel_failure]
enabled = false                       # default false
# discord_webhook_url = "..."
unhealthy_sec = 300                   # FR-005, default 300

[alerts.missed_call]
enabled = false                       # default false
# discord_webhook_url = "..."
```

## Backward compatibility (FR-001)

`[sms].discord_webhook_url` and `[sms].enabled` are unchanged and continue to
work standalone. If `[alerts.sms]` is absent from `config.toml`,
`alerts.sms.enabled`/`webhook_url_override` are seeded from `[sms]`'s
existing values, so an operator upgrading with no config changes keeps
today's SMS-forwarding behavior exactly as-is. If both `[sms]` and
`[alerts.sms]` set an enabled/webhook value, `[alerts.sms]` wins (it is the
more specific, newer section).

## Validation rules (mirrors existing `warn_unknown_keys_in` convention)

- `ALERTS_KEYS = ["discord_webhook_url"]`
- `ALERTS_CATEGORY_KEYS = ["enabled", "discord_webhook_url"]` plus the
  category's own threshold key(s) where applicable
  (`at_worker_unresponsive_sec` / `unhealthy_sec`).
- Threshold values are validated with the existing `as_u64_range` helper;
  reasonable bounds: `at_worker_unresponsive_sec` 5..=600,
  `unhealthy_sec` (both categories) 30..=3600. Out-of-range or malformed
  values log a warning and fall back to the default (Edge Case: "alerting
  subsystem itself errors" → default to disabled/default value, never a
  fatal config error for this section).
- Unknown keys inside `[alerts]` or any `[alerts.<category>]` table warn
  (not error), consistent with every other section.
