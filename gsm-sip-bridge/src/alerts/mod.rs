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
use crate::config::{AlertsConfig, CategoryAlertConfig};
use chrono::{DateTime, Utc};

/// The value shown in an alert's phone field when no number can be determined,
/// and the ultimate hostname fallback (specs/034-alert-identity, FR-005).
pub const UNKNOWN_IDENTITY: &str = "unknown";

/// The host's system hostname, or [`UNKNOWN_IDENTITY`] if it can't be read.
/// Reads the live kernel hostname from `/proc/sys/kernel/hostname` (Linux
/// target) rather than calling `gethostname(2)`, keeping this crate
/// zero-`unsafe` (tools/count-unsafe.sh, same rationale as
/// `supervise::runner`). Falls back to `$HOSTNAME`, then the literal.
pub fn system_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| UNKNOWN_IDENTITY.to_string())
}

/// The instance label shown in every alert footer (specs/034-alert-identity):
/// the configured `[alerts].instance_name` when non-empty, else the system
/// hostname.
pub fn instance_label(config: &AlertsConfig) -> String {
    match config.instance_name.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => system_hostname(),
    }
}

/// Maps each line's `unit_id` — the `ec20-XXXXXX` card id its agent reports —
/// to its configured phone number, for the daemon-detected alert categories
/// (registration/tunnel/Gm loss) that hold only the `unit_id` and cannot reach
/// the line's own process (specs/034-alert-identity, research R4). Built from
/// the `[[volte.line]]`/`[[vowifi.line]]` overrides carrying both a
/// `modem_serial` and an `msisdn`, since `derive_module_id(modem_serial)` is
/// the exact transform that produced the reported card id.
///
/// Two classes of line are deliberately absent (and so render `unknown`):
/// - lines pinned only by `modem_port`, which has no serial to hash;
/// - PC/SC reader lines, whose `card_id` is `pcsc{i}` — never `ec20-*` — so a
///   `modem_serial`-derived key could not match them anyway.
///
/// `derive_module_id` keeps only the last six alphanumerics uppercased, so two
/// distinct configured serials can collapse to one key. Rather than silently
/// keep whichever the iterator visited last — attributing one card's number to
/// another, which is worse for triage than `unknown` and violates FR-005's
/// "never a fabricated value" — such colliding keys are dropped entirely.
pub fn line_phone_map(
    config: &crate::config::AppConfig,
) -> std::collections::HashMap<String, String> {
    let volte = config
        .volte
        .line_overrides
        .iter()
        .filter_map(|l| Some((l.modem_serial.as_deref()?, l.msisdn.as_deref()?)));
    let vowifi = config
        .vowifi
        .line_overrides
        .iter()
        .filter_map(|l| Some((l.modem_serial.as_deref()?, l.msisdn.as_deref()?)));

    let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut collided: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (serial, msisdn) in volte
        .chain(vowifi)
        .filter(|(serial, msisdn)| !serial.is_empty() && !msisdn.is_empty())
    {
        let card_id = crate::modules::discovery::derive_module_id(serial);
        if collided.contains(&card_id) {
            continue;
        }
        if map.remove(&card_id).is_some() {
            // A second configured serial hashes to the same card id: we cannot
            // tell which number belongs to which card, so neither is safe to
            // show. Drop both and let them fall back to `unknown`.
            collided.insert(card_id);
        } else {
            map.insert(card_id, msisdn.to_string());
        }
    }
    map
}

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
    /// specs/027-discover-retry-health: an explicitly configured VoWiFi
    /// line (`modem_port`/`modem_serial`) never resolved into a line at
    /// all, even after retrying discovery for it.
    LineDiscoveryFailed,
    /// specs/028-gm-tcp-reconnect: a registered line's Gm signaling
    /// connection has been down and unrecoverable for its configured
    /// threshold, despite automatic reconnects and a re-registration.
    GmConnectionLost,
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
            AlertCategory::LineDiscoveryFailed => "line_discovery_failed",
            AlertCategory::GmConnectionLost => "gm_connection_lost",
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
    /// specs/034-alert-identity: the affected card/line's phone number, when
    /// the origin can determine it. `None` renders as `unknown` in the alert
    /// (FR-005) — never a fabricated value.
    pub phone_number: Option<String>,
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

fn category_config(config: &AlertsConfig, category: AlertCategory) -> &CategoryAlertConfig {
    match category {
        AlertCategory::Sms => &config.sms,
        AlertCategory::ModuleLifecycle => &config.module_lifecycle,
        AlertCategory::RegistrationLoss => &config.registration_loss,
        AlertCategory::TunnelFailure => &config.tunnel_failure,
        AlertCategory::MissedCall => &config.missed_call,
        AlertCategory::LineDiscoveryFailed => &config.line_discovery_failed,
        AlertCategory::GmConnectionLost => &config.gm_connection_lost,
    }
}

