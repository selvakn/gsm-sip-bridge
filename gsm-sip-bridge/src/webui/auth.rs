//! Web UI authentication (specs/122-management-webui, research R9): one
//! shared password, in-memory sessions, and a per-IP lockout.
//!
//! Sessions deliberately do not survive a restart. SMS bodies carry one-time
//! codes, so a login that outlives the process would be a longer-lived secret
//! than anything the page shows.

use super::WebuiState;
use crate::metrics;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

pub const COOKIE_NAME: &str = "gsb_session";
pub const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(300);
const MAX_TRACKED_IPS: usize = 1024;

/// Session tokens, each valid until its expiry.
#[derive(Default)]
pub struct Sessions {
    tokens: Mutex<HashMap<String, Instant>>,
}

impl Sessions {
    /// Mints a token from 32 random bytes.
    pub fn create(&self, now: Instant) -> String {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let mut tokens = self.tokens.lock().unwrap();
        // Expired entries nobody looked up again would otherwise stay forever.
        tokens.retain(|_, expiry| *expiry > now);
        tokens.insert(token.clone(), now + SESSION_TTL);
        token
    }

    /// Whether `token` is a live session. An expired one is dropped on the
    /// way past.
    pub fn is_valid(&self, token: &str, now: Instant) -> bool {
        let mut tokens = self.tokens.lock().unwrap();
        match tokens.get(token) {
            Some(expiry) if *expiry > now => true,
            Some(_) => {
                tokens.remove(token);
                false
            }
            None => false,
        }
    }

    pub fn remove(&self, token: &str) {
        self.tokens.lock().unwrap().remove(token);
    }
}

#[derive(Debug, Clone, Copy)]
struct Attempts {
    failures: u32,
    locked_until: Option<Instant>,
    last_seen: Instant,
}

/// Per-IP failure counting. Five consecutive failures lock the address out
/// for five minutes; a success clears the count. The table is bounded, with
/// the least recently seen address evicted first, so a flood of source
/// addresses cannot grow it without limit.
#[derive(Default)]
pub struct RateLimiter {
    attempts: Mutex<HashMap<IpAddr, Attempts>>,
}

impl RateLimiter {
    /// `Err(seconds)` while `ip` is locked out.
    pub fn check(&self, ip: IpAddr, now: Instant) -> Result<(), u64> {
        let attempts = self.attempts.lock().unwrap();
        match attempts.get(&ip).and_then(|a| a.locked_until) {
            Some(until) if until > now => Err((until - now).as_secs().max(1)),
            _ => Ok(()),
        }
    }

    pub fn record_failure(&self, ip: IpAddr, now: Instant) {
        let mut attempts = self.attempts.lock().unwrap();
        if !attempts.contains_key(&ip) && attempts.len() >= MAX_TRACKED_IPS {
            if let Some(oldest) = attempts
                .iter()
                .min_by_key(|(_, a)| a.last_seen)
                .map(|(ip, _)| *ip)
            {
                attempts.remove(&oldest);
            }
        }
        let entry = attempts.entry(ip).or_insert(Attempts {
            failures: 0,
            locked_until: None,
            last_seen: now,
        });
        // A lockout that has run its course starts the count afresh.
        if entry.locked_until.is_some_and(|until| until <= now) {
            entry.failures = 0;
            entry.locked_until = None;
        }
        entry.failures += 1;
        entry.last_seen = now;
        if entry.failures >= MAX_FAILURES {
            entry.locked_until = Some(now + LOCKOUT);
        }
    }

    pub fn record_success(&self, ip: IpAddr) {
        self.attempts.lock().unwrap().remove(&ip);
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.attempts.lock().unwrap().len()
    }
}

/// Constant-time, so the time taken does not reveal how much of a guess was
/// right. Lengths are compared in the clear, which only discloses the length.
pub fn password_matches(expected: &str, given: &str) -> bool {
    expected.as_bytes().ct_eq(given.as_bytes()).into()
}

fn json_error(status: StatusCode, error: &str) -> Response {
    (status, Json(serde_json::json!({ "error": error }))).into_response()
}

fn session_cookie(token: &str) -> String {
    format!(
        "{COOKIE_NAME}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        SESSION_TTL.as_secs()
    )
}

/// The `gsb_session` value from a `Cookie` header, if any.
pub fn session_token(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE_NAME)
        .map(|(_, value)| value.to_string())
}

#[derive(Deserialize)]
pub struct LoginBody {
    password: String,
}

pub async fn login(
    State(state): State<WebuiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<LoginBody>,
) -> Response {
    let ip = peer.ip();
    let now = Instant::now();

    if let Err(retry_after) = state.limiter.check(ip, now) {
        metrics::WEBUI_LOGINS_TOTAL
            .with_label_values(&["locked_out"])
            .inc();
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({
                "error": "too_many_attempts",
                "retry_after_seconds": retry_after,
            })),
        )
            .into_response();
        if let Ok(v) = HeaderValue::from_str(&retry_after.to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, v);
        }
        return response;
    }

    if !password_matches(state.password.expose_secret(), &body.password) {
        state.limiter.record_failure(ip, now);
        metrics::WEBUI_LOGINS_TOTAL
            .with_label_values(&["invalid"])
            .inc();
        tracing::warn!(%ip, "web UI login refused: wrong password");
        return json_error(StatusCode::UNAUTHORIZED, "invalid_credentials");
    }

    state.limiter.record_success(ip);
    metrics::WEBUI_LOGINS_TOTAL
        .with_label_values(&["success"])
        .inc();
    let token = state.sessions.create(now);
    let mut response = StatusCode::NO_CONTENT.into_response();
    if let Ok(v) = HeaderValue::from_str(&session_cookie(&token)) {
        response.headers_mut().insert(header::SET_COOKIE, v);
    }
    response
}

