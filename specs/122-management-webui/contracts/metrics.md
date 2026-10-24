# Contract: New Prometheus series

These are exported on the existing `/metrics` endpoint (`[metrics].port`). They are registered in the default registry, the same one `metrics_handler` already gathers.

| Name | Type | Labels | Meaning |
|---|---|---|---|
| `gsm_sip_bridge_signal_rssi_dbm` | gauge | `module`, `transport` | From `AT+CSQ`. The series is **removed** (not set to 0) while the reading is unknown (FR-023) |
| `gsm_sip_bridge_signal_rsrp_dbm` | gauge | `module`, `transport` | LTE only; removed otherwise |
| `gsm_sip_bridge_signal_rsrq_db` | gauge | `module`, `transport` | LTE only |
| `gsm_sip_bridge_signal_sinr_db` | gauge | `module`, `transport` | LTE only |
| `gsm_sip_bridge_signal_last_read_seconds` | gauge | `module`, `transport` | Age of the last successful reading, refreshed on scrape the same way `agent_last_report_seconds` is |
| `gsm_sip_bridge_events_recorded_total` | counter | `kind_group` | `kind_group` is the part of `kind` before `:` (e.g. `alert_raised`), keeping cardinality closed |
| `gsm_sip_bridge_webui_logins_total` | counter | `outcome` (`success`/`invalid`/`locked_out`) | Only registered when the UI is enabled |

**Cardinality**: `module` is already a label on existing series, and `transport` has 3 values. No per-call or per-IP labels are added.

**Renames and removals**: none. `tests/test_metric_renames.rs` is unaffected.
