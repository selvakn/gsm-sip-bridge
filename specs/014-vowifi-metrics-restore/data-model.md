# Phase 1 Data Model: Restore Call and SMS Observability Under VoWiFi

Three layers change: the in-flight event types agents send, the metric surface
the daemon exports, and the SQLite schema.

---

## 1. Event types (agent → daemon)

New types in `gsm-sip-bridge/src/control/protocol.rs`, carried by the new
`ControlCmd::Observe` variant. Wire format in
[contracts/observability-protocol.md](contracts/observability-protocol.md).

### `AgentReport`

The single message an agent sends. Every field except `agent` and `state` is
optional, so a plain heartbeat is a small message.

| Field | Type | Notes |
|---|---|---|
| `agent` | `AgentKind` | `ims` (Agent A) or `sip` (Agent B). Identifies the reporter for liveness tracking. |
| `module_id` | `String` | Resolved per R7. Label on every metric this report feeds. |
| `state` | `AgentState` | Absolute gauge state, always present — this is what makes the report a heartbeat. |
| `events` | `Vec<ObservedEvent>` | Counter deltas since the last report. Empty on a pure heartbeat. |
| `dropped` | `u64` | Reports discarded by this agent's buffer since the last successful send (FR-019a). Cumulative delta, not absolute. |

### `AgentState` (absolute; gauges)

| Field | Type | Applies to | Meaning |
|---|---|---|---|
| `active_calls` | `u32` | Agent A | VoWiFi calls currently bridged |
| `registered` | `Option<bool>` | Agent A | IMS registration currently held (FR-012) |
| `tunnel_up` | `Option<bool>` | Agent A | Per R6 (FR-013) |
| `pbx_registered` | `Option<bool>` | Agent B | Agent B's own PBX registration |

`Option` is "this agent does not report this signal", distinct from `Some(false)`
meaning "reports it, and it is down". The daemon never invents a value for `None`.

### `ObservedEvent` (deltas; counters and histogram observations)

| Variant | Fields | Feeds |
|---|---|---|
| `CallCompleted` | `status: CallStatus`, `duration_seconds: f64` | `calls_total{transport="vowifi"}`, `call_duration_seconds` (only when `status = Answered`) |
| `PbxLegCompleted` | `outcome: String` (bounded set) | `sip_calls_total{transport="vowifi"}` |
| `BridgeFailed` | `reason: BridgeFailureReason` | `vowifi_bridge_failures_total` |
| `SmsReceived` | — | `sms_received_total{transport="vowifi"}` |
| `SmsForwarded` | `outcome: SmsOutcome` | `sms_forwarded_total{transport="vowifi"}` |
| `RegistrationAttempt` | `status: RegistrationStatus` | `vowifi_registrations_total` |

### Bounded enumerations (FR-014 — label cardinality)

These are Rust enums, not free strings, which is what makes the cardinality bound
a compile-time property rather than a convention:

- `CallStatus` = `Answered | Missed | Failed`
  (matches the existing `calls.status` CHECK constraint exactly)
- `BridgeFailureReason` = `BridgeSetupFailed | RingTimeout | CallerCancelled | PbxDeclined | AgentUnreachable`
- `RegistrationStatus` = `Success | AuthFailed | Rejected | Timeout`
- `SmsOutcome` = `Sent | Failed`

Free-text reasons from the existing `ControlMessage::BridgeFailed { reason }` are
**mapped** into `BridgeFailureReason` at the reporting site. Anything unmapped
becomes `BridgeSetupFailed`, so an unrecognised carrier string can never mint a
new series.

---

## 2. Metric surface

Full inventory in [contracts/metrics-inventory.md](contracts/metrics-inventory.md).
Summary of the change:

**Six existing vecs gain a `transport` label** (values: `cs`, `vowifi`):
`calls_total`, `sip_calls_total`, `call_duration_seconds`, `active_calls`,
`sms_received_total`, `sms_forwarded_total`.

**Seven new metrics**:

| Metric | Type | Labels | Requirement |
|---|---|---|---|
| `gsm_sip_bridge_vowifi_registered` | Gauge | — | FR-012 |
| `gsm_sip_bridge_vowifi_registrations_total` | Counter | `status` | FR-012 |
| `gsm_sip_bridge_vowifi_tunnel_up` | Gauge | — | FR-013 |
| `gsm_sip_bridge_vowifi_bridge_failures_total` | Counter | `reason` | FR-014 |
| `gsm_sip_bridge_agent_up` | Gauge | `agent` | FR-021b |
| `gsm_sip_bridge_agent_last_report_seconds` | Gauge | `agent` | FR-021b |
| `gsm_sip_bridge_observability_events_dropped_total` | Counter | `agent` | FR-019a |

### Daemon-side liveness state

Not a metric — in-memory state behind the registry, one entry per `AgentKind`:

| Field | Type | Purpose |
|---|---|---|
| `last_report` | `Instant` | Set on every accepted report |
| `reported_gauges` | list of `(metric, labels)` | What to zero when this agent expires |

Evaluated at scrape time (R5): older than 3× the report interval ⇒ `agent_up` = 0
and every gauge that agent owns is zeroed. Counters are never touched by expiry —
they are cumulative and must not move backwards (FR-020).

---

## 3. SQLite schema: v2 → v3

Current version is `2` (`store/schema.rs`, `SCHEMA_VERSION`). This feature adds
v3.

```sql
ALTER TABLE calls ADD COLUMN transport TEXT NOT NULL DEFAULT 'cs'
    CHECK (transport IN ('cs','vowifi'));
ALTER TABLE sms   ADD COLUMN transport TEXT NOT NULL DEFAULT 'cs'
    CHECK (transport IN ('cs','vowifi'));

CREATE INDEX IF NOT EXISTS idx_calls_transport ON calls(transport);
CREATE INDEX IF NOT EXISTS idx_sms_transport   ON sms(transport);

DROP VIEW IF EXISTS recent_calls;
DROP VIEW IF EXISTS recent_sms;
-- recreated with the transport column appended
```

**Backfill (FR-011c)**: the `DEFAULT 'cs'` clause populates every pre-existing row
during `ALTER TABLE` — no separate data-migration pass, and no window where a row
has a NULL transport. This is accurate by construction: the VoWiFi path has never
written a row.

**Views must be dropped and recreated**, not left alone — they are declared with
`CREATE VIEW IF NOT EXISTS`, so an existing database would silently keep the old
column list forever.

**Upgrade safety (FR-011d)**: `ALTER TABLE ... ADD COLUMN` is in-place and
O(1) in SQLite; existing rows and indices are untouched. The migration follows the
existing `match version.as_str()` ladder in `init_schema`, extended with a `"2"`
arm that runs v3 and falls through, so a v1 database still upgrades all the way in
one open.

### Entity changes

`store::calls::CallRecord` and `store::sms::SmsRecord` each gain a `transport`
field. `CallRecord` is currently written only by the daemon; Agent A becomes a
second writer (R3) against the same WAL database — the same multi-writer pattern
`vowifi/mod.rs` already documents and uses for SMS.

---

## 4. Configuration

One new optional key:

```toml
[metrics]
port = 9091
# How often each VoWiFi agent re-reports its state. Also sets the staleness
# threshold (3x this) after which an agent is marked down.
agent_report_interval_seconds = 10
```

Must be added to `METRICS_KEYS` in `config/mod.rs` so the unknown-key warning
does not fire on it, and to `config.toml.example` with the comment above.
