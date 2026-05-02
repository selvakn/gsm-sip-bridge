# Data Model: Observability Metrics

## Metric Families

### Counters (monotonically increasing)

| Metric Name | Labels | Description |
|---|---|---|
| `gsm_bridge_calls_total` | `module_id`, `direction`, `status` | Total GSM calls (direction: incoming; status: answered, missed, rejected) |
| `gsm_bridge_sip_calls_total` | `module_id`, `status` | Total outbound SIP calls (status: initiated, connected, busy, timeout, error) |
| `gsm_bridge_sip_registrations_total` | `status` | SIP registration attempts (status: success, failure) |
| `gsm_bridge_module_init_total` | `module_id`, `status` | Module initialization attempts (status: success, failure) |
| `gsm_bridge_module_retries_total` | `module_id` | Module retry attempts |
| `gsm_bridge_at_commands_total` | `module_id`, `command`, `status` | AT command executions (status: success, timeout, error) |
| `gsm_bridge_audio_errors_total` | `module_id`, `type` | Audio errors (type: alsa_open, alsa_read, alsa_write, ring_buffer_overflow) |

### Gauges (current value)

| Metric Name | Labels | Description |
|---|---|---|
| `gsm_bridge_active_calls` | `module_id` | Currently active bridged calls per module |
| `gsm_bridge_modules_active` | | Number of active (healthy) modules |
| `gsm_bridge_modules_failed` | | Number of failed modules pending retry |
| `gsm_bridge_sip_registered` | | SIP registration state (1 = registered, 0 = unregistered) |
| `gsm_bridge_module_state` | `module_id`, `state` | Per-module state (1 = current state) |
| `gsm_bridge_uptime_seconds` | | Process uptime in seconds |

### Histograms

| Metric Name | Labels | Buckets | Description |
|---|---|---|---|
| `gsm_bridge_call_duration_seconds` | `module_id` | 1, 5, 15, 30, 60, 120, 300, 600, 1800 | Duration of completed bridged calls |

## Label Values

### `module_id`
Format: `ec20-<serial_number>` (stable across reboots, matches CardInstance card_id)

### `status` (calls)
- `answered`: GSM call was answered and bridged
- `missed`: GSM call rang but was not picked up (timeout or no answer)
- `rejected`: GSM call was explicitly rejected

### `status` (sip_calls)
- `initiated`: SIP INVITE sent
- `connected`: SIP call established (200 OK)
- `busy`: SIP 486 Busy Here
- `timeout`: SIP call timed out
- `error`: Other SIP failure

### `state` (module_state)
Values map to `CardState` enum: `idle`, `ringing`, `bridging`, `error`

## Metric Lifecycle

1. **Startup**: Registry created, all families registered. Exposer binds to port.
2. **Runtime**: Counters incremented via fire-and-forget calls. Gauges set on state changes. Histogram observed on call completion.
3. **Shutdown**: Exposer stops accepting connections. Registry destroyed.

## Thread Safety

prometheus-cpp guarantees thread-safe access to all metric objects. No additional synchronization needed in the metrics module.
