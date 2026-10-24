# Contract: Prometheus metrics

Computed from the SQLite store on each `/metrics` scrape (daemon only), `retry_eligible = 1` rows.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `gsm_sip_bridge_sms_undelivered` | gauge | `state` = `pending` \| `failed` | Count of texts not yet delivered. |
| `gsm_sip_bridge_sms_abandoned` | gauge | – | Count of texts given up on (age limit). |
| `gsm_sip_bridge_sms_oldest_undelivered_age_seconds` | gauge | – | Age of the oldest `pending`/`failed` text; `0` when none. |

Unchanged: `gsm_sip_bridge_sms_forwarded_total{module,outcome,transport}` — now also counts retry attempts.
If the store cannot be read at scrape time the gauges are left at their last value and a warning is logged (scrape still succeeds).
Suggested external alert: `gsm_sip_bridge_sms_oldest_undelivered_age_seconds > 7200` (SC-004).