pub async fn logout(State(state): State<WebuiState>, request: Request) -> Response {
    if let Some(token) = session_token(request.headers()) {
        state.sessions.remove(&token);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("gsb_session=; Max-Age=0; Path=/"),
    );
    response
}

/// Paths served without a session: the login page, the assets it needs, and
/// the login call itself.
fn is_public(path: &str, method: &axum::http::Method) -> bool {
    match path {
        "/login" | "/assets/app.js" | "/assets/style.css" => true,
        "/api/login" => method == axum::http::Method::POST,
        _ => false,
    }
}

/// Everything not public needs a live session. A browser navigation is sent
/// to the login page; an API call gets a bare 401 that carries no data
/// (SC-005).
pub async fn require_session(
    State(state): State<WebuiState>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if is_public(path, request.method()) {
        return next.run(request).await;
    }
    let authenticated = session_token(request.headers())
        .is_some_and(|token| state.sessions.is_valid(&token, Instant::now()));
    if authenticated {
        return next.run(request).await;
    }
    if path.starts_with("/api/") {
        json_error(StatusCode::UNAUTHORIZED, "unauthenticated")
    } else {
        Redirect::to("/login").into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    #[test]
    fn five_failures_lock_the_address_out() {
        let limiter = RateLimiter::default();
        let t0 = Instant::now();
        for _ in 0..4 {
            limiter.record_failure(ip(1), t0);
            assert!(limiter.check(ip(1), t0).is_ok());
        }
        limiter.record_failure(ip(1), t0);
        let retry = limiter.check(ip(1), t0).unwrap_err();
        assert!((299..=300).contains(&retry), "{retry}");
        // Another address is unaffected.
        assert!(limiter.check(ip(2), t0).is_ok());
    }

    #[test]
    fn the_lockout_expires_and_the_count_restarts() {
        let limiter = RateLimiter::default();
        let t0 = Instant::now();
        for _ in 0..5 {
            limiter.record_failure(ip(1), t0);
        }
        let later = t0 + LOCKOUT + Duration::from_secs(1);
        assert!(limiter.check(ip(1), later).is_ok());
        limiter.record_failure(ip(1), later);
        assert!(
            limiter.check(ip(1), later).is_ok(),
            "one failure after a served lockout must not lock again"
        );
    }

    #[test]
    fn a_success_clears_the_failure_count() {
        let limiter = RateLimiter::default();
        let t0 = Instant::now();
        for _ in 0..4 {
            limiter.record_failure(ip(1), t0);
        }
        limiter.record_success(ip(1));
        limiter.record_failure(ip(1), t0);
        assert!(limiter.check(ip(1), t0).is_ok());
    }

    #[test]
    fn the_table_is_bounded_and_evicts_the_oldest() {
        let limiter = RateLimiter::default();
        let t0 = Instant::now();
        for i in 0..(MAX_TRACKED_IPS as u32 + 10) {
            let addr = IpAddr::from([10, (i >> 8) as u8, i as u8, 1]);
            limiter.record_failure(addr, t0 + Duration::from_millis(u64::from(i)));
        }
        assert_eq!(limiter.tracked(), MAX_TRACKED_IPS);
    }

    #[test]
    fn sessions_expire_after_seven_days() {
        let sessions = Sessions::default();
        let t0 = Instant::now();
        let token = sessions.create(t0);
        assert!(sessions.is_valid(&token, t0 + Duration::from_secs(3600)));
        assert!(!sessions.is_valid(&token, t0 + SESSION_TTL + Duration::from_secs(1)));
        assert!(
            !sessions.is_valid(&token, t0),
            "an expired token is dropped, not merely ignored"
        );
    }

    #[test]
    fn tokens_are_unique_and_long() {
        let sessions = Sessions::default();
        let now = Instant::now();
        let a = sessions.create(now);
        let b = sessions.create(now);
        assert_ne!(a, b);
        assert_eq!(a.len(), 43, "32 bytes, base64url without padding");
        sessions.remove(&a);
        assert!(!sessions.is_valid(&a, now));
        assert!(sessions.is_valid(&b, now));
    }

    #[test]
    fn password_comparison() {
        assert!(password_matches("correct horse", "correct horse"));
        assert!(!password_matches("correct horse", "correct hors"));
        assert!(!password_matches("correct horse", ""));
    }

    #[test]
    fn the_session_cookie_is_found_among_others() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; gsb_session=tok123; b=2"),
        );
        assert_eq!(session_token(&headers).as_deref(), Some("tok123"));
        assert_eq!(session_token(&axum::http::HeaderMap::new()), None);
    }
}
