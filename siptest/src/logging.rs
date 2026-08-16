//! Mirrors `gsm_sip_bridge::observability::logging::init`: writes to
//! **stderr** so stdout stays the command's actual answer (FR-033), and
//! `-v`/`--verbose` always forces trace regardless of `[logging].level`.
//! Also feeds a bounded in-process ring (`crate::logbuf`) so `GET /log/tail`
//! can answer without the agent locating the daemon's stderr.

use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

pub fn init(level: &str, verbose: bool) {
    let effective = if verbose { "trace" } else { level };
    let default_directive = match effective.to_ascii_lowercase().as_str() {
        "trace" => "debug,siptest=trace",
        "debug" => "debug,siptest=debug",
        "warn" => "warn,siptest=warn",
        "error" => "error,siptest=error",
        _ => "info,siptest=info",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_directive));

    tracing_subscriber::registry()
        .with(filter)
        .with(LogBufLayer)
        .with(fmt::layer().with_target(true).with_writer(std::io::stderr))
        .init();
}

struct LogBufLayer;

impl<S> Layer<S> for LogBufLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let meta = event.metadata();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let line = format!(
            "{}.{:03} {:5} {}: {}",
            now.as_secs(),
            now.subsec_millis(),
            meta.level(),
            meta.target(),
            visitor.message
        );
        crate::logbuf::push(line);
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        }
    }
}
