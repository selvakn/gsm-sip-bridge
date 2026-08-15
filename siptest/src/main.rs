//! Thin entry point: parse arguments, bring up logging, then hand off to the
//! library — mirroring `gsm-sip-bridge/src/main.rs`'s shape, for the same
//! reason: a binary crate's items are unreachable from `tests/`.

use std::process::ExitCode;

use siptest::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse_args();
    let level = siptest::config::read_log_level(&cli.config);
    siptest::logging::init(&level, cli.verbose);

    match &cli.command {
        Some(command) => siptest::commands::run(&cli, command),
        None => siptest::daemon::run(&cli.config),
    }
}
