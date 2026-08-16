//! Composition root: builds the shared state, keeps registration alive in a
//! background thread, and serves the control API on its own tokio runtime
//! (`Builder::new_multi_thread().enable_all()` + `block_on`, never
//! `#[tokio::main]` — matching `gsm_sip_bridge::runtime`'s convention).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::api::state::{Counters, InboundMode, InboundPolicy, SharedState};
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
        registration_config: reg_config.clone(),
        inbound_policy: Mutex::new(InboundPolicy {
            mode: config.inbound.mode.parse().unwrap_or(InboundMode::Answer),
            answer_delay_ms: config.inbound.answer_delay_ms,
            reject_status: config.inbound.reject_status,
            duration_secs: config.inbound.duration_secs,
        }),
        manual_decisions: Mutex::new(std::collections::HashMap::new()),
    });

    let stop = Arc::new(AtomicBool::new(false));

    let reg_thread = {
        let state = state.clone();
        let stop = stop.clone();
        std::thread::spawn(move || registration_loop(state, reg_config, stop))
    };

    let inbound_thread = {
        let state = state.clone();
        let stop = stop.clone();
        std::thread::spawn(move || inbound_listener_loop(state, stop))
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
    let _ = inbound_thread.join();
    Ok(())
}

/// Owns nothing but the read side of `recv_request` (via `SipSocket`, which
/// already demultiplexes responses away from it — `sip/socket.rs`'s module
/// doc). Answers `OPTIONS` immediately, busies out a second concurrent
/// INVITE, refuses anything else with `405`, and hands a fresh INVITE to
/// `call::execute_inbound_call`, which runs to completion before this loop
/// reads again — correct for `max_concurrent = 1`.
/// `pub` so integration tests can drive it directly against a hand-built
/// `SharedState`, without needing the full HTTP daemon up.
pub fn inbound_listener_loop(state: Arc<SharedState>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let Ok(Some((req, peer))) = state.sip_socket.recv_request(Duration::from_millis(500))
        else {
            continue;
        };
        // Matched on IP only, never port — the bridge's telephony agent
        // sends INVITEs from a different port than its registrar
        // (`test_inbound_from_other_port.rs`), so port can't be part of the
        // check. Anything from elsewhere is dropped with no reply at all,
        // not just refused: this is the daemon's only defense against an
        // unauthenticated peer directing an answered call's RTP at an
        // arbitrary third-party destination via its own SDP offer
        // (`execute_inbound_call` trusts whatever `c=`/`m=` the offer
        // names), and replying to a spoofed source would make siptest a
        // UDP reflection amplifier for that same peer.
        if peer.ip() != state.bridge_registrar.ip() {
            tracing::warn!(%peer, "dropping SIP request from an unexpected source IP");
            continue;
        }
        match req.method.as_str() {
            "OPTIONS" => {
                let _ = state
                    .sip_socket
                    .send(peer, &crate::sip::inbound::build_options_ok(&req));
            }
            "INVITE" => {
                if state
                    .calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .active()
                    .is_some()
                {
                    let _ = state
                        .sip_socket
                        .send(peer, &crate::sip::inbound::build_busy(&req));
                    continue;
                }
                crate::call::execute_inbound_call(&state, req, peer);
            }
            "ACK" | "BYE" | "CANCEL" => {
                // Stray — the dialog these belong to is handled entirely
                // inside `execute_inbound_call`'s own read loop while it
                // runs; anything reaching here has no matching call.
            }
            _ => {
                let _ = state
                    .sip_socket
                    .send(peer, &crate::sip::inbound::build_405(&req));
            }
        }
    }
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
                    sleep_unless_stopped(
                        &stop,
                        Duration::from_secs(refresh_interval_secs(expires) as u64),
                    );
                } else {
                    let failures = state
                        .registration
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .consecutive_failures;
                    sleep_unless_stopped(&stop, Duration::from_secs(backoff_secs(failures)));
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

/// Half the granted lease, floored at 30s so a short-lived grant doesn't
/// drive a refresh storm. Falls back to 60s when the registrar granted no
/// `Expires` at all.
fn refresh_interval_secs(granted_expires: Option<u32>) -> u32 {
    granted_expires.map(|e| (e / 2).max(30)).unwrap_or(60)
}

/// The documented backoff ladder for consecutive registration failures —
/// 2/4/8/16/30s, holding at 30s past the fourth failure.
fn backoff_secs(consecutive_failures: u32) -> u64 {
    [2u64, 4, 8, 16, 30]
        .get(consecutive_failures.min(4) as usize)
        .copied()
        .unwrap_or(30)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_interval_is_half_the_grant_floored_at_thirty_seconds() {
        assert_eq!(refresh_interval_secs(Some(300)), 150);
        assert_eq!(
            refresh_interval_secs(Some(40)),
            30,
            "20 would be below the floor"
        );
        assert_eq!(
            refresh_interval_secs(Some(20)),
            30,
            "10 would be below the floor"
        );
        assert_eq!(
            refresh_interval_secs(None),
            60,
            "no granted Expires falls back to a fixed 60s"
        );
    }

    #[test]
    fn backoff_follows_the_documented_ladder_and_holds_at_thirty() {
        assert_eq!(backoff_secs(0), 2);
        assert_eq!(backoff_secs(1), 4);
        assert_eq!(backoff_secs(2), 8);
        assert_eq!(backoff_secs(3), 16);
        assert_eq!(backoff_secs(4), 30);
        assert_eq!(backoff_secs(5), 30, "past the ladder's end, holds at 30s");
        assert_eq!(backoff_secs(100), 30);
    }
}
