//! Discord alerting for critical operational events (specs/022-discord-
//! critical-alerts) — generalizes the existing SMS-to-Discord forwarding
//! (`sms::discord`) into a configurable mechanism covering module/modem
//! lifecycle failure, IMS/SIP registration loss, VoWiFi tunnel failure, and
//! PBX-missed calls, alongside the original SMS category.
//!
//! Every category shares one decision core (this module) and one Discord
//! sender (`alerts::discord::DiscordClient::send_alert`); category-specific
//! *detection* (when a condition has been unhealthy long enough to alert,
//! or has recovered) lives at each category's own call site
//! (`modules::mod`, `metrics::ingest`, `supervise::orchestrate`), since each
//! sits behind a different existing signal (data-model.md).

pub mod discord;

use crate::config::secret::Secret;
use crate::config::CategoryAlertConfig;
use chrono::{DateTime, Utc};

/// Which critical-event category an alert belongs to. A closed enum, not a
/// free string, to keep Prometheus label cardinality bounded — the same
/// convention `control::protocol::ObservedEvent` already uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertCategory {
    /// Existing behavior (specs/006-sms-discord-forward), brought under
    /// this mechanism's config (FR-001) without changing its own delivery
    /// semantics.
    Sms,
    /// SIM absent/unreadable, discovery/initialization failure, or an AT
    /// command worker unresponsive for its configured threshold.
    ModuleLifecycle,
    /// A VoLTE or VoWiFi line's SIP registration lost for its configured
    /// threshold (never for a deliberate/clean unregister).
    RegistrationLoss,
    /// A VoWiFi line's ePDG/IPsec tunnel non-established for its configured
    /// threshold.
    TunnelFailure,
    /// A call that was never bridged (`CallStatus::Missed` only — see spec
    /// Clarifications Q4; `CallStatus::Failed` is out of scope).
    MissedCall,
}

impl AlertCategory {
    /// Prometheus label value and `config.toml` `[alerts.<name>]` sub-table
    /// name.
    pub fn as_str(self) -> &'static str {
        match self {
            AlertCategory::Sms => "sms",
            AlertCategory::ModuleLifecycle => "module_lifecycle",
            AlertCategory::RegistrationLoss => "registration_loss",
            AlertCategory::TunnelFailure => "tunnel_failure",
            AlertCategory::MissedCall => "missed_call",
        }
    }
}

/// Whether this event is the healthy→unhealthy transition or the
/// unhealthy→healthy recovery notice (FR-013). `Sms` and `MissedCall` are
/// one-shot events with no ongoing health state, so they are always
/// `Failure`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CriticalEventKind {
    Failure,
    Recovered,
}

/// The payload passed to the alert dispatcher from any call site.
#[derive(Debug, Clone)]
pub struct CriticalEvent {
    pub category: AlertCategory,
    /// Module id (GSM) or line id (VoWiFi/VoLTE) — whichever identifies the
    /// affected unit for this category.
    pub unit_id: Option<String>,
    /// Human-readable condition, e.g. "SIM unreadable after 5 recovery
    /// attempts" or "caller +91... never answered".
    pub description: String,
    pub at: DateTime<Utc>,
    pub kind: CriticalEventKind,
}

/// Outcome of one alert-dispatch decision, for logging and the
/// `gsm_sip_bridge_critical_alerts_total{category,outcome}` counter
/// (contracts/metrics.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlertOutcome {
    Sent(u16),
    Suppressed,
    Skipped,
    Failed(String),
}

impl AlertOutcome {
    pub fn as_label(&self) -> &'static str {
        match self {
            AlertOutcome::Sent(_) => "sent",
            AlertOutcome::Suppressed => "suppressed",
            AlertOutcome::Skipped => "skipped",
            AlertOutcome::Failed(_) => "failed",
        }
    }
}

/// Resolves the effective webhook for a category: its own override if set,
/// else the shared default, else `None` (FR-008/FR-014).
pub fn resolve_webhook<'a>(
    category_config: &'a CategoryAlertConfig,
    default_webhook: &'a Secret<String>,
) -> Option<&'a str> {
    let candidate = match &category_config.webhook_url_override {
        Some(secret) => secret.expose_secret().as_str(),
        None => default_webhook.expose_secret().as_str(),
    };
    if candidate.is_empty() {
        None
    } else {
        Some(candidate)
    }
}

/// The pure "should we even try to send?" decision (FR-007/FR-014): a
/// category that is disabled, or has no resolvable webhook, is `Skipped`
/// before any network call is attempted. Returns `None` to mean "proceed to
/// send" (the caller then performs the actual Discord POST and records its
/// own `Sent`/`Failed` outcome).
pub fn precheck(
    category_config: &CategoryAlertConfig,
    default_webhook: &Secret<String>,
) -> Option<AlertOutcome> {
    if !category_config.enabled {
        return Some(AlertOutcome::Skipped);
    }
    if resolve_webhook(category_config, default_webhook).is_none() {
        return Some(AlertOutcome::Skipped);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, override_url: Option<&str>) -> CategoryAlertConfig {
        CategoryAlertConfig {
            enabled,
            webhook_url_override: override_url.map(|s| Secret::new(s.to_string())),
        }
    }

    #[test]
    fn resolve_webhook_prefers_override() {
        let default = Secret::new("https://default".to_string());
        let category = cfg(true, Some("https://override"));
        assert_eq!(
            resolve_webhook(&category, &default),
            Some("https://override")
        );
    }

    #[test]
    fn resolve_webhook_falls_back_to_default() {
        let default = Secret::new("https://default".to_string());
        let category = cfg(true, None);
        assert_eq!(
            resolve_webhook(&category, &default),
            Some("https://default")
        );
    }

    #[test]
    fn resolve_webhook_none_when_both_empty() {
        let default = Secret::new(String::new());
        let category = cfg(true, None);
        assert_eq!(resolve_webhook(&category, &default), None);
    }

    #[test]
    fn precheck_skips_disabled_category() {
        let default = Secret::new("https://default".to_string());
        let category = cfg(false, None);
        assert_eq!(precheck(&category, &default), Some(AlertOutcome::Skipped));
    }

    #[test]
    fn precheck_skips_when_no_webhook_resolves() {
        let default = Secret::new(String::new());
        let category = cfg(true, None);
        assert_eq!(precheck(&category, &default), Some(AlertOutcome::Skipped));
    }

    #[test]
    fn precheck_proceeds_when_enabled_with_webhook() {
        let default = Secret::new("https://default".to_string());
        let category = cfg(true, None);
        assert_eq!(precheck(&category, &default), None);
    }
}
