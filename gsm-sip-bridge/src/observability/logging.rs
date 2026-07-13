use tracing::field::{Field, Visit};
use tracing::span;
use tracing_subscriber::layer::Context;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

const REDACTED_FIELD_NAMES: &[&str] = &[
    "password",
    "webhook_url",
    "discord_webhook_url",
    "secret",
    "token",
    "sip.password",
    "sms.discord_webhook_url",
];

const REDACTED_PLACEHOLDER: &str = "[REDACTED]";

/// `level` is the resolved `[logging].level` from config.toml (default
/// `"info"`); `verbose` is the `-v`/`--verbose` CLI flag, which always forces
/// `trace` regardless of config — a manual escape hatch for ad-hoc
/// debugging. `RUST_LOG`, if set, wins over both.
pub fn init(level: &str, verbose: bool) {
    let effective_level = if verbose { "trace" } else { level };
    let default_directive = match effective_level.to_ascii_lowercase().as_str() {
        "trace" => "debug,gsm_sip_bridge=trace,pjsua_safe=debug",
        "debug" => "debug,gsm_sip_bridge=debug,pjsua_safe=debug",
        "warn" => "warn,gsm_sip_bridge=warn",
        "error" => "error,gsm_sip_bridge=error",
        _ => "info,gsm_sip_bridge=info",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_directive));

    tracing_subscriber::registry()
        .with(filter)
        .with(RedactionLayer)
        .with(fmt::layer().with_target(true))
        .init();
}

struct RedactionLayer;

impl<S> Layer<S> for RedactionLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = RedactionVisitor {
            found_sensitive: false,
        };
        event.record(&mut visitor);
    }

    fn on_new_span(&self, _attrs: &span::Attributes<'_>, _id: &span::Id, _ctx: Context<'_, S>) {}
}

struct RedactionVisitor {
    found_sensitive: bool,
}

impl Visit for RedactionVisitor {
    fn record_debug(&mut self, field: &Field, _value: &dyn std::fmt::Debug) {
        if is_sensitive_field(field.name()) {
            self.found_sensitive = true;
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if is_sensitive_field(field.name()) {
            self.found_sensitive = true;
            let _ = value;
            let _ = REDACTED_PLACEHOLDER;
        }
    }
}

fn is_sensitive_field(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    REDACTED_FIELD_NAMES
        .iter()
        .any(|&sensitive| lower.contains(sensitive))
        || lower.starts_with("auth")
}
