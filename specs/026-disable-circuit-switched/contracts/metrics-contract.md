# Contract: metrics endpoint with the circuit-switched path disabled

**Feature**: 026-disable-circuit-switched
**Satisfies**: FR-014, FR-021, FR-021a, FR-021b, FR-021c, FR-022, SC-005, SC-005a

## New series

```text
# HELP gsm_sip_bridge_cs_enabled 1 when the circuit-switched call path is enabled, 0 when disabled
# TYPE gsm_sip_bridge_cs_enabled gauge
gsm_sip_bridge_cs_enabled 0
```

| Property | Value |
|---|---|
| Type | Gauge, no labels |
| Value | `1` enabled, `0` disabled |
| Presence | **Both states**, always |

Presence in both states is the requirement, not an implementation convenience. A gauge that appeared only when disabled would be indistinguishable from a dead daemon — which is precisely the ambiguity FR-021b exists to remove (SC-005a).

## Series absent when disabled

| Series |
|---|
| `gsm_sip_bridge_modules_active` |
| `gsm_sip_bridge_modules_failed` |
| `gsm_sip_bridge_module_init_total` |
| `gsm_sip_bridge_module_retries_total` |
| `gsm_sip_bridge_scheduled_restart_total` |

**Absent, not zero** (FR-021a). A zero-valued `modules_active` would make any `modules_active == 0` alert rule fire continuously against a path the operator switched off deliberately — the exact false-alert case SC-005 forbids, and it would force every existing alert rule to be rewritten.

These metrics are `once_cell::sync::Lazy` statics that register into the registry on first dereference, and every dereference site is inside `modules/mod.rs`. With `CardPool` not running they are never touched, never registered, and never exported. **This is a property of the current code, not a library guarantee** — it must be pinned by a test, because it breaks silently the moment any non-circuit-switched path touches one of these statics.

## Series unaffected

| Series | Why it stays |
|---|---|
| `gsm_sip_bridge_outbound_attempts_total` | `metrics::ingest` increments it from VoWiFi/VoLTE agent reports, independently of the circuit-switched path |
| All `gsm_sip_bridge_vowifi_*` | Reported by the VoWiFi agents through the control socket |
| All `gsm_sip_bridge_volte_*` | Same, for VoLTE |
| `gsm_sip_bridge_agent_up`, `gsm_sip_bridge_agent_last_report_seconds` | Agent liveness, not card state |
| `gsm_sip_bridge_build_info`, `gsm_sip_bridge_uptime_seconds` | Process-level |
| `gsm_sip_bridge_sms_*`, `gsm_sip_bridge_store_*` | Message store stays open (FR-016) |

The endpoint itself keeps serving (FR-014). Nothing about the scrape path changes.

## When enabled

Byte-identical to today's output apart from the added `gsm_sip_bridge_cs_enabled 1` line (FR-021c). Same series, same names, same labels — existing dashboards and alert rules keep working with no edits. This is asserted by test, not assumed.

## Alerting

Circuit-switched card-lifecycle alerts must not fire while the path is off (FR-022), and no circuit-switched card may be reported in a failed, given-up, or active-alert state (FR-021). With `CardPool` not running there is no lifecycle state machine to produce such an event, so this follows from the gating — again, pinned by test rather than assumed.
