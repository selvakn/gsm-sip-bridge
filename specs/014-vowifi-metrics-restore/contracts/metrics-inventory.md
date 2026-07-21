# Contract: Exported Metric Surface

Served at `http://<host>:9091/metrics` by the daemon — the single scrape target,
unchanged in `docker/prometheus.yml` (FR-015).

Legend: **±** = label added by this feature, **NEW** = metric added by this
feature, unmarked = unchanged.

---

## Call metrics

| Metric | Type | Labels | Reported by |
|---|---|---|---|
| `gsm_sip_bridge_calls_total` | Counter | `module`, `status`, **±`transport`** | daemon (cs), Agent A (vowifi) |
| `gsm_sip_bridge_sip_calls_total` | Counter | `module`, `status`, **±`transport`** | daemon (cs), Agent B (vowifi) |
| `gsm_sip_bridge_call_duration_seconds` | Histogram | `module`, **±`transport`** | daemon (cs), Agent A (vowifi) |
| `gsm_sip_bridge_active_calls` | Gauge | `module`, **±`transport`** | daemon (cs), Agent A (vowifi) |

`transport` ∈ {`cs`, `vowifi`}. Histogram buckets are unchanged
(1s … 1800s).

## SMS metrics

| Metric | Type | Labels | Reported by |
|---|---|---|---|
| `gsm_sip_bridge_sms_received_total` | Counter | `module`, **±`transport`** | daemon (cs), Agent B (vowifi) |
| `gsm_sip_bridge_sms_forwarded_total` | Counter | `module`, `outcome`, **±`transport`** | daemon (cs), Agent B (vowifi) |
| `gsm_sip_bridge_sms_db_writes_total` | Counter | `outcome` | store writer |

## VoWiFi health — NEW

| Metric | Type | Labels | Semantics |
|---|---|---|---|
| `gsm_sip_bridge_vowifi_registered` | Gauge | — | 1 = IMS registration currently held. Zeroed when Agent A goes stale. |
| `gsm_sip_bridge_vowifi_registrations_total` | Counter | `status` | `success` \| `auth_failed` \| `rejected` \| `timeout` |
| `gsm_sip_bridge_vowifi_tunnel_up` | Gauge | — | 1 = P-CSCF assigned and reachable from Agent A. A liveness proxy, not charon SA state (research.md § R6). |
| `gsm_sip_bridge_vowifi_bridge_failures_total` | Counter | `reason` | 5 closed values (data-model.md § 1) |

## Agent liveness — NEW

| Metric | Type | Labels | Semantics |
|---|---|---|---|
| `gsm_sip_bridge_agent_up` | Gauge | `agent` | 1 = reported within 3× the report interval. `agent` ∈ {`ims`, `sip`}. |
| `gsm_sip_bridge_agent_last_report_seconds` | Gauge | `agent` | Age of the most recent report, computed at scrape time |
| `gsm_sip_bridge_observability_events_dropped_total` | Counter | `agent` | Reports discarded on buffer overflow (FR-019a) |

## Unchanged

`sip_registrations_total`, `sip_registered`, `module_init_total`,
`module_retries_total`, `modules_active`, `modules_failed`,
`audio_errors_total`, `store_writes_total`, `store_queue_depth`,
`uptime_seconds`, `scheduled_restart_total`, `build_info`.

Note `sip_registered` remains the **daemon's** PBX registration. Agent B's PBX
registration is reported separately rather than overwriting it — two processes
writing one gauge would make it meaningless.

---

## Cardinality

Bounded by construction. Per module: `transport` ×2, `status` ×3 — so
`calls_total` tops out at 6 series per module. `bridge_failures_total` is 5
series total, `agent_up` 2. Every label value comes from a closed Rust enum
(data-model.md § 1); no caller number, call-id, or IMSI ever becomes a label
(FR-011b).

---

## Dashboard impact (FR-016)

Existing panels are **not edited**. They query metric names without constraining
`transport`, so they keep working and now include VoWiFi traffic in their totals.
Panels that legend by `module` gain no new series when one modem serves both
transports (same `module` value, and the extra `transport` dimension only splits
series where both transports are actually live).

New panels added additively for User Story 4: VoWiFi registration state, tunnel
state, bridge-failure reasons, and agent liveness.
