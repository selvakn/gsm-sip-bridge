# Contract: new Prometheus metrics

Added to `metrics/mod.rs` alongside the existing `SMS_*` family (same
`CounterVec`/`GaugeVec` + `Lazy` construction convention).

## `gsm_sip_bridge_critical_alerts_total{category, outcome}`

Counter. Incremented once per alert *decision*, regardless of category.

- `category` ∈ `sms | module_lifecycle | registration_loss | tunnel_failure |
  missed_call`
- `outcome` ∈ `sent | suppressed | skipped | failed`
  - `sent`: Discord POST returned 2xx.
  - `suppressed`: condition is still continuously unhealthy; no re-alert
    sent per FR-013 (transition-based alerting).
  - `skipped`: category disabled, or no webhook resolved for it (FR-014).
  - `failed`: Discord POST attempted and did not succeed (network error,
    4xx/5xx) — mirrors `SMS_FORWARDED_TOTAL`'s `failed` outcome.

## `gsm_sip_bridge_critical_event_active{category, module}`

Gauge, 0 or 1. 1 while `category` is in its alerted (post-threshold)
unhealthy state for `module` (module id for GSM-side categories, line id for
VoWiFi/VoLTE-side categories); 0 once recovered or never unhealthy. Mirrors
the existing `AGENT_UP{agent, module_id}` gauge shape so a Grafana panel can
reuse the same query pattern (`sum by (category) (gsm_sip_bridge_critical_event_active)`
for "how many things are currently broken").

## Existing metrics reused, not duplicated

- `SMS_RECEIVED_TOTAL` / `SMS_FORWARDED_TOTAL` / `SMS_DB_WRITES_TOTAL`
  continue to own the SMS category's own counters, unchanged — FR-001 only
  requires SMS alerting to move under the same *configuration* mechanism,
  not the same metric names.
- `CALLS_TOTAL{module_id, "missed", cs}` already counts missed
  circuit-switched calls; the missed-call alert's `critical_alerts_total`
  counter is additive to this, not a replacement.
