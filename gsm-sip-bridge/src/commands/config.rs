//! `gsm-sip-bridge config ...` — resolved-configuration queries, including the
//! `*-shell-env` printers whose output other processes `eval`.

use super::shell_quote;
use crate::cli::Cli;
use crate::config::load_config;
use std::fmt::Write;
use std::process::ExitCode;

pub(crate) fn handle_config_command(args: &crate::cli::ConfigArgs, cli: &Cli) -> ExitCode {
    use crate::cli::ConfigSubcommand;
    match &args.subcommand {
        ConfigSubcommand::VowifiEnabled => {
            let Some(path) = cli.config.as_deref() else {
                return ExitCode::FAILURE;
            };
            match load_config(path) {
                Ok(config) if config.vowifi.enabled => ExitCode::SUCCESS,
                _ => ExitCode::FAILURE,
            }
        }
        ConfigSubcommand::VolteEnabled => {
            let Some(path) = cli.config.as_deref() else {
                return ExitCode::FAILURE;
            };
            match load_config(path) {
                Ok(config) if config.volte.enabled => ExitCode::SUCCESS,
                _ => ExitCode::FAILURE,
            }
        }
        ConfigSubcommand::VolteShellEnv => {
            let Some(path) = cli.config.as_deref() else {
                eprintln!("config volte-shell-env: --config is required");
                return ExitCode::FAILURE;
            };
            let config = match load_config(path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("config volte-shell-env: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // Global-only: per-line values (modem_port/iface/cid/apn/pcscf/
            // pcscf_port) live in `[[volte.line]]`, read directly by
            // `volte-bridge` from config.toml — there is nothing to derive
            // as a shell var here anymore.
            let v = &config.volte;
            let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
            println!("VOLTE_ENABLED={}", if v.enabled { 1 } else { 0 });
            println!("VOLTE_PCSCF_SOURCE_PATH={}", q(&v.pcscf_source_path));
            println!("VOLTE_STATUS_PATH={}", q(&v.status_path));
            println!("VOLTE_LOCK_PATH={}", q(&v.lock_path));
            println!(
                "VOLTE_BRIDGE_INBOUND={}",
                if v.bridge_inbound { 1 } else { 0 }
            );
            println!("VOLTE_MAX_LINES={}", v.max_lines);
            ExitCode::SUCCESS
        }
        ConfigSubcommand::VowifiShellEnv => {
            let Some(path) = cli.config.as_deref() else {
                eprintln!("config vowifi-shell-env: --config is required");
                return ExitCode::FAILURE;
            };
            match load_config(path) {
                Ok(config) => {
                    print!("{}", render_vowifi_shell_env(&config));
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("config vowifi-shell-env: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

pub fn render_vowifi_shell_env(config: &crate::config::AppConfig) -> String {
    let mut out = String::new();
    // Global-only: per-line values (mcc/mnc/modem_port/netns/veth
    // names+addrs/strongswan iface+if_id/vpcd_port) come from
    // `discover --shell-env` instead — see `render_discover_shell_env`.
    let v = &config.vowifi;
    let lines: Vec<(&str, String)> = vec![
        ("APN", v.apn.clone()),
        ("EPDG_FQDN", v.epdg_fqdn.clone()),
        ("EPDG_IP", v.epdg_ip.clone().unwrap_or_default()),
        ("SRC_ADDR", v.src_addr.clone().unwrap_or_default()),
        ("KEEPALIVE_INTERVAL", v.keepalive_interval_sec.to_string()),
        ("TUNNEL_ENGINE", v.tunnel_engine.clone()),
        ("VPCD_HOST", v.vpcd_host.clone()),
        ("VPCD_PORT", v.vpcd_port.to_string()),
        ("METRICS_PORT", config.metrics.port.to_string()),
    ];
    for (key, value) in lines {
        let _ = writeln!(&mut out, "{key}={}", shell_quote(&value));
    }
    out
}
