//! The no-subcommand default: the long-running circuit-switched GSM->SIP
//! daemon (`CardPool` + SIP bridge + SMS handler + metrics/control servers).

use crate::alerts::discord::DiscordClient;
use crate::cli::Cli;
use crate::config::load_config;
use crate::config::secret::Secret;
use crate::control::server::start_control_server;
use crate::metrics;
use crate::modules::{CardPool, ControlCmdSender};
use crate::observability::modemmanager;
use crate::runtime;
use crate::sip::SipBridge;
use crate::sms::SmsHandler;
use crate::store::StoreHandle;
use std::process::ExitCode;
use tokio::sync::{mpsc, watch};

pub fn run(cli: &Cli) -> ExitCode {
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
        let agent_report_interval_seconds = config.metrics.agent_report_interval_seconds;
        let metrics_handle = tokio::spawn(async move {
            if let Err(e) =
                metrics::server::serve(metrics_port, agent_report_interval_seconds).await
            {
                tracing::error!(error = %e, "metrics server failed");
            }
        });

        // specs/022-discord-critical-alerts (Greptile P1/P2 fix): alert
        // evaluation lives in metrics::ingest, keyed to real AgentReport
        // arrival rather than an external Prometheus scrape — this must be
        // wired up before start_control_server below starts accepting the
        // reports that trigger it.
        match DiscordClient::new(Secret::new(String::new())) {
            Ok(client) => metrics::ingest::init_alerts(config.alerts.clone(), client),
            Err(e) => {
                tracing::error!(error = %e, "failed to create critical-alerts Discord client")
            }
        }

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
