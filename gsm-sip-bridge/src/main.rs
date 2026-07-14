use gsm_sip_bridge::cli::{Cli, Commands};
use gsm_sip_bridge::config::load_config;
use gsm_sip_bridge::control::client;
use gsm_sip_bridge::control::protocol::{ControlCmd, ControlResp};
use gsm_sip_bridge::control::server::start_control_server;
use gsm_sip_bridge::metrics;
use gsm_sip_bridge::modules::{CardPool, ControlCmdSender};
use gsm_sip_bridge::observability::{logging, modemmanager};
use gsm_sip_bridge::runtime;
use gsm_sip_bridge::sip::SipBridge;
use gsm_sip_bridge::sms::SmsHandler;
use gsm_sip_bridge::store::StoreHandle;
use std::process::ExitCode;
use tokio::sync::{mpsc, watch};

fn main() -> ExitCode {
    let cli = Cli::parse_args();

    // Read [logging].level ahead of the full config load below (which may
    // legitimately fail, e.g. an unset secret env var) so logging is set up
    // before anything else runs.
    let log_level = cli
        .config
        .as_deref()
        .map(gsm_sip_bridge::config::read_log_level)
        .unwrap_or_else(|| "info".to_string());
    logging::init(&log_level, cli.verbose);

    // Handle card subcommands before daemon startup
    if let Some(Commands::Card(card_args)) = &cli.command {
        return handle_card_command(card_args, &cli);
    }

    if let Some(Commands::ImsRegister(args)) = &cli.command {
        return handle_ims_register_command(args);
    }

    if let Some(Commands::ImsCall(args)) = &cli.command {
        return handle_ims_call_command(args);
    }

    if let Some(Commands::VowifiImsAgent) = &cli.command {
        return handle_vowifi_ims_agent_command(&cli);
    }

    if let Some(Commands::VowifiSipAgent) = &cli.command {
        return handle_vowifi_sip_agent_command(&cli);
    }

    if let Some(Commands::VowifiStatus) = &cli.command {
        return handle_vowifi_status_command(&cli);
    }

    if let Some(Commands::VowifiUsimBridge(args)) = &cli.command {
        return gsm_sip_bridge::vowifi::usim_bridge::run(
            &args.modem,
            &args.vpcd_host,
            args.vpcd_port,
        );
    }

    if let Some(Commands::VowifiImsi(args)) = &cli.command {
        return gsm_sip_bridge::vowifi::imsi::run(&args.modem);
    }

    if let Some(Commands::VowifiPlmn(args)) = &cli.command {
        return gsm_sip_bridge::vowifi::plmn::run(&args.modem);
    }

    if let Some(Commands::Config(args)) = &cli.command {
        return handle_config_command(args, &cli);
    }

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting gsm-sip-bridge"
    );

    let config = match load_config(cli.config.as_deref().unwrap_or(std::path::Path::new(""))) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "configuration failed");
            return ExitCode::from(1);
        }
    };

    modemmanager::check_modemmanager();
    metrics::register_build_info();
    metrics::server::record_start_time();

    let rt = match runtime::build_runtime() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "runtime initialization failed");
            return ExitCode::from(1);
        }
    };

    let store = match StoreHandle::open(std::path::Path::new(&config.sms.db_path)) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "store initialization failed");
            return ExitCode::from(66);
        }
    };

    let (shutdown_tx, shutdown_rx) = runtime::shutdown_channel();
    let (control_tx, control_rx): (ControlCmdSender, _) = mpsc::channel(8);
    let socket_path = config.control.socket_path.clone();

    rt.block_on(async {
        let metrics_port = config.metrics.port;
        let metrics_handle = tokio::spawn(async move {
            if let Err(e) = metrics::server::serve(metrics_port).await {
                tracing::error!(error = %e, "metrics server failed");
            }
        });

        tracing::info!(
            sip_server = %config.sip.server,
            sip_port = config.sip.port,
            modules_max = config.modules.max_concurrent,
            metrics_port = config.metrics.port,
            control_socket = %socket_path,
            "configuration loaded"
        );

        let single_card = match (&cli.serial, &cli.audio) {
            (Some(serial), Some(audio)) => {
                tracing::info!(
                    serial = %serial.display(),
                    audio = %audio,
                    "single-card override mode"
                );
                Some((serial.clone(), audio.clone()))
            }
            _ => None,
        };

        let (shutdown_watch_tx, shutdown_watch_rx) = watch::channel(false);

        let ctrl_server = start_control_server(&socket_path, control_tx, shutdown_watch_rx).await;

        let sip_bridge = SipBridge::new(&config);
        let sms_handler = SmsHandler::new(&config.sms, store.sender());
        let card_pool = CardPool::new(config, store, sip_bridge, sms_handler);

        let pool_handle = tokio::spawn(async move {
            card_pool.run(single_card, shutdown_rx, control_rx).await;
        });

        runtime::wait_for_shutdown(shutdown_tx).await;

        let _ = shutdown_watch_tx.send(true);
        ctrl_server.abort();
        pool_handle.abort();
        metrics_handle.abort();
    });

    tracing::info!("shutdown complete");
    ExitCode::SUCCESS
}

