# Data Model: Discord Alerts for Critical Events

No new SQLite tables (research.md R5). The entities below are in-process Rust
types, split across the existing `config`, `control::protocol`,
`metrics::ingest`, and a new `alerts` module.

## AlertCategory

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertCategory {
    Sms,               // existing behavior, brought under this mechanism (FR-001)
    ModuleLifecycle,   // SIM absent/unreadable, discovery failure, AT worker unresponsive
    RegistrationLoss,  // IMS/SIP registration lost, VoLTE or VoWiFi
    TunnelFailure,     // VoWiFi ePDG/IPsec tunnel failure
    MissedCall,        // CallStatus::Missed only (spec Clarifications Q4)
}
```

`as_str()` gives the Prometheus label / config sub-table name
(`sms`, `module_lifecycle`, `registration_loss`, `tunnel_failure`,
`missed_call`), matching the existing `CallStatus::as_str()` convention.

## Config: `AlertsConfig` (new, `config/mod.rs`)

```rust
pub struct AlertsConfig {
    /// Shared default webhook, used by any category without its own override.
    pub default_webhook_url: Secret<String>,
    pub sms: CategoryAlertConfig,
    pub module_lifecycle: CategoryAlertConfig,
    pub registration_loss: CategoryAlertConfig,
    pub tunnel_failure: CategoryAlertConfig,
    pub missed_call: CategoryAlertConfig,
}

pub struct CategoryAlertConfig {
    pub enabled: bool,
    /// `None` → use `AlertsConfig::default_webhook_url`.
    pub webhook_url_override: Option<Secret<String>>,
}
```

Defaults (FR-006/FR-003/FR-005/FR-006-registration): `sms.enabled = true`
(unchanged); `module_lifecycle.enabled = registration_loss.enabled =
tunnel_failure.enabled = missed_call.enabled = false`. If `[sms]` sets
`discord_webhook_url`/`enabled` and `[alerts.sms]` is absent, those values
seed `alerts.sms` (backward compatibility, research.md R7).

Category-specific thresholds live alongside, not nested in
`CategoryAlertConfig` (only module_lifecycle and tunnel_failure/
registration_loss need one each):

```rust
pub struct ModuleLifecycleThresholds {
    /// FR-003. Default 60s.
    pub at_worker_unresponsive_sec: u64,
}
pub struct TunnelFailureThresholds {
    /// FR-005. Default 300s.
    pub unhealthy_sec: u64,
}
pub struct RegistrationLossThresholds {
    /// FR-006. Default 300s.
    pub unhealthy_sec: u64,
}
```

## CriticalEvent (new, `alerts` module)

The payload passed to the alert dispatcher from any of the five call sites.

```rust
pub struct CriticalEvent {
    pub category: AlertCategory,
    /// Module id (GSM) or line id (VoWiFi/VoLTE) — whichever identifies the
    /// affected unit for this category. `None` only for categories with no
    /// natural per-unit identity (none currently — kept `Option` so a future
    /// category isn't forced to invent one).
    pub unit_id: Option<String>,
    /// Human-readable condition, e.g. "SIM unreadable after 5 recovery
    /// attempts" or "caller +91... never answered".
    pub description: String,
    pub at: chrono::DateTime<chrono::Utc>,
    /// Whether this is the healthy→unhealthy transition or the
    /// unhealthy→healthy recovery notice (FR-013). `MissedCall` and `Sms`
    /// are one-shot events and are always `Failure` (they have no ongoing
    /// health state to recover from).
    pub kind: CriticalEventKind,
}

pub enum CriticalEventKind {
    Failure,
    Recovered,
}
```

## Health tracking: `metrics::ingest` additions

Extends the existing per-`(AgentKind, module_id)` liveness record
(`ingest.rs`) with two `Option<Instant>` fields, set/cleared as each
`AgentReport.state.{registered,tunnel_up}` arrives:

```rust
struct LivenessRecord {
    last_report: Instant,        // existing
    state: AgentState,           // existing (latest snapshot)
    registered_unhealthy_since: Option<Instant>,   // new
    tunnel_unhealthy_since: Option<Instant>,       // new
}
```

Transition rule (applied in `apply_report`, mirroring research.md R4):
- `registered` observed `Some(false)` and `registered_unhealthy_since` is
  `None` → set it to `Instant::now()`.
- `registered` observed `Some(true)` and `registered_unhealthy_since` is
  `Some(_)` → this is the recovery point: if the elapsed time had already
  crossed the threshold (an alert was sent), emit a `Recovered` event; clear
  the field either way.
- Same rule, independently, for `tunnel_up` / `tunnel_unhealthy_since`.

A companion evaluator, `evaluate_critical_alerts(thresholds) ->
Vec<CriticalEvent>`, mirrors `evaluate_liveness`: called on each scrape/tick,
it reads the current records, and for any `*_unhealthy_since` whose elapsed
time has just crossed its threshold since the last evaluation, yields one
`Failure` event (module_lifecycle's SIM path and missed-call/AT-worker do
not go through this evaluator — they fire directly at their own call sites,
since they already have a precise "just now" trigger point rather than a
polled duration).

## Alert Delivery outcome (log + metric only, no DB row)

```rust
pub enum AlertOutcome { Sent(u16), Suppressed, Skipped, Failed(String) }
```

Recorded via `tracing` (structured fields: category, unit_id, outcome) and
`metrics::CRITICAL_ALERTS_TOTAL{category, outcome}` (research.md R6) —
not persisted to SQLite (research.md R5).
