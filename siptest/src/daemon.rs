//! Composition root: builds the shared state, keeps registration alive in a
//! background thread, and serves the control API on its own tokio runtime
//! (`Builder::new_multi_thread().enable_all()` + `block_on`, never
//! `#[tokio::main]` — matching `gsm_sip_bridge::runtime`'s convention).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::api::state::{Counters, SharedState};
use crate::config::Config;
use crate::error::SipTestResult;
use crate::sip::registration::{
    self, RegistrationConfig, RegistrationCredentials, RegistrationStatus,
};
use crate::sip::socket::SipSocket;

pub fn run(config_path: &std::path::Path) -> std::process::ExitCode {
    let config = match crate::config::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "failed to load config");
            return std::process::ExitCode::FAILURE;
        }
    };

    match run_with_config(config) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "siptest daemon failed");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run_with_config(config: Config) -> SipTestResult<()> {
    let registrar_addr = config.sip.registrar_addr()?;
    let local_ip = config.sip.local_ip.as_ref().and_then(|s| s.parse().ok());
    let sip_socket = Arc::new(SipSocket::bind(
        local_ip,
        config.sip.local_port,
        registrar_addr,
    )?);
    tracing::info!(local_addr = %sip_socket.local_addr(), "SIP socket bound");

    let reg_config = RegistrationConfig {
        registrar_addr,
        registrar_host: config.sip.realm.clone(),
        aor_user: config.sip.username.clone(),
        realm: config.sip.realm.clone(),
        password: config.sip.password.clone(),
        expires: config.sip.register_expires_secs,
    };

    let state = Arc::new(SharedState {
        registration: Mutex::new(RegistrationStatus::default()),
        calls: Mutex::new(crate::api::state::CallRegistry::new(
            config.retention.max_calls_retained,
        )),
        attempt_history: Mutex::new(crate::safety::CallAttemptHistory::new()),
        counters: Mutex::new(Counters::default()),
        local_sip_addr: sip_socket.local_addr(),
        bridge_registrar: registrar_addr,
        last_outbound_observed: Mutex::new(None),
        events: crate::api::events::EventBus::default(),
        safety: config.safety.clone(),
        config: config.clone(),
        next_call_seq: Mutex::new(0),
        sip_socket: sip_socket.clone(),
        registration_creds: Mutex::new(RegistrationCredentials {
            cseq: 0,
            call_id: crate::sip::message::new_tag(),
            from_tag: crate::sip::message::new_tag(),
            cached_nonce: None,
            nc: 0,
        }),
    });

    let stop = Arc::new(AtomicBool::new(false));

    let reg_thread = {
        let state = state.clone();
        let stop = stop.clone();
        std::thread::spawn(move || registration_loop(state, reg_config, stop))
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let bind: std::net::SocketAddr = config
        .api
        .bind
        .parse()
        .map_err(|e| crate::error::SipTestError::Config(format!("invalid [api].bind: {e}")))?;

    rt.block_on(async {
        let app = crate::api::router(state.clone());
        tracing::info!(%bind, "control API listening");
        let listener = tokio::net::TcpListener::bind(bind).await?;
        let shutdown = shutdown_signal();
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown)
            .await
    })?;

    stop.store(true, Ordering::Relaxed);
    let _ = reg_thread.join();
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn registration_loop(state: Arc<SharedState>, cfg: RegistrationConfig, stop: Arc<AtomicBool>) {
    let mut creds = RegistrationCredentials {
        cseq: 0,
        call_id: crate::sip::message::new_tag(),
        from_tag: crate::sip::message::new_tag(),
        cached_nonce: None,
        nc: 0,
    };

    while !stop.load(Ordering::Relaxed) {
        match registration::register(&state.sip_socket, &cfg, &mut creds) {
            Ok(status) => {
                let registered = status.state == registration::RegState::Registered;
                let expires = status.granted_expires;
                {
                    let mut reg = state.registration.lock().unwrap_or_else(|e| e.into_inner());
                    *reg = status;
                }
                state
                    .counters
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .registrations += 1;
                state.events.publish(
                    "registration_state",
                    serde_json::json!({"registered": registered, "granted_expires": expires}),
                );
                if registered {
                    let refresh_in = expires.map(|e| (e / 2).max(30)).unwrap_or(60);
                    sleep_unless_stopped(&stop, Duration::from_secs(refresh_in as u64));
                } else {
                    let failures = state
                        .registration
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .consecutive_failures;
                    let backoff = [2u64, 4, 8, 16, 30]
                        .get(failures.min(4) as usize)
                        .copied()
                        .unwrap_or(30);
                    sleep_unless_stopped(&stop, Duration::from_secs(backoff));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "registration attempt failed");
                state
                    .counters
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .errors += 1;
                sleep_unless_stopped(&stop, Duration::from_secs(5));
            }
        }
    }

    registration::deregister(&state.sip_socket, &cfg, &mut creds);
}

fn sleep_unless_stopped(stop: &AtomicBool, d: Duration) {
    let step = Duration::from_millis(200);
    let mut waited = Duration::ZERO;
    while waited < d {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(step.min(d - waited));
        waited += step;
    }
}
