# Data Model: Read-only management web UI

**Feature**: `122-management-webui`

## Persisted (SQLite, schema v5 → v6)

The migration follows the existing pattern in `store/schema.rs`: a `MIGRATIONS` entry plus a `SCHEMA_VERSION` bump. It is applied by whichever process opens the store first; the other processes see v5 and skip it.

### `calls` (altered)

| Column | Type | Change | Notes |
|---|---|---|---|
| `id` | INTEGER PK | — | |
| `module_id` | TEXT | — | The line that carried the call, or `''` when the call was refused before a line was chosen (outbound only) |
| `caller_id` | TEXT | **meaning widened** | The remote party: the caller for inbound calls, the dialled number for outbound ones |
| `started_at` | TEXT (RFC 3339, UTC) | — | Inbound: when ringing began. Outbound: when the request was accepted |
| `duration_seconds` | REAL | — | Answered calls only; 0 otherwise |
| `status` | TEXT | — | `answered` / `missed` / `failed` |
| `sip_destination` | TEXT | — | |
| `transport` | TEXT | — | `cs` / `vowifi` / `volte` |
| `direction` | TEXT NOT NULL DEFAULT `'unknown'` | **new** | CHECK in (`inbound`, `outbound`, `unknown`). Every earlier row backfills as `unknown`: the CS worker and the IMS agent both recorded calls this bridge placed, and nothing stored says which. Every new row names its direction |
| `loss_percent` | REAL NULL | **new** | Local receive loss at teardown; NULL when unknown or CS |
| `jitter_ms` | REAL NULL | **new** | Local receive jitter at teardown |
| `rtt_ms` | REAL NULL | **new** | Round-trip time from the far end's RTCP |

New index: `idx_calls_direction ON calls(direction)`. The `recent_calls` view is dropped and recreated with the new columns, the same reason v3/v4 recreated it.

**Status mapping for outbound calls** (research R7):

| Outcome | `status` | Recorded? |
|---|---|---|
| `Placed`, then the call ends | `answered` (duration = answer → end) | yes |
| `Unanswered` | `missed` | yes |
| `CallerAbandoned` | `missed` | yes |
| `RefusedNetworkFailure`, committed carrier rejection | `failed` | yes |
| `RefusedNoIdleLine` | `failed` | yes, with `module_id = ''` |
| `RefusedInvalidDestination` from a trusted source | `failed` | yes |
| Request from an untrusted source (403) | — | **no** (not a call; scanner noise) |

### `events` (new)

| Column | Type | Notes |
|---|---|---|
| `id` | INTEGER PK AUTOINCREMENT | |
| `at` | TEXT (RFC 3339, UTC) | When the condition was observed |
| `module_id` | TEXT NULL | The line or module; NULL for bridge-wide events |
| `kind` | TEXT NOT NULL | Closed set, see below |
| `severity` | TEXT NOT NULL | CHECK in (`info`, `warning`, `critical`) |
| `detail` | TEXT NOT NULL | Human-readable, at most 500 chars (truncated) |
| `raise_id` | INTEGER NULL | On a `*_resolved` row, the `id` of the raise it closes |
| `duration_seconds` | REAL NULL | On a `*_resolved` row, `at − raise.at` |

Indexes: `idx_events_at ON events(at)` and `idx_events_module ON events(module_id)`.

**`kind` values** (a closed Rust enum `store::events::EventKind`; `as_str` gives the stored text):

| Kind | Source | Severity |
|---|---|---|
| `alert_raised:<category>` | `alerts::dispatch`, `Failure` | critical |
| `alert_resolved:<category>` | `alerts::dispatch`, `Recovered` | info |
| `tunnel_recovered` | `ingest::apply_event` `TunnelRecovered` (detail: reason, P-CSCFs if any) | warning |
| `dead_peer_loss` | the same event when `dead_peer = Some(Real)` | warning |
| `uplink_hold_started` | `UplinkHoldStarted` (detail: cause) | warning |
| `uplink_hold_ended` | `UplinkHoldEnded` (detail: cause, `ended_by`; `duration_seconds = held_secs`) | info |
| `module_initialized` | `modules/pool` `MODULE_INIT_TOTAL{result="success"}` sites | info |
| `module_init_failed` | `MODULE_INIT_TOTAL{result="failure"}` sites | warning |
| `scheduled_restart` | `modules/pool/restart_cycle.rs` | info |

