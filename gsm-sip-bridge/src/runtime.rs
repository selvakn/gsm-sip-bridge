use crate::error::{BridgeError, BridgeResult};
use tokio::runtime::Runtime;
use tokio::signal;
use tokio::sync::broadcast;

const SHUTDOWN_GRACE_PERIOD_SECS: u64 = 10;

pub fn build_runtime() -> BridgeResult<Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| BridgeError::Config(format!("failed to build tokio runtime: {e}")))
}

pub fn shutdown_channel() -> (broadcast::Sender<()>, broadcast::Receiver<()>) {
    broadcast::channel(1)
}

pub async fn wait_for_shutdown(shutdown_tx: broadcast::Sender<()>) {
    let ctrl_c = signal::ctrl_c();
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
        .expect("failed to register SIGTERM handler");

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("received SIGINT, initiating graceful shutdown");
        }
        _ = sigterm.recv() => {
            tracing::info!("received SIGTERM, initiating graceful shutdown");
        }
    }

    let _ = shutdown_tx.send(());

    tracing::info!(
        grace_period_secs = SHUTDOWN_GRACE_PERIOD_SECS,
        "waiting for in-flight work to complete"
    );
    tokio::time::sleep(std::time::Duration::from_secs(SHUTDOWN_GRACE_PERIOD_SECS)).await;
}