/// The single entry point every category's call site uses: resolves
/// enabled/webhook (FR-007/FR-008/FR-014), sends via `client` if applicable,
/// and records the outcome in `CRITICAL_ALERTS_TOTAL`/`CRITICAL_EVENT_ACTIVE`
/// plus a structured log line. Intended to be run via
/// `tokio::runtime::Handle::spawn`/`Runtime::spawn` (fire-and-forget, never
/// awaited by a call/SMS/AT-command hot path — FR-011). Returns the outcome
/// so a caller that needs to react to delivery success/failure (e.g.
/// `metrics::ingest`'s Pending→Alerted/Idle transition, Greptile P1) can —
/// most callers just discard it.
pub async fn dispatch(
    client: &discord::DiscordClient,
    config: &AlertsConfig,
    event: CriticalEvent,
) -> AlertOutcome {
    let cfg = category_config(config, event.category);
    let outcome = match precheck(cfg, &config.default_webhook_url) {
        Some(outcome) => outcome,
        None => {
            let webhook = resolve_webhook(cfg, &config.default_webhook_url)
                .expect("precheck already confirmed a webhook resolves")
                .to_string();
            match client.send_alert(&webhook, &event).await {
                Ok(status) => AlertOutcome::Sent(status),
                Err(e) => AlertOutcome::Failed(e),
            }
        }
    };

    crate::metrics::CRITICAL_ALERTS_TOTAL
        .with_label_values(&[event.category.as_str(), outcome.as_label()])
        .inc();
    // Sms/MissedCall are one-shot events with no ongoing health state
    // (data-model.md) — the active gauge only makes sense for the three
    // categories that track a healthy/unhealthy condition over time.
    let tracks_ongoing_health = matches!(
        event.category,
        AlertCategory::ModuleLifecycle
            | AlertCategory::RegistrationLoss
            | AlertCategory::TunnelFailure
            | AlertCategory::LineDiscoveryFailed
            | AlertCategory::GmConnectionLost
    );
    if tracks_ongoing_health {
        if let Some(unit_id) = &event.unit_id {
            let active = match event.kind {
                CriticalEventKind::Failure => 1.0,
                CriticalEventKind::Recovered => 0.0,
            };
            crate::metrics::CRITICAL_EVENT_ACTIVE
                .with_label_values(&[event.category.as_str(), unit_id])
                .set(active);
        }
    }

    match &outcome {
        AlertOutcome::Sent(status) => tracing::info!(
            category = event.category.as_str(),
            unit_id = ?event.unit_id,
            status,
            "critical alert sent"
        ),
        AlertOutcome::Suppressed => tracing::debug!(
            category = event.category.as_str(),
            unit_id = ?event.unit_id,
            "critical alert suppressed (still unhealthy, already alerted)"
        ),
        AlertOutcome::Skipped => tracing::debug!(
            category = event.category.as_str(),
            unit_id = ?event.unit_id,
            description = %event.description,
            "critical alert skipped (category disabled or no webhook configured)"
        ),
        AlertOutcome::Failed(e) => tracing::warn!(
            category = event.category.as_str(),
            unit_id = ?event.unit_id,
            error = %e,
            "critical alert delivery failed"
        ),
    }

    outcome
}

/// Records a suppressed tick (FR-013: still continuously unhealthy, no
/// re-alert) directly in the metric, without attempting delivery — used by
/// `metrics::ingest`'s evaluator for categories it polls on each report.
pub fn record_suppressed(category: AlertCategory) {
    crate::metrics::CRITICAL_ALERTS_TOTAL
        .with_label_values(&[category.as_str(), AlertOutcome::Suppressed.as_label()])
        .inc();
}

/// A cheap, `Clone`-able handle bundling everything a *synchronous* call
/// site (no ambient `tokio::runtime::Handle::current()`) needs to fire an
/// alert without blocking (research.md R3) — `supervise::orchestrate`'s
/// per-line threads build one dedicated `Runtime` at startup and pass this
/// down instead of threading `Handle`/`DiscordClient`/`AlertsConfig`
/// separately through every function in the call chain.
#[derive(Clone)]
pub struct AlertContext {
    client: std::sync::Arc<discord::DiscordClient>,
    config: std::sync::Arc<AlertsConfig>,
    handle: tokio::runtime::Handle,
}

impl AlertContext {
    pub fn new(
        client: discord::DiscordClient,
        config: AlertsConfig,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            client: std::sync::Arc::new(client),
            config: std::sync::Arc::new(config),
            handle,
        }
    }

    /// Fire-and-forget (FR-011): spawns onto the dedicated runtime and
    /// returns immediately, never blocking the calling thread on the
    /// Discord round-trip.
    pub fn fire(&self, event: CriticalEvent) {
        let client = std::sync::Arc::clone(&self.client);
        let config = std::sync::Arc::clone(&self.config);
        self.handle
            .spawn(async move { dispatch(&client, &config, event).await });
    }
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

    // --- specs/034-alert-identity ---

    #[test]
    fn instance_label_prefers_configured_name_and_trims_it() {
        let config = AlertsConfig {
            instance_name: Some("bridge-01".to_string()),
            ..Default::default()
        };
        assert_eq!(instance_label(&config), "bridge-01");

        // Surrounding whitespace must not render padded inside the footer.
        let padded = AlertsConfig {
            instance_name: Some("  bridge-01  ".to_string()),
            ..Default::default()
        };
        assert_eq!(instance_label(&padded), "bridge-01");
    }

    #[test]
    fn instance_label_falls_back_to_hostname_when_unset_or_blank() {
        // Unset ⇒ the system hostname, which is always a non-empty string
        // (`system_hostname` guarantees at least the `"unknown"` fallback).
        assert!(!instance_label(&AlertsConfig::default()).is_empty());

        let blank = AlertsConfig {
            instance_name: Some("   ".to_string()),
            ..Default::default()
        };
        assert_eq!(instance_label(&blank), system_hostname());
    }

    #[test]
    fn system_hostname_is_never_empty() {
        assert!(!system_hostname().is_empty());
    }
}
