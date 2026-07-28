//! The VoWiFi subcommands: the two long-running bridge agents, their status
//! query, and the `modem-ims` reconcile step that must run before either.

use crate::cli::Cli;
use crate::config::load_config;
use std::process::ExitCode;

/// Shared by the three `vowifi-*` subcommands: `--config` is mandatory for
/// these (unlike the daemon path, which tolerates a missing path via
/// `cli.config.as_deref().unwrap_or(...)` for its own defaulting), and
/// `[vowifi].enabled` must be `true` — this is the guard that stops an
/// operator who hasn't provisioned VoWiFi from accidentally starting one of
/// these agents (see `config::VowifiConfig::enabled` docs).
fn load_vowifi_config(cli: &Cli) -> Result<crate::config::AppConfig, ExitCode> {
    let Some(path) = cli.config.as_deref() else {
        eprintln!("error: --config is required for vowifi-* subcommands");
        return Err(ExitCode::FAILURE);
    };
    let config = load_config(path).map_err(|e| {
        eprintln!("error: {e}");
        ExitCode::FAILURE
    })?;
    if !config.vowifi.enabled {
        eprintln!("error: [vowifi].enabled is false in the config file");
        return Err(ExitCode::FAILURE);
    }
    Ok(config)
}

/// Loads `--line N`'s fully-derived `VowifiConfig` from the `discover`
/// subcommand's line-resolution file — see
/// `specs/013-multi-card-vowifi/contracts/agent-topology-contract.md`.
/// `--line` is required: every line, including a single-SIM setup, is
/// resolved by `discover` first (`docker/entrypoint.sh` always runs it
/// before starting this agent). Deliberately does NOT re-run discovery
/// itself: doing so would re-probe modems a sibling
/// `vowifi-usim-bridge`/other line's agent may already have open
/// (research.md item 3).
pub(crate) fn handle_vowifi_ims_agent_command(cli: &Cli, line: Option<u32>) -> ExitCode {
    let config = match load_vowifi_config(cli) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let Some(index) = line else {
        eprintln!(
            "error: vowifi-ims-agent requires --line N (run `gsm-sip-bridge discover` first)"
        );
        return ExitCode::FAILURE;
    };
    let (card_id, line_config) = match load_line_config(index) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    crate::ims::agent::run(&card_id, &line_config, &config)
}

/// Reads the `discover` subcommand's line-resolution file and returns line
/// `index`'s card id and fully-derived `VowifiConfig`.
fn load_line_config(index: u32) -> Result<(String, crate::config::VowifiConfig), String> {
    let path = lines_file_path();
    let resolution = crate::vowifi::discovery::read_line_resolution(&path)?;
    resolution
        .line(index)
        .map(|l| (l.card_id.clone(), l.config.clone()))
        .ok_or_else(|| {
            format!(
                "line {index} not found in {} (run `gsm-sip-bridge discover` first; \
                 does that many usable VoWiFi lines actually exist?)",
                path.display()
            )
        })
}

pub(crate) fn lines_file_path() -> std::path::PathBuf {
    crate::modules::discovery::lines_file_path()
}

pub(crate) fn handle_vowifi_sip_agent_command(cli: &Cli) -> ExitCode {
    let config = match load_vowifi_config(cli) {
        Ok(c) => c,
        Err(code) => return code,
    };
    crate::vowifi::run(&config)
}

pub(crate) fn handle_vowifi_status_command(cli: &Cli) -> ExitCode {
    let config = match load_vowifi_config(cli) {
        Ok(c) => c,
        Err(code) => return code,
    };
    crate::vowifi::print_status(&config.vowifi)
}

/// Deliberately silent (no stdout/stderr on the success path) — callers
/// (`docker/entrypoint.sh`) only care about the exit code, e.g.
/// `if gsm-sip-bridge --config "$CONFIG" config vowifi-enabled; then ...`.
/// Unlike `load_vowifi_config`, does NOT require `[vowifi].enabled = true`
/// — that's exactly the thing being checked, not a precondition.
/// Loads the full config (not `load_vowifi_config`, which insists VoWiFi is
/// enabled): the whole point here is to act on `[vowifi].enabled` in *both*
/// directions — disable the modem's IMS when the bridge is on, re-enable it
/// when the bridge is off and VoLTE should work again.
pub(crate) fn handle_modem_ims_command(args: &crate::cli::ModemImsArgs, cli: &Cli) -> ExitCode {
    let Some(path) = cli.config.as_deref() else {
        eprintln!("modem-ims: --config is required");
        return ExitCode::FAILURE;
    };
    let config = match load_config(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("modem-ims: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Either host-driven path wants the modem's own IMS stack off — never
    // both at once (`volte::guard` refuses that), but either alone still
    // needs it (specs/020-volte-line-netns: this used to check
    // `config.vowifi.enabled` alone, which left a VoLTE-only deployment's
    // modem fighting our registration with its own internal one).
    let host_ims_wanted = config.vowifi.enabled || config.volte.enabled;
    crate::vowifi::ims_mode::run(&args.modem, host_ims_wanted)
}
