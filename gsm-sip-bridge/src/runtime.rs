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

/// Blocks until SIGINT/SIGTERM, then returns — no broadcast, no grace-period
/// sleep. Split out of `wait_for_shutdown` for `supervise::orchestrate::run`
/// (specs/021-entrypoint-supervise-rust Phase 4): that caller has no
/// in-flight *async* work of its own to drain (its "children" are separate
/// OS processes, not tokio tasks on this runtime) — its job on receiving the
/// signal is to immediately run `shutdown::execute_shutdown_plan`, which has
/// its own bounded `WaitForExit` polling per step. Reusing the full
/// `wait_for_shutdown` there (an earlier version of this port did) meant the
/// entire 10s `SHUTDOWN_GRACE_PERIOD_SECS` sleep below elapsed *before*
/// execute_shutdown_plan ever ran a single step — racing Docker's own
/// default `stop_grace_period` (also 10s) to start the real teardown work
/// with no time budget left, risking a SIGKILL mid-teardown under any real
/// load. Found live: harmless when idle (nothing to wait for signals to
/// short-circuit), but a real risk under load. Caught only because a
/// separate fix (see `orchestrate.rs`'s `daemon_supervisor`/`sip_agent`/
/// `ims-agent` loops) made child signals actually reach their targets for
/// the first time, which is what made this timing matter at all.
pub async fn wait_for_signal() {
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
}

pub async fn wait_for_shutdown(shutdown_tx: broadcast::Sender<()>) {
    wait_for_signal().await;

    let _ = shutdown_tx.send(());

    tracing::info!(
        grace_period_secs = SHUTDOWN_GRACE_PERIOD_SECS,
        "waiting for in-flight work to complete"
    );
    tokio::time::sleep(std::time::Duration::from_secs(SHUTDOWN_GRACE_PERIOD_SECS)).await;
}
