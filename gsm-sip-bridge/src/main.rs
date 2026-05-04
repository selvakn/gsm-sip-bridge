use gsm_sip_bridge::cli::Cli;
use gsm_sip_bridge::config::load_config;
use gsm_sip_bridge::metrics;
use gsm_sip_bridge::observability::{logging, modemmanager};
use gsm_sip_bridge::runtime;
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

    let (shutdown_tx, _shutdown_rx) = runtime::shutdown_channel();

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

        if let (Some(serial), Some(audio)) = (&cli.serial, &cli.audio) {
            tracing::info!(
                serial = %serial.display(),
                audio = %audio,
                "single-card override mode"
            );
        }

        tracing::info!("ready, waiting for modules (not yet implemented)");

        runtime::wait_for_shutdown(shutdown_tx).await;

        metrics_handle.abort();
    });

    store.shutdown();
    tracing::info!("shutdown complete");
    ExitCode::SUCCESS
}
