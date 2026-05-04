use clap::Parser;
use std::path::PathBuf;

const AFTER_LONG_HELP: &str = r#"ENVIRONMENT:
    METRICS_PORT             Override the metrics HTTP port (default: 9091)
    RUST_LOG                 Standard tracing-subscriber filter

For configuration reference, see docs/configuration.md.
For the v4.1.x -> v5.0.0 migration, see docs/migrating-from-v4.1.x.md."#;

#[derive(Parser, Debug)]
#[command(
    name = "gsm-sip-bridge",
    version,
    about = "Bridges incoming GSM calls on Quectel EC20 modules to a SIP extension.",
    after_long_help = AFTER_LONG_HELP
)]
pub struct Cli {
    #[arg(short = 'c', long = "config")]
    pub config: PathBuf,

    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,

    #[arg(short = 's', long = "serial", requires = "audio")]
    pub serial: Option<PathBuf>,

    #[arg(short = 'a', long = "audio", requires = "serial")]
    pub audio: Option<String>,
}

impl Cli {
    pub fn parse_args() -> Self {
        Self::parse()
    }
}
