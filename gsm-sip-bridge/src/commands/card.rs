//! `gsm-sip-bridge card ...` — talks to the running daemon over its
//! Unix control socket.

use crate::cli::Cli;
use crate::config::load_config;
use crate::control::client;
use crate::control::protocol::{ControlCmd, ControlResp};
use std::process::ExitCode;

pub(crate) fn handle_card_command(args: &crate::cli::CardArgs, cli: &Cli) -> ExitCode {
    let socket_path = match cli.config.as_deref() {
        None => crate::config::DEFAULT_CONTROL_SOCKET.to_string(),
        Some(p) => match load_config(p) {
            Ok(c) => c.control.socket_path,
            Err(_) => crate::config::DEFAULT_CONTROL_SOCKET.to_string(),
        },
    };

    let cmd = match build_control_cmd(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    match client::send_cmd(&socket_path, &cmd) {
        Ok(resp) => print_resp(resp),
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) fn build_control_cmd(args: &crate::cli::CardArgs) -> Result<ControlCmd, String> {
    use crate::cli::CardSubcommand;
    match &args.subcommand {
        CardSubcommand::Restart { slot, mode } => Ok(ControlCmd::CardRestart {
            slot: *slot,
            mode: mode.clone(),
        }),
        CardSubcommand::SetMode { slot, mode } => Ok(ControlCmd::SetMode {
            slot: *slot,
            mode: mode.clone(),
        }),
        CardSubcommand::GetMode { slot } => Ok(ControlCmd::GetMode { slot: *slot }),
        CardSubcommand::List => Ok(ControlCmd::ListSlots),
    }
}

fn print_resp(resp: ControlResp) -> ExitCode {
    match resp {
        ControlResp::Ok => {
            println!("ok");
            ExitCode::SUCCESS
        }
        ControlResp::OkMode { mode } => {
            println!("mode: {mode}");
            ExitCode::SUCCESS
        }
        ControlResp::OkSlots { slots } => {
            if slots.is_empty() {
                println!("no slots registered");
            } else {
                println!("{:<6} {:<14} {:<20} network", "slot", "state", "phone");
                println!("{}", "-".repeat(60));
                for s in slots {
                    println!(
                        "{:<6} {:<14} {:<20} {}",
                        s.slot, s.state, s.phone, s.network
                    );
                }
            }
            ExitCode::SUCCESS
        }
        ControlResp::Err { error } => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{CardArgs, CardSubcommand};

    #[test]
    fn restart_defaults_to_full_mode() {
        let args = CardArgs {
            subcommand: CardSubcommand::Restart {
                slot: 3,
                mode: "full".to_string(),
            },
        };
        let cmd = build_control_cmd(&args).unwrap();
        assert!(matches!(
            cmd,
            ControlCmd::CardRestart { slot: 3, ref mode } if mode == "full"
        ));
    }

    #[test]
    fn restart_passes_an_explicit_radio_mode_through() {
        let args = CardArgs {
            subcommand: CardSubcommand::Restart {
                slot: 1,
                mode: "radio".to_string(),
            },
        };
        let cmd = build_control_cmd(&args).unwrap();
        assert!(matches!(
            cmd,
            ControlCmd::CardRestart { slot: 1, ref mode } if mode == "radio"
        ));
    }
}
