# Contract: metrics endpoint + Discord alert (new)

New surfaces, additive to the existing metrics scrape and Discord alert contracts
(`specs/005-observability-metrics`, `specs/022-discord-critical-alerts`) — nothing existing changes
shape.

## Metric: `gsm_sip_bridge_critical_event_active{category="line_discovery_failed"}` (reused, not new)

Implementation-time simplification: rather than a bespoke gauge, this reuses the metric
`alerts::dispatch()` already maintains for every ongoing-health alert category:

```
# HELP gsm_sip_bridge_critical_event_active 1 while a critical-event category is in its alerted unhealthy state for this module/line
# TYPE gsm_sip_bridge_critical_event_active gauge
gsm_sip_bridge_critical_event_active{category="line_discovery_failed",module="<identifier>"} 1
```

- `module` label: same identifier as the corresponding `FailedLine.card_id` /
  `vowifi-status` "Configured line \<identifier\>" output — the configured `modem_port` path or
  `modem_serial`.
- Value `1` once the line's retry window elapses without success (the `Failure` event fires);
  `0` once the line later resolves within the same process lifetime and the paired `Recovered`
  event fires (FR-011) — this is the exact same mechanism `registration_loss`/`tunnel_failure`
  already use, just a new `category` label value alongside theirs.
- Set by `alerts::dispatch()` itself (via `AlertContext::fire`, called from the retry loop), not
  via the `AgentReport` ingestion path the `vowifi_*` gauges use — see `research.md` R5 for why
  (no agent process exists for a line that was never discovered).
- Absent entirely for a line that resolved on its first discovery pass, or whose category is
  disabled — no series is ever written for it.

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
