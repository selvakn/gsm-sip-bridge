//! One module per CLI subcommand family, plus the dispatcher [`run`] that
//! maps a parsed [`Commands`] onto them.
//!
//! These handlers used to live in `src/main.rs`. That made them unreachable
//! from `tests/` — a binary crate's items cannot be imported — so ~2100 lines
//! of real logic (line resolution, call reporting, and the `*-shell-env`
//! printers whose output other processes `eval`) had no tests at all. Moving
//! them into the library changes nothing about what they do; it only makes
//! them addressable. `main.rs` is now argument parsing, logging setup, and a
//! call to [`run`].

pub mod card;
pub mod config;
pub mod daemon;
pub mod discover;
pub mod healthcheck;
pub mod ims;
pub mod volte;
pub mod vowifi;

use crate::cli::{Cli, Commands};
use std::process::ExitCode;

/// Single-quotes `s` for safe use as a POSIX shell word, escaping any
/// embedded single quotes (`'` -> `'\''`).
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Runs the selected subcommand.
///
/// A `match` rather than the chain of `if let Some(Commands::X(..))` this
/// replaces: the compiler now checks exhaustiveness, so adding a variant to
/// [`Commands`] without wiring it up is a build error instead of a subcommand
/// that silently falls through and starts the daemon.
pub fn run(cli: &Cli, command: &Commands) -> ExitCode {
    match command {
        Commands::Card(args) => card::handle_card_command(args, cli),
        Commands::ImsRegister(args) => ims::handle_ims_register_command(args),
        Commands::ImsCall(args) => ims::handle_ims_call_command(args),
        Commands::VowifiImsAgent(args) => vowifi::handle_vowifi_ims_agent_command(cli, args.line),
        Commands::VowifiSipAgent => vowifi::handle_vowifi_sip_agent_command(cli),
        Commands::VowifiStatus => vowifi::handle_vowifi_status_command(cli),
        Commands::VowifiUsimBridge(args) => {
            crate::vowifi::usim_bridge::run(&args.modem, &args.vpcd_host, args.vpcd_port)
        }
        Commands::VowifiImsi(args) => crate::vowifi::imsi::run(&args.modem),
        Commands::VowifiPlmn(args) => crate::vowifi::plmn::run(&args.modem),
        Commands::ModemIms(args) => vowifi::handle_modem_ims_command(args, cli),
        Commands::VoltePdn(args) => volte::handle_volte_pdn_command(args, cli.config.as_deref()),
        Commands::VolteStatus(args) => volte::handle_volte_status_command(args),
        Commands::VolteDiscover(args) => volte::handle_volte_discover_command(args),
        Commands::VolteRegister(args) => {
            volte::handle_volte_register_command(args, cli.config.as_deref())
        }
        Commands::VolteCall(args) => volte::handle_volte_call_command(args),
        Commands::VolteListen(args) => volte::handle_volte_listen_command(args),
        Commands::VolteBridge(args) => volte::handle_volte_bridge_command(args, cli),
        Commands::VolteDiscoverLines(args) => volte::handle_volte_discover_lines_command(args, cli),
        Commands::VolteCarrierAgent(args) => volte::handle_volte_carrier_agent_command(args, cli),
        Commands::VolteCleanup(args) => volte::handle_volte_cleanup_command(args.line),
        Commands::Config(args) => config::handle_config_command(args, cli),
        Commands::Discover(args) => discover::handle_discover_command(args, cli),
        Commands::Render(args) => discover::handle_render_command(args),
        Commands::Supervise => {
            let Some(path) = cli.config.as_deref() else {
                eprintln!("supervise: --config is required");
                return ExitCode::FAILURE;
            };
            crate::supervise::orchestrate::run(path)
        }
        Commands::Healthcheck => healthcheck::run(cli),
        Commands::TcpProbe { host, port } => healthcheck::run_tcp_probe(host, *port),
    }
}
