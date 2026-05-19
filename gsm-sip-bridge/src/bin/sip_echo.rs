use gsm_sip_bridge::cli::Cli;
use gsm_sip_bridge::config::load_config;
use gsm_sip_bridge::observability::logging;
use gsm_sip_bridge::runtime;
use gsm_sip_bridge::sip::SipBridge;
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = Cli::parse_args();
    logging::init(cli.verbose);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting sip-echo (SIP audio echo, no GSM)"
    );

    let config = match load_config(cli.config.as_deref().unwrap_or(std::path::Path::new(""))) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "configuration failed");
            return ExitCode::from(1);
        }
    };

    tracing::info!(
        sip_server = %config.sip.server,
        sip_port = config.sip.port,
        sip_user = %config.sip.username,
        transport = ?config.sip.transport,
        "SIP configuration loaded"
    );

    let mut sip = SipBridge::new(&config);

    if let Err(e) = sip.register() {
        tracing::error!(error = %e, "SIP registration failed");
        return ExitCode::from(1);
    }

    tracing::info!(
        "SIP registered — waiting for calls. \
         Audio from incoming SIP calls is echoed back via the PJSIP conference bridge. \
         Press Ctrl+C to quit."
    );

    let rt = match runtime::build_runtime() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "runtime initialization failed");
            sip.unregister();
            return ExitCode::from(1);
        }
    };

    let (shutdown_tx, _) = runtime::shutdown_channel();
    rt.block_on(async {
        runtime::wait_for_shutdown(shutdown_tx).await;
    });

    tracing::info!("shutting down");
    sip.unregister();
    tracing::info!("sip-echo stopped");
    ExitCode::SUCCESS
}
