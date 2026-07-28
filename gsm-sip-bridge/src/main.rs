//! Thin entry point: parse arguments, bring up logging, then hand off to the
//! library. Every subcommand handler lives in `gsm_sip_bridge::commands` (and
//! the no-subcommand daemon in `commands::daemon`) so it is reachable from
//! `tests/`, which nothing in a binary crate can be.

use gsm_sip_bridge::cli::Cli;
use gsm_sip_bridge::commands;
use gsm_sip_bridge::observability::logging;
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = Cli::parse_args();

    // Read [logging].level ahead of the full config load (which may
    // legitimately fail, e.g. an unset secret env var) so logging is set up
    // before anything else runs.
    let log_level = cli
        .config
        .as_deref()
        .map(gsm_sip_bridge::config::read_log_level)
        .unwrap_or_else(|| "info".to_string());
    logging::init(&log_level, cli.verbose);

    match &cli.command {
        Some(command) => commands::run(&cli, command),
        None => commands::daemon::run(&cli),
    }
}
