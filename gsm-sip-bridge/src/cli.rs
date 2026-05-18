use clap::{Parser, Subcommand};
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
    #[arg(short = 'c', long = "config", default_value = "")]
    pub config: PathBuf,

    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,

    #[arg(short = 's', long = "serial", requires = "audio")]
    pub serial: Option<PathBuf>,

    #[arg(short = 'a', long = "audio", requires = "serial")]
    pub audio: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Manage GSM cards
    Card(CardArgs),
}

#[derive(Parser, Debug)]
pub struct CardArgs {
    #[command(subcommand)]
    pub subcommand: CardSubcommand,
}

#[derive(Subcommand, Debug)]
pub enum CardSubcommand {
    /// Restart a card slot (reset give-up state and re-initialize)
    Restart {
        #[arg(long, short)]
        slot: u32,
    },
    /// Set the network mode for a slot (2g, 3g, 4g, auto)
    SetMode {
        #[arg(long, short)]
        slot: u32,
        #[arg(long, short)]
        mode: String,
    },
    /// Get the stored network mode preference for a slot
    GetMode {
        #[arg(long, short)]
        slot: u32,
    },
    /// List all known card slots and their current state
    List,
}

impl Cli {
    pub fn parse_args() -> Self {
        Self::parse()
    }
}
