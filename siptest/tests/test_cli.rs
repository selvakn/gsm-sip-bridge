//! Pure-parse CLI tests, mirroring `gsm-sip-bridge/tests/test_cli.rs` — no
//! process spawning, just `Cli::try_parse_from`.

use clap::error::ErrorKind;
use clap::Parser;
use siptest::cli::{Cli, Commands};

#[test]
fn help_renders_without_panicking() {
    let result = Cli::try_parse_from(["siptest", "--help"]);
    let err = result.expect_err("--help should short-circuit as a clap error");
    assert_eq!(err.kind(), ErrorKind::DisplayHelp);
}

#[test]
fn call_help_renders_without_panicking() {
    let result = Cli::try_parse_from(["siptest", "call", "--help"]);
    let err = result.expect_err("call --help should short-circuit as a clap error");
    assert_eq!(err.kind(), ErrorKind::DisplayHelp);
}

#[test]
fn no_subcommand_runs_the_daemon_with_config_defaulting_to_siptest_toml() {
    let cli = Cli::try_parse_from(["siptest"]).unwrap();
    assert!(cli.command.is_none());
    assert_eq!(cli.config.to_str().unwrap(), "siptest.toml");
    assert!(!cli.verbose);
}

#[test]
fn call_subcommand_parses_destination_and_optional_duration() {
    let cli = Cli::try_parse_from([
        "siptest",
        "call",
        "--destination",
        "+919000000000",
        "--duration-secs",
        "45",
    ])
    .unwrap();
    match cli.command {
        Some(Commands::Call {
            destination,
            duration_secs,
            ..
        }) => {
            assert_eq!(destination, "+919000000000");
            assert_eq!(duration_secs, Some(45));
        }
        other => panic!("expected Commands::Call, got {other:?}"),
    }
}

#[test]
fn call_without_a_destination_is_a_clear_error_not_a_panic() {
    let result = Cli::try_parse_from(["siptest", "call"]);
    let err = result.expect_err("missing --destination should be a clap error");
    assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn status_subcommand_parses() {
    let cli = Cli::try_parse_from(["siptest", "status"]).unwrap();
    assert!(matches!(cli.command, Some(Commands::Status)));
}

#[test]
fn verbose_and_config_flags_are_read_regardless_of_subcommand() {
    let cli =
        Cli::try_parse_from(["siptest", "--config", "/etc/siptest.toml", "-v", "status"]).unwrap();
    assert_eq!(cli.config.to_str().unwrap(), "/etc/siptest.toml");
    assert!(cli.verbose);
}
