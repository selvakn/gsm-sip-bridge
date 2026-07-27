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
struct AgentRecord {
    last_report: Instant,                          // existing
    last_applied: (u64, u64),                       // existing
    registered_unhealthy_since: Option<Instant>,    // new
    registered_alert_phase: AlertPhase,             // new
    tunnel_unhealthy_since: Option<Instant>,        // new
    tunnel_alert_phase: AlertPhase,                 // new
}

enum AlertPhase { Idle, Pending, Alerted }
```

**Revised post-review** (PR #16 Greptile findings, 2026-07-27): the original
design evaluated thresholds from `metrics::server`'s `/metrics` scrape
handler, and marked an incident "alerted" optimistically before delivery was
confirmed. Both were bugs: a line with no Prometheus scraper (or one that
recovered between two scrapes) could have its failure transition evaluated
late or never at all, and a failed Discord delivery left the incident
permanently marked "alerted" — retried never, and a later recovery fired a
`Recovered` notice for a `Failure` the operator was never actually told
about.

The fix moves evaluation into `apply_report` itself (`AlertPhase` replaces
the earlier plain `bool`), so it runs at the real report cadence
(`[metrics].agent_report_interval_seconds`) regardless of whether anything
scrapes `/metrics`, and splits "is this unhealthy" from "have we told
anyone yet":

- `apply_state` still owns `*_unhealthy_since` exactly as before (set on
  first `Some(false)`/`Some(false)` tunnel report, cleared the moment it
  reports healthy again).
- The pure `decide_transition(unhealthy_since, &mut phase, threshold)`,
  called for each signal right after `apply_state` (same lock, so two
  reports for the same key can't race into deciding the same
  threshold-crossing twice): `Idle` → `Pending` + a `Failure` event once the
  streak crosses threshold; `Pending` stays `Pending` while dispatch is in
  flight (no double-send); `Alerted` + still unhealthy → `Suppressed`
  (FR-013); `Alerted` + healthy → back to `Idle` + a `Recovered` event.
- `record_alert_outcome`, called from the dispatch task once it resolves,
  moves `Pending` → `Alerted` on confirmed delivery or back to `Idle` on
  failure — so a failed send is retried on the next unhealthy report instead
  of the incident going silently and permanently dark. `Recovered`
  transitions commit synchronously regardless of delivery outcome (a lost
  "all clear" is far less costly than a lost failure notice).

module_lifecycle's SIM path and missed-call/AT-worker still fire directly
at their own call sites, unaffected — they already have a precise "just
now" trigger point, not a polled duration.

## Alert Delivery outcome (log + metric only, no DB row)

```rust
pub enum AlertOutcome { Sent(u16), Suppressed, Skipped, Failed(String) }
```

Recorded via `tracing` (structured fields: category, unit_id, outcome) and
`metrics::CRITICAL_ALERTS_TOTAL{category, outcome}` (research.md R6) —
not persisted to SQLite (research.md R5).
