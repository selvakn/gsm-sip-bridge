# Contract — Prometheus Metrics

**Endpoint**: `GET /metrics` on the configured `[metrics].port` (default `9091`).
**Format**: Prometheus exposition (text), produced by `prometheus::TextEncoder`.
**Prefix**: `gsm_sip_bridge_*` (clean break from v4.1.x's `gsm_bridge_*`; mapping documented in the migration guide).
**Spec links**: FR-050, FR-051, FR-052, FR-072, R-08

## Conventions

- Counter names end in `_total`.
- Duration histograms end in `_seconds` and use buckets named in the metric definition.
- Boolean states are exposed as gauges with values `0` or `1`.
- Module-scoped metrics carry a `module="<id>"` label (e.g. `module="ec20-A1B2C3"`). The label value matches the persisted `module_id`.
- Labels are bounded: status enums are fixed sets; module label cardinality is at most `[modules].max_concurrent` (≤8).

## Catalog

### Calls

| Name | Type | Labels | Semantics |
|---|---|---|---|
| `gsm_sip_bridge_calls_total` | Counter | `module`, `status={incoming,answered,missed}`, `caller_id` | Total GSM calls observed, broken down by per-module final disposition and caller MSISDN. (Note: `caller_id` makes this high-cardinality if many distinct callers; matches v4.1.x and is documented here as expected.) |
| `gsm_sip_bridge_sip_calls_total` | Counter | `module`, `status={initiated,connected,timeout,error}` | Outbound SIP calls per module. |
| `gsm_sip_bridge_call_duration_seconds` | Histogram | `module` | Buckets: `1, 5, 10, 30, 60, 120, 300, 600, 1200, 1800` seconds. Observed only on call termination with `status = answered`. |
| `gsm_sip_bridge_active_calls` | Gauge | `module` | `0` or `1` (one module = at most one call). Sum across modules = total active. |

### SIP registration

| Name | Type | Labels | Semantics |
|---|---|---|---|
| `gsm_sip_bridge_sip_registrations_total` | Counter | `status={attempted,success,failed}` | SIP REGISTER outcomes. |
| `gsm_sip_bridge_sip_registered` | Gauge | — | `1` if registered, `0` otherwise. |

### Modules

| Name | Type | Labels | Semantics |
|---|---|---|---|
| `gsm_sip_bridge_module_init_total` | Counter | `module`, `status={success,failure}`, `reason` | Module init attempts at startup or during retry. `reason` populated only on `failure` (e.g. `sim_not_registered`, `serial_busy`, `audio_unavailable`). |
| `gsm_sip_bridge_module_retries_total` | Counter | `module` | Number of retry-thread reinit attempts. |
| `gsm_sip_bridge_modules_active` | Gauge | — | Count of modules in `Active` state. |
| `gsm_sip_bridge_modules_failed` | Gauge | — | Count of modules in `Failed` state pending retry. |

### Audio

| Name | Type | Labels | Semantics |
|---|---|---|---|
| `gsm_sip_bridge_audio_errors_total` | Counter | `module`, `kind={underrun,overrun,alsa_open,alsa_io,format}` | Audio errors per module by kind. Underrun/overrun are SPSC ring buffer events; the others are ALSA-side. |

### SMS

| Name | Type | Labels | Semantics |
|---|---|---|---|
| `gsm_sip_bridge_sms_received_total` | Counter | `module` | SMS messages successfully read from the SIM. |
| `gsm_sip_bridge_sms_forwarded_total` | Counter | `module`, `outcome={sent,failed,skipped}` | Discord forwarding outcomes. |
| `gsm_sip_bridge_sms_db_writes_total` | Counter | `outcome={success,failure}` | SMS-row write attempts. |

### Persisted store

| Name | Type | Labels | Semantics |
|---|---|---|---|
| `gsm_sip_bridge_store_writes_total` | Counter | `table={calls,sms,meta}`, `outcome={success,failure}` | All writes to the store, broken down by table. |
| `gsm_sip_bridge_store_queue_depth` | Gauge | — | Pending work items waiting for the DB writer thread. Persistent non-zero = writer falling behind. |

### Process

| Name | Type | Labels | Semantics |
|---|---|---|---|
| `gsm_sip_bridge_uptime_seconds` | Gauge | — | Seconds since process start. |
| `gsm_sip_bridge_build_info` | Gauge (always 1) | `version`, `git_sha`, `pjsip_version`, `rust_version` | Constant-1 gauge used to expose build metadata as labels (standard Prometheus pattern). |

(Optional) Process and runtime metrics from the `prometheus` crate's process collector are exposed under their canonical names if the collector is enabled (decision tracked in research.md "open items").

## Rename mapping (v4.1.x → v5.0.0)

| v4.1.x | v5.0.0 |
|---|---|
| `gsm_bridge_calls_total` | `gsm_sip_bridge_calls_total` |
| `gsm_bridge_sip_calls_total` | `gsm_sip_bridge_sip_calls_total` |
| `gsm_bridge_sip_registrations_total` | `gsm_sip_bridge_sip_registrations_total` |
| `gsm_bridge_module_init_total` | `gsm_sip_bridge_module_init_total` |
| `gsm_bridge_module_retries_total` | `gsm_sip_bridge_module_retries_total` |
| `gsm_bridge_audio_errors_total` | `gsm_sip_bridge_audio_errors_total` |
| `gsm_bridge_sip_registered` | `gsm_sip_bridge_sip_registered` |
| `gsm_bridge_modules_active` | `gsm_sip_bridge_modules_active` |
| `gsm_bridge_modules_failed` | `gsm_sip_bridge_modules_failed` |
| `gsm_bridge_active_calls` | `gsm_sip_bridge_active_calls` |
| `gsm_bridge_uptime_seconds` | `gsm_sip_bridge_uptime_seconds` |
| `gsm_bridge_sms_received_total` | `gsm_sip_bridge_sms_received_total` |
| `gsm_bridge_sms_forwarded_total` | `gsm_sip_bridge_sms_forwarded_total` |
| `gsm_bridge_sms_db_writes_total` | `gsm_sip_bridge_sms_db_writes_total` |
| `gsm_bridge_call_duration_seconds` | `gsm_sip_bridge_call_duration_seconds` |
| (new in v5.0.0) | `gsm_sip_bridge_store_writes_total` |
| (new in v5.0.0) | `gsm_sip_bridge_store_queue_depth` |
| (new in v5.0.0) | `gsm_sip_bridge_build_info` |

## Non-blocking guarantee (FR-051)

A `/metrics` scrape MUST NOT acquire any lock that the audio path or SIP signaling path can hold. The `prometheus::Registry` collects metric family snapshots into a temporary buffer; the collection happens on the axum worker task and never touches the audio SPSC queues. Buffered atomics behind counters and gauges are read with `Relaxed` ordering. Verified by `tests/test_metrics.rs` which holds eight concurrent calls in flight while a tight scrape loop runs and asserts no audio underruns.
