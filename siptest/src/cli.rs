use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// siptest — a SIP softphone for agent-driven end-to-end testing of the
/// bridge (specs/037-siptest-softphone). With no subcommand, runs the
/// long-lived daemon: registers to the bridge, serves the control API, and
/// answers/places calls as instructed over it.
#[derive(Parser, Debug)]
#[command(name = "siptest", version, about)]
pub struct Cli {
    /// Path to siptest.toml. Required for the daemon; also read by the
    /// client subcommands below to find `[api].bind`.
    #[arg(long, default_value = "siptest.toml")]
    pub config: PathBuf,

    /// Force trace-level logging, overriding `[logging].level`.
    #[arg(short, long)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Place an outbound call through a running siptest daemon and print its
    /// report. Exits 0 only when the call met the configured `require` level
    /// — an answered-but-silent call is a failure (FR-032).
    Call {
        #[arg(long)]
        destination: String,
        #[arg(long)]
        duration_secs: Option<u64>,
        /// `auto` | `pcmu` | `g722`. Defaults to `[media].codec` when omitted.
        #[arg(long)]
        codec: Option<String>,
        /// Block until the call reaches a terminal state (always on for this
        /// subcommand; kept as a flag for symmetry with the API's `?wait=`).
        #[arg(long, default_value_t = true)]
        wait: bool,
    },
    /// Print a running daemon's `/status`.
    Status,
}

impl Cli {
    pub fn parse_args() -> Self {
        Self::parse()
    }
}
