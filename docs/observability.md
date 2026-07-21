# Observability

Prometheus metrics, the Grafana dashboard, and what gets persisted to the
call/SMS database.

## Metrics endpoint

Prometheus-compatible metrics are served at `http://<host>:9091/metrics`
(port configurable via `[metrics].port`).

| Metric | Type | Description |
|---|---|---|
| `gsm_sip_bridge_calls_total` | Counter | Inbound calls by module, status, and **transport** (`cs` or `vowifi`) |
| `gsm_sip_bridge_sip_calls_total` | Counter | Outbound SIP calls by module, status, and **transport** |
| `gsm_sip_bridge_call_duration_seconds` | Histogram | Call duration distribution by module and **transport** (1s to 30min buckets) |
| `gsm_sip_bridge_active_calls` | Gauge | Currently active bridged calls per module and **transport** |
| `gsm_sip_bridge_sip_registrations_total` | Counter | SIP registration attempts by status (circuit-switched daemon's own PBX registration) |
| `gsm_sip_bridge_sip_registered` | Gauge | SIP registration state (1=registered, 0=unregistered) |
| `gsm_sip_bridge_module_init_total` | Counter | Module initialization attempts by status |
| `gsm_sip_bridge_module_retries_total` | Counter | Module retry attempts |
| `gsm_sip_bridge_modules_active` | Gauge | Number of active modules |
| `gsm_sip_bridge_modules_failed` | Gauge | Number of failed modules pending retry |
| `gsm_sip_bridge_audio_errors_total` | Counter | Audio errors by module and type |
| `gsm_sip_bridge_sms_received_total` | Counter | SMS messages received per module and **transport** |
| `gsm_sip_bridge_sms_forwarded_total` | Counter | Discord forwarding outcomes per module, outcome, and **transport** |
| `gsm_sip_bridge_sms_db_writes_total` | Counter | SMS database write outcomes |
| `gsm_sip_bridge_store_writes_total` | Counter | All store writes by table and outcome |
| `gsm_sip_bridge_store_queue_depth` | Gauge | Pending items for DB writer thread |
| `gsm_sip_bridge_uptime_seconds` | Gauge | Process uptime in seconds |
| `gsm_sip_bridge_build_info` | Gauge | Build metadata (version, git SHA) |
| `gsm_sip_bridge_vowifi_registered` | Gauge | 1 if the VoWiFi IMS registration is currently held |
| `gsm_sip_bridge_vowifi_registrations_total` | Counter | VoWiFi IMS registration attempts by outcome |
| `gsm_sip_bridge_vowifi_tunnel_up` | Gauge | 1 if Agent A has a P-CSCF assignment and a live transport to it — a liveness proxy, not raw IKE/ESP SA state |
| `gsm_sip_bridge_vowifi_bridge_failures_total` | Counter | Inbound VoWiFi calls that failed to bridge, by reason |
| `gsm_sip_bridge_agent_up` | Gauge | 1 if the named VoWiFi agent (`ims` or `sip`) has reported within the last 3 report intervals |
| `gsm_sip_bridge_agent_last_report_seconds` | Gauge | Age of the named agent's most recent report |
| `gsm_sip_bridge_observability_events_dropped_total` | Counter | Reports an agent's bounded buffer discarded on overflow |

**Transport label**: the six metrics marked **transport** above carry
`transport="cs"` for the circuit-switched daemon and `transport="vowifi"`
for the two VoWiFi agents (`ims::agent`, `vowifi::mod`) — one metric family
per call/SMS concept, with the two paths distinguishable rather than
invisible to each other. Existing dashboard queries that don't constrain
`transport` see the combined total, unchanged in value from before VoWiFi
existed on a circuit-switched-only deployment.

**Module identity across transports**: when the same physical modem serves
both circuit-switched voice and VoWiFi, its `module` label is identical on
both transports (resolved via `modules::discovery::derive_module_id`/
`resolve_module_id_for_port` from the modem's USB serial either way), so a
per-module panel shows that card's complete traffic. VoWiFi metrics fall
back to the literal `module="vowifi"` only if the card's USB serial can't be
resolved.

**How VoWiFi metrics reach this endpoint**: `ims::agent` and `vowifi::mod`
run in separate processes/network namespaces and serve no scrape endpoint
of their own. Each reports call/SMS/health events to the daemon over the
same Unix control socket the CLI uses (`ControlCmd::Observe`), buffered
locally (bounded, 1024 reports) if the daemon is briefly unreachable so a
routine daemon restart doesn't lose in-flight events. See
`specs/014-vowifi-metrics-restore/contracts/observability-protocol.md` for
the wire format.

## Grafana dashboard

The "GSM-SIP Bridge" dashboard is auto-provisioned on first boot of the
Docker Compose stack (credentials: `admin` / `admin`).

![GSM-SIP Bridge Grafana Dashboard](../screenshots/grafana-dashboard.png)

Dashboard panels include:

- System overview (SIP registration, active modules, uptime, call counts)
- GSM and SIP call rates over time
- Active calls per module
- Call duration percentiles (p50/p95/p99)
- SIP registration state timeline
- Module health and retry counts
- Audio and SIP error rates
- SMS forwarding success/failure rates
- VoWiFi IMS registration and ePDG tunnel state
- VoWiFi bridge failures by reason
- VoWiFi agent liveness and dropped-event counts

## Call and SMS database

All incoming calls and SMS messages are persisted to the SQLite database
(WAL mode for concurrent access). In the Docker Compose stack, sqlite-web
serves a read-only browser for it at `http://localhost:8088`. For direct
`sqlite3` queries, pruning, and backup recipes see
[operations.md](operations.md).

**Calls table**:

| Column | Description |
|---|---|
| `module_id` | Card identifier (e.g., `ec20-A1B2C3`) |
| `caller_id` | Caller's phone number |
| `started_at` | ISO 8601 timestamp (UTC) — the moment of answer for an answered call, the moment the call arrived otherwise |
| `duration_seconds` | Call duration in seconds (0.0 for missed/failed calls) |
| `status` | `answered`, `missed`, or `failed` |
| `sip_destination` | SIP extension dialed (empty for missed/failed calls) |
| `transport` | `cs` (circuit-switched) or `vowifi`. Rows written before this column existed were backfilled as `cs` — accurate by construction, since the VoWiFi path had never written a row before it existed. |

**SMS table**:

| Column | Description |
|---|---|
| `module_id` | Card identifier |
| `sender` | SMS sender number |
| `body` | Message text |
| `received_at` | ISO 8601 timestamp (UTC) |
| `forwarding_status` | `pending`, `sent`, `failed`, or `skipped` |
| `transport` | `cs` or `vowifi`, same semantics and backfill as the calls table |

**card_slots table** (IMEI→slot mapping, persisted across restarts):

| Column | Description |
|---|---|
| `slot` | Slot index (0-based, stable for the life of the hardware) |
| `imei` | 15-digit IMEI uniquely identifying the physical modem |
| `assigned_at` | ISO 8601 timestamp when the slot was first assigned |

**card_mode_prefs table** (per-slot network mode preference):

| Column | Description |
|---|---|
| `slot` | Slot index |
| `mode` | Network mode: `auto`, `2g`, `3g`, or `4g` |
| `updated_at` | ISO 8601 timestamp of the last `card set-mode` call |
