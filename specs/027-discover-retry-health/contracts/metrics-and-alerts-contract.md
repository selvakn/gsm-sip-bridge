# Contract: metrics endpoint + Discord alert (new)

New surfaces, additive to the existing metrics scrape and Discord alert contracts
(`specs/005-observability-metrics`, `specs/022-discord-critical-alerts`) — nothing existing changes
shape.

## Metric: `gsm_sip_bridge_vowifi_line_discovery_failed`

```
# HELP gsm_sip_bridge_vowifi_line_discovery_failed 1 if this configured VoWiFi/VoLTE line failed to be discovered after retries, 0 otherwise
# TYPE gsm_sip_bridge_vowifi_line_discovery_failed gauge
gsm_sip_bridge_vowifi_line_discovery_failed{module="<identifier>"} 1
```

- `module` label: same identifier as the corresponding `FailedLine.card_id` /
  `vowifi-status` "Configured line \<identifier\>" output — the configured `modem_port` path,
  `modem_serial`, or synthetic `pcscN` id.
- Value `1` once the line's retry window elapses without success; reset to `0` (or the series
  simply stops being reported as `1` — implementation detail for `/speckit-tasks`) if the line
  later resolves within the same process lifetime (FR-011).
- Set directly by the startup retry loop (`supervise::orchestrate`), not via the `AgentReport`
  ingestion path other `vowifi_*` gauges use — see `research.md` R5 for why (no agent process
  exists for a line that was never discovered).
- Absent entirely for a line that resolved on its first discovery pass (no series with
  `module="<that line>"` at all) — matches how `VOWIFI_REGISTERED`/`VOWIFI_TUNNEL_UP` only ever
  have series for lines that exist.

## Discord alert: `line_discovery_failed` category

New `AlertCategory` alongside `registration_loss`/`tunnel_failure`/`sms`/`module_lifecycle`/
`missed_call`, controlled by a new `[alerts.line_discovery_failed]` config section with the same
shape as the existing categories:

```toml
[alerts.line_discovery_failed]
enabled = true
# webhook_url_override = "..."   # optional; falls back to [alerts].default_webhook_url
```

### Failure notification

Dispatched once, when a configured line's retry window elapses without success (FR-009/SC-004 —
exactly one, not a flood). Same delivery mechanism (`alerts::discord::DiscordClient`) and
enabled/webhook-resolution rules as `registration_loss`/`tunnel_failure` already use.

### Recovery notification

Dispatched once, if that same line later resolves within the same process lifetime after a failure
notification was already sent for it (FR-011) — mirrors the existing
`AlertPhase::Alerted → CriticalEventKind::Recovered` pairing `registration_loss`/`tunnel_failure`
already use, so an operator sees the same "this got worse" / "this got better" pair of messages
they already recognize from those categories.

### No notification cases

- A line that resolves *during* its retry window (before the window elapses) never triggers the
  failure notification at all (FR-010) — no failure-then-immediate-recovery noise.
- A line that never had a failure notification sent (still retrying, or the category is disabled)
  never triggers a recovery notification either — there is nothing to pair it with.

## Backward compatibility

Both surfaces are purely additive: an existing scrape config with no knowledge of the new metric
name is unaffected; an existing `config.toml` with no `[alerts.line_discovery_failed]` section gets
this category's default (disabled, matching every other category's `CategoryAlertConfig::disabled()`
default per `config/mod.rs`) rather than a parse error.