`<category>` is one of the `AlertCategory::as_str()` values, **excluding** `sms` and `missed_call` (spec FR-050).

**Raise/resolve state machine** (in-memory per process, keyed by `(category, module_id)`):

```text
            Failure                  Recovered
 (none) ──────────────▶ open(raise_id) ──────────────▶ (none)
   │  Recovered: ignored      │  Failure: ignored (dedup of retried delivery)
```

**Retention**: rows where `at < now − [alerts].event_retention_days` are deleted at daemon start and every 6h.

### `sms` (unchanged)

The SMS page reads `module_id`, `sender`, `body`, `received_at`, `forwarding_status`, `forwarded_at` and `transport` as they are.

## In memory (daemon)

### `SignalReading`

Kept as the latest value per line; history lives in Prometheus.

| Field | Type | Notes |
|---|---|---|
| `at_unix` | u64 | When it was read |
| `rssi_dbm` | Option<i32> | From `AT+CSQ`: `-113 + 2·rssi`; `None` for 99 |
| `rat` | Option<String> | e.g. `LTE`, `WCDMA`, `GSM`, from `AT+QENG` |
| `rsrp_dbm` | Option<i32> | LTE only |
| `rsrq_db` | Option<i32> | LTE only |
| `sinr_db` | Option<i32> | LTE only |

Sources:
- CS lines: written by the `ModuleWorker` into `webui::signal` (a `Mutex<HashMap<module_id, SignalReading>>`).
- VoWiFi/VoLTE lines: arrive in `AgentReport.details.signal` and are kept in `ingest::details()`.

### `ActiveCallInfo`

Arrives in `AgentReport.details.active_call_details`, or is built from `CardPool` for CS lines.

| Field | Type | Notes |
|---|---|---|
| `remote` | String | The caller or the dialled number |
| `direction` | `inbound` / `outbound` | |
| `started_at_unix` | u64 | |
| `answered` | bool | Ringing vs. connected |
| `loss_percent` | Option<f64> | Latest snapshot |
| `jitter_ms` | Option<f64> | |
| `rtt_ms` | Option<f64> | |

At most 4 entries per report, to keep reports bounded.

### `LineView`

Assembled per request, not stored.

| Field | Source |
|---|---|
| `id`, `index` | Ingest key `module_id` / CS slot number |
| `transport` | `cs` / `vowifi` / `volte`, from the agent kind (`ingest::transport_label`) |
| `carrier` | PLMN → operator name from `details.plmn` (new; MCC/MNC is shown when the name is unknown). CS: `SlotInfo.network` |
| `msisdn` | `alerts::line_phone_map` or the CS slot `phone`; the client masks it to the last 4 digits |
| `registration_state` | `details.registration_state` (`ims::RegistrationState` Display) |
| `registered_at`, `expires_at` | Ingest record |
| `tunnel_up`, `child_sa_up`, `gm_connection_up`, `uplink` | Ingest records (IMS + Supervisor) |
| `pdn_up` | `VOLTE_PDN_UP` gauge |
| `active_calls` | Count + `ActiveCallInfo[]` |
| `signal` | `SignalReading` or `null` + `signal_note` (`not_available_pcsc`, `not_polled_yet`) |
| `last_report_age_seconds`, `stale` | `ingest::evaluate_liveness` |
| `health` | Derived: `ok` / `degraded` / `down` / `stale`. The reasons are listed so the card can name the failing component (US1, scenario 2) |

**Health derivation** (pure function, unit-tested):
- `stale` if any of the line's agents is stale;
- otherwise `down` if not registered, or if the tunnel or child SA is down (VoWiFi), or the PDN is down (VoLTE);
- otherwise `degraded` if Gm is down, the uplink is held, or the tunnel is flapping;
- otherwise `ok`.

### `Session`

`HashMap<token, Instant /* expiry */>` inside `webui::auth::Sessions`, plus `HashMap<IpAddr, (failures, locked_until)>`, bounded at 1024 entries.
