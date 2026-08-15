//! Mirrors `gsm_sip_bridge::observability::logging::init`: writes to
//! **stderr** so stdout stays the command's actual answer (FR-033), and
//! `-v`/`--verbose` always forces trace regardless of `[logging].level`.

use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

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
        .with(fmt::layer().with_target(true).with_writer(std::io::stderr))
        .init();
}
