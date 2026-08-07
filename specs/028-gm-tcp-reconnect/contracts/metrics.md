# Contract: Metrics & Alert Evaluation

**Feature**: `028-gm-tcp-reconnect`

## New gauge

Registered in `metrics::mod`, beside `VOWIFI_REGISTERED` and
`VOWIFI_TUNNEL_UP`.

```
gsm_sip_bridge_vowifi_gm_connection_up{module="<line id>"}  0 | 1
```

| Property | Value |
|---|---|
| Type | `GaugeVec` |
| Labels | `module` — the same per-line id every other Agent A gauge uses (`card_id` in specs/013 terms). No new label vocabulary. |
| `1` | The line's Gm client connection last round-tripped a liveness probe, **and** its Gm server listener's accept loop is alive. |
| `0` | Either half is down — reconnecting or failed. |
| Absent | The line has never reported. Absence is not `0`; a monitor must distinguish "no data" from "down". |

**Naming note**: the `vowifi_` prefix is retained despite the metric also
covering VoLTE lines, matching the existing precedent already documented at
`ingest.rs:396` for `gsm_sip_bridge_vowifi_tunnel_up{module="volte"}`.
Introducing a second prefix for the same concept would fragment the
vocabulary; the `module` label is what distinguishes transports.

**Written by**: `metrics::ingest`, at scrape time, from the reported
`AgentState.gm_connection_up`. **Never** written in Agent A's own process —
Agent A serves no `/metrics` endpoint, so a direct write lands in a registry
nothing scrapes (`protocol.rs:128-133`).

## Ingest state

Added to the per-module liveness record in `metrics::ingest`, mirroring the
existing `registered_*` / `tunnel_*` pairs exactly:

| Field | Type | Purpose |
|---|---|---|
| `gm_connection_unhealthy_since` | `Option<Instant>` | Set when a report first carries `gm_connection_up: Some(false)`; cleared on `Some(true)`. `None` in the report means *not reported* and MUST leave the field unchanged. |
| `gm_connection_alert_phase` | `AlertPhase` | Carried across reports, initialised from `existing.map_or(AlertPhase::Idle, ...)` like its siblings. |

## Alert wiring — four required edits

The `AlertPhase` machine (`ingest.rs:241-256`) supplies FR-015's
one-alert-per-episode and FR-016's paired recovery for free. Wiring it up
means four edits, **two of which fail loudly or silently if missed**:

1. **`decide_transition` call** in the `ALERTS_CONFIG` block
   (`ingest.rs:174-208`) — a third `match decide_transition(...)` arm pushing
   `AlertCategory::GmConnectionLost` into `pending_transitions`.

2. **`description` match** (`ingest.rs:297-313`) — ⚠️ this match ends in
   `_ => unreachable!("only RegistrationLoss/TunnelFailure transitions are
   produced here")`. Producing a `GmConnectionLost` transition without
   extending it **panics the ingest path**. Required arms:
   - `Failure`: `"{module} line's carrier signaling connection down for over {n}s"`
   - `Recovered`: `"{module} line's carrier signaling connection re-established"`

3. **`record_alert_outcome` match** (`ingest.rs:355-366`) — ⚠️ this one ends in
   `_ => return`, so a missing arm fails *silently*: the phase never advances
   `Pending → Alerted`, and the recovery notice therefore never fires.
   Symptom would be "failure alert works, recovery alert never arrives."
   Must return `(&mut record.gm_connection_alert_phase,
   record.gm_connection_unhealthy_since)`.

4. **Critical-alert allowlist** (`alerts/mod.rs:190`) — add
   `AlertCategory::GmConnectionLost` to the `matches!` set alongside
   `ModuleLifecycle | RegistrationLoss | TunnelFailure | LineDiscoveryFailed`.

## Threshold

| Item | Value |
|---|---|
| Config path | `alerts.gm_connection_lost.unhealthy_sec` |
| Default | `300` — same as `registration_loss` and `tunnel_failure` (`config/mod.rs:451,466`) |
| Validated range | `30..=3600`, via `in_range_or_warn`, matching `build.rs:463` |

300s is comfortably longer than detection (~130s) plus three reconnect attempts
plus a forced re-registration, so a drop that self-heals never alerts (FR-014,
SC-008).

## Test assertions

- Gauge appears with the correct `module` label for both a VoWiFi and a VoLTE
  line, and reads `0` while reconnecting.
- `gm_connection_up: None` in a report leaves both the gauge and
  `gm_connection_unhealthy_since` unchanged (older-peer compatibility).
- One sustained failure episode produces exactly one `Failure` and, on
  recovery, exactly one `Recovered` — repeated unhealthy reports in between
  produce `Transition::Suppressed`, not additional sends.
- A failure shorter than the threshold produces no transition at all.
- `record_alert_outcome` advances the new category's phase (regression guard
  for edit 3 above).
