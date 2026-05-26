# Config Contract: `[scheduled_restart]`

**Feature**: 010-scheduled-card-restart | **Date**: 2026-05-26

This feature adds one new top-level section to `config.toml`. No existing key changes.

## TOML schema

```toml
[scheduled_restart]
# Master switch. When false, no scheduled cycle ever fires.
# Type: bool. Default: true.
enabled = true

# Standard 5-field cron expression (minute hour day-of-month month day-of-week).
# Evaluated in system local time. Default: every night at 1 AM.
# Examples:
#   "0 1 * * *"      → 1:00 AM every day (DEFAULT)
#   "0 3 * * 0"      → 3:00 AM every Sunday
#   "30 2 * * 1-5"   → 2:30 AM Mon–Fri
#   "0 */6 * * *"    → every 6 hours on the hour
# Invalid expressions disable the scheduler (logged at WARN); the daemon continues.
# Type: string. Default: "0 1 * * *".
cron = "0 1 * * *"

# Symmetric random jitter applied to the cron-tick start time.
# The actual cycle starts at: cron_tick + uniform_random(-start_jitter_seconds, +start_jitter_seconds).
# Set to 0 to disable jitter (deterministic start time).
# Type: integer (0..=86400). Default: 600 (±10 minutes).
start_jitter_seconds = 600

# Base wait between consecutive per-card restarts within a cycle.
# Type: integer (0..=3600). Default: 30 seconds.
inter_card_gap_seconds = 30

# Symmetric random jitter applied to the inter-card gap.
# Effective gap = inter_card_gap_seconds + uniform_random(-jitter, +jitter), clamped at 0.
# MUST be <= inter_card_gap_seconds (validated at startup).
# Type: integer (0..=3600). Default: 15 seconds.
inter_card_gap_jitter_seconds = 15
```

## Field semantics

| Field | Type | Default | Range | Notes |
|-------|------|---------|-------|-------|
| `enabled` | bool | `true` | — | When `false`, scheduler is constructed but never fires. Logged at startup. |
| `cron` | string | `"0 1 * * *"` | Valid 5-field cron | Local time. DST rules per host TZ. Invalid → disable scheduler (no abort). |
| `start_jitter_seconds` | u64 | `600` | `0..=86400` | `0` = no jitter. |
| `inter_card_gap_seconds` | u64 | `30` | `0..=3600` | `0` = no gap (cards restart back-to-back). |
| `inter_card_gap_jitter_seconds` | u64 | `15` | `0..=3600`, `<= inter_card_gap_seconds` | `0` = deterministic gap. |

## Behavior when section omitted entirely

All defaults above apply. The scheduler runs enabled with the documented defaults. A startup log entry confirms this.

## Behavior on validation failure

| Failure | Action | Daemon continues? |
|---------|--------|-------------------|
| Unknown key under `[scheduled_restart]` | WARN log; key ignored. | Yes |
| `cron` value missing | Use default `"0 1 * * *"`. | Yes |
| `cron` value present but unparseable | ERROR log including the offending value; scheduler disabled for process lifetime. | Yes (FR-004) |
| `start_jitter_seconds` out of range | ERROR log; scheduler disabled. | Yes |
| `inter_card_gap_seconds` out of range | ERROR log; scheduler disabled. | Yes |
| `inter_card_gap_jitter_seconds` > `inter_card_gap_seconds` | ERROR log; scheduler disabled. | Yes |
| `enabled = false` | INFO log; scheduler initialized but inactive. | Yes |

## Startup log examples

When enabled with defaults:
```text
INFO scheduled_restart enabled
INFO   cron = "0 1 * * *" (system local time)
INFO   start_jitter = ±600s (±10 min)
INFO   inter_card_gap = 30s ± 15s
INFO   next scheduled cycle at 2026-05-27 01:00:00 +05:30 (±10 min)
```

When disabled via config:
```text
INFO scheduled_restart disabled (enabled = false in config)
```

When invalid cron:
```text
WARN scheduled_restart disabled: cron expression "0 25 * * *" is invalid
       (hour 25 out of range 0-23)
```

## Log entries during cycle execution

All include `cycle_id` (u64) and `slot` (u32) fields for structured filtering.

| Log entry | Level | Fields |
|-----------|-------|--------|
| cycle-start | INFO | `cycle_id`, `cron_tick`, `actual_start`, `pending_slots`, `n_slots` |
| per-card-start | INFO | `cycle_id`, `slot`, `attempt` (initial / deferred-retry) |
| per-card-outcome | INFO (success) / WARN (failed/timedout) / DEBUG (skipped/deferred) | `cycle_id`, `slot`, `attempt`, `outcome`, `reason` (when applicable), `duration_ms` |
| cycle-complete | INFO | `cycle_id`, `total`, `succeeded`, `failed`, `deferred_recovered`, `skipped`, `duration_ms`, `next_cycle_at` |

## Metrics contract

One new Prometheus counter is exposed at the existing metrics endpoint:

```text
# HELP gsm_sip_bridge_scheduled_restart_total Scheduled-restart attempts per slot and outcome
# TYPE gsm_sip_bridge_scheduled_restart_total counter
gsm_sip_bridge_scheduled_restart_total{slot="0",outcome="success"} 14
gsm_sip_bridge_scheduled_restart_total{slot="0",outcome="failed"} 0
gsm_sip_bridge_scheduled_restart_total{slot="0",outcome="deferred-recovered"} 1
gsm_sip_bridge_scheduled_restart_total{slot="0",outcome="skipped-non-ready"} 0
gsm_sip_bridge_scheduled_restart_total{slot="0",outcome="skipped-active-call"} 0
gsm_sip_bridge_scheduled_restart_total{slot="0",outcome="skipped-already-restarted-by-manual"} 1
gsm_sip_bridge_scheduled_restart_total{slot="0",outcome="timed-out"} 0
```

`outcome` label values are exactly: `success`, `failed`, `deferred-recovered`, `skipped-non-ready`, `skipped-active-call`, `skipped-already-restarted-by-manual`, `timed-out`.

`deferred-recovered` counts initial-attempt deferrals whose deferred-retry later succeeded. Pure first-attempt successes increment `success` only.

## Backwards compatibility

- Configs that **omit** `[scheduled_restart]` entirely run with documented defaults. Existing operator config files do not need changes.
- The new section name is added to `TOP_LEVEL_SECTIONS` in `config/mod.rs` so the existing unknown-key warning does not falsely fire on `[scheduled_restart]`.
- No CLI commands change. No control-protocol commands change.
- No database schema change.
