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

    logging::init(cli.verbose);

    // Handle card subcommands before daemon startup
    if let Some(Commands::Card(card_args)) = &cli.command {
        return handle_card_command(card_args, &cli);
    }

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting gsm-sip-bridge"
    );

    let config = match load_config(&cli.config) {
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
    let socket_path = if cli.config.as_os_str().is_empty() {
        gsm_sip_bridge::config::DEFAULT_CONTROL_SOCKET.to_string()
    } else {
        match load_config(&cli.config) {
            Ok(c) => c.control.socket_path,
            Err(_) => gsm_sip_bridge::config::DEFAULT_CONTROL_SOCKET.to_string(),
        }
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