fn handle_card_command(args: &gsm_sip_bridge::cli::CardArgs, cli: &Cli) -> ExitCode {
    let socket_path = match cli.config.as_deref() {
        None => gsm_sip_bridge::config::DEFAULT_CONTROL_SOCKET.to_string(),
        Some(p) => match load_config(p) {
            Ok(c) => c.control.socket_path,
            Err(_) => gsm_sip_bridge::config::DEFAULT_CONTROL_SOCKET.to_string(),
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

fn build_ims_register_config(
    args: &gsm_sip_bridge::cli::ImsRegisterArgs,
) -> gsm_sip_bridge::ims::ImsRegisterConfig {
    gsm_sip_bridge::ims::ImsRegisterConfig {
        modem_port: args.modem.clone(),
        pcscf_addr: args.pcscf,
        pcscf_port: args.pcscf_port,
        mcc: args.mcc.clone(),
        mnc: args.mnc.clone(),
        imsi: args.imsi.clone(),
        imei: args.imei.clone(),
        use_tcp: args.tcp,
        sec_agree: args.sec_agree,
        msisdn: args.msisdn.clone(),
    }
}

fn handle_ims_register_command(args: &gsm_sip_bridge::cli::ImsRegisterArgs) -> ExitCode {
    use gsm_sip_bridge::ims::{run_register, RegisterOutcome};

    let cfg = build_ims_register_config(args);

    match run_register(&cfg) {
        Ok(RegisterOutcome::Success { status, headers }) => {
            println!("REGISTER succeeded: {status} OK");
            for (k, v) in headers {
                println!("  {k}: {v}");
            }
            ExitCode::SUCCESS
        }
        Ok(RegisterOutcome::Rejected { status, reason }) => {
            eprintln!("REGISTER rejected: {status} {reason}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn handle_ims_call_command(args: &gsm_sip_bridge::cli::ImsCallArgs) -> ExitCode {
    use gsm_sip_bridge::ims::call::{run_call, CallConfig, CallOutcome};
    use std::time::Duration;

    let cfg = CallConfig {
        register: build_ims_register_config(&args.register),
        callee: args.to.clone(),
        record_path: args.record.clone(),
        record_sent_path: args.record_sent.clone(),
        ring_timeout: Duration::from_secs(args.ring_timeout_secs),
        call_duration: Duration::from_secs(args.call_duration_secs),
    };

    match run_call(&cfg) {
        Ok(CallOutcome::Answered {
            recorded_path,
            recorded_samples,
            sent_path,
            sent_samples,
        }) => {
            println!(
                "call answered — recorded {recorded_samples} received samples to {}",
                recorded_path.display()
            );
            if let Some(sent_path) = sent_path {
                println!(
                    "  and {sent_samples} sent samples to {}",
                    sent_path.display()
                );
            }
            ExitCode::SUCCESS
        }
        Ok(CallOutcome::NotAnswered { status, reason }) => {
            eprintln!("call not answered: {status} {reason}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Shared by the three `vowifi-*` subcommands: `--config` is mandatory for
/// these (unlike the daemon path, which tolerates a missing path via
/// `cli.config.as_deref().unwrap_or(...)` for its own defaulting), and
/// `[vowifi].enabled` must be `true` — this is the guard that stops an
/// operator who hasn't provisioned VoWiFi from accidentally starting one of
/// these agents (see `config::VowifiConfig::enabled` docs).
fn load_vowifi_config(cli: &Cli) -> Result<gsm_sip_bridge::config::AppConfig, ExitCode> {
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

fn handle_vowifi_ims_agent_command(cli: &Cli) -> ExitCode {
    let config = match load_vowifi_config(cli) {
        Ok(c) => c,
        Err(code) => return code,
    };
    gsm_sip_bridge::ims::agent::run(&config.vowifi)
}

fn handle_vowifi_sip_agent_command(cli: &Cli) -> ExitCode {
    let config = match load_vowifi_config(cli) {
        Ok(c) => c,
        Err(code) => return code,
    };
    gsm_sip_bridge::vowifi::run(&config)
}

fn handle_vowifi_status_command(cli: &Cli) -> ExitCode {
    let config = match load_vowifi_config(cli) {
        Ok(c) => c,
        Err(code) => return code,
    };
    gsm_sip_bridge::vowifi::print_status(&config.vowifi)
}

/// Deliberately silent (no stdout/stderr on the success path) — callers
/// (`docker/entrypoint.sh`) only care about the exit code, e.g.
/// `if gsm-sip-bridge --config "$CONFIG" config vowifi-enabled; then ...`.
/// Unlike `load_vowifi_config`, does NOT require `[vowifi].enabled = true`
/// — that's exactly the thing being checked, not a precondition.
fn handle_config_command(args: &gsm_sip_bridge::cli::ConfigArgs, cli: &Cli) -> ExitCode {
    use gsm_sip_bridge::cli::ConfigSubcommand;
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
        ConfigSubcommand::VowifiShellEnv => {
            let Some(path) = cli.config.as_deref() else {
                eprintln!("config vowifi-shell-env: --config is required");
                return ExitCode::FAILURE;
            };
            match load_config(path) {
                Ok(config) => {
                    print_vowifi_shell_env(&config);
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

/// Single-quotes `s` for safe use as a POSIX shell word, escaping any
/// embedded single quotes (`'` -> `'\''`).
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn print_vowifi_shell_env(config: &gsm_sip_bridge::config::AppConfig) {
    let v = &config.vowifi;
    let lines: Vec<(&str, String)> = vec![
        ("MCC", v.mcc.clone()),
        ("MNC", v.mnc.clone()),
        ("APN", v.apn.clone()),
        ("MODEM_PORT", v.modem_port.clone()),
        ("NETNS", v.netns.clone()),
        ("EPDG_FQDN", v.epdg_fqdn.clone()),
        ("EPDG_IP", v.epdg_ip.clone().unwrap_or_default()),
        ("SRC_ADDR", v.src_addr.clone().unwrap_or_default()),
        ("KEEPALIVE_INTERVAL", v.keepalive_interval_sec.to_string()),
        ("VETH_SIP", v.veth_sip_iface.clone()),
        ("VETH_IMS", v.veth_ims_iface.clone()),
        ("VETH_IMS_ADDR", format!("{}/30", v.veth_local_addr)),
        ("VETH_SIP_ADDR", format!("{}/30", v.veth_peer_addr)),
        ("TUNNEL_ENGINE", v.tunnel_engine.clone()),
        ("STRONGSWAN_TUN_IFACE", v.strongswan_tun_iface.clone()),
        ("STRONGSWAN_IF_ID", v.strongswan_if_id.to_string()),
        ("VPCD_HOST", v.vpcd_host.clone()),
        ("VPCD_PORT", v.vpcd_port.to_string()),
        ("IMSI", v.imsi_override.clone().unwrap_or_default()),
        ("METRICS_PORT", config.metrics.port.to_string()),
    ];
    for (key, value) in lines {
        println!("{key}={}", shell_quote(&value));
    }
}

fn build_control_cmd(args: &gsm_sip_bridge::cli::CardArgs) -> Result<ControlCmd, String> {
    use gsm_sip_bridge::cli::CardSubcommand;
    match &args.subcommand {
        CardSubcommand::Restart { slot } => Ok(ControlCmd::CardRestart { slot: *slot }),
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
