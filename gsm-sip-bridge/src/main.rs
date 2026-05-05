use gsm_sip_bridge::cli::Cli;
use gsm_sip_bridge::config::load_config;
use gsm_sip_bridge::metrics;
use gsm_sip_bridge::modules::CardPool;
use gsm_sip_bridge::observability::{logging, modemmanager};
use gsm_sip_bridge::runtime;
use gsm_sip_bridge::sip::SipBridge;
use gsm_sip_bridge::sms::SmsHandler;
use gsm_sip_bridge::store::StoreHandle;
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = Cli::parse_args();

    logging::init(cli.verbose);

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

        let sip_bridge = SipBridge::new(&config);
        let sms_handler = SmsHandler::new(&config.sms, store.sender());
        let card_pool = CardPool::new(config, store.sender(), sip_bridge, sms_handler);

        let pool_handle = tokio::spawn(async move {
            card_pool.run(single_card, shutdown_rx).await;
        });

        runtime::wait_for_shutdown(shutdown_tx).await;

        pool_handle.abort();
        metrics_handle.abort();
    });

    store.shutdown();
    tracing::info!("shutdown complete");
    ExitCode::SUCCESS
}
