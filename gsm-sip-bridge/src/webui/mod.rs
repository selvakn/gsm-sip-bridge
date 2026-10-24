//! Read-only management web UI (specs/122-management-webui).
//!
//! A second, authenticated HTTP listener (off unless `[webui].enabled`) that
//! serves a small embedded single-page app and a JSON API. Nothing here can
//! change state: the only non-GET routes are login and logout.

pub mod api;
pub mod auth;
pub mod config_view;
pub mod lines;
pub mod queries;
pub mod signal;

use crate::config::secret::Secret;
use crate::config::LoadedConfig;
use crate::modules::ControlCmdSender;
use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::Arc;

const INDEX_HTML: &str = include_str!("assets/index.html");
const APP_JS: &str = include_str!("assets/app.js");
const STYLE_CSS: &str = include_str!("assets/style.css");

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; frame-ancestors 'none'";

/// What every handler shares. Cheap to clone: it is one `Arc`.
#[derive(Clone)]
pub struct WebuiState(Arc<Shared>);

pub struct Shared {
    pub(crate) password: Secret<String>,
    pub(crate) sessions: auth::Sessions,
    pub(crate) limiter: auth::RateLimiter,
    /// The config as loaded at startup, with its pre-`env:` document.
    pub loaded: LoadedConfig,
    /// Reaches the circuit-switched card pool, when it runs.
    pub cs: Option<ControlCmdSender>,
    /// Read-only access to the message store, opened on first use.
    pub(crate) store: Arc<queries::StoreReader>,
}

impl Deref for WebuiState {
    type Target = Shared;
    fn deref(&self) -> &Shared {
        &self.0
    }
}

impl WebuiState {
    pub fn new(loaded: LoadedConfig, cs: Option<ControlCmdSender>) -> Self {
        // Touch each outcome so the series read 0 rather than being absent
        // until the first attempt.
        for outcome in ["success", "invalid", "locked_out"] {
            crate::metrics::WEBUI_LOGINS_TOTAL.with_label_values(&[outcome]);
        }
        Self(Arc::new(Shared {
            password: loaded.config.webui.password.clone(),
            sessions: auth::Sessions::default(),
            limiter: auth::RateLimiter::default(),
            store: Arc::new(queries::StoreReader::new(loaded.config.sms.db_path.clone())),
            loaded,
            cs,
        }))
    }
}

/// `ETag` for the embedded assets: they only change with the build.
fn etag() -> String {
    format!(
        "\"{}-{}\"",
        env!("CARGO_PKG_VERSION"),
        option_env!("GIT_SHA").unwrap_or("dev")
    )
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(v) = HeaderValue::from_str(&etag()) {
        headers.insert(header::ETAG, v);
    }
    response
}

async fn index() -> Response {
    asset("text/html; charset=utf-8", INDEX_HTML)
}

async fn app_js() -> Response {
    asset("text/javascript; charset=utf-8", APP_JS)
}

async fn style_css() -> Response {
    asset("text/css; charset=utf-8", STYLE_CSS)
}

/// Adds the headers every response carries (contracts/http-api.md).
async fn security_headers(request: Request, next: Next) -> Response {
    let is_api = request.uri().path().starts_with("/api/");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    if is_api {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

/// The UI's routes. Serve it with connect info
/// (`into_make_service_with_connect_info::<SocketAddr>()`) so login can see
/// the client address; [`serve_on`] does.
pub fn router(state: WebuiState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/login", get(index))
        .route("/assets/app.js", get(app_js))
        .route("/assets/style.css", get(style_css))
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        .route("/api/overview", get(api::overview))
        .route("/api/sms", get(api::sms))
        .route("/api/events", get(api::events))
        .route("/api/config", get(api::config))
        .route("/api/calls", get(api::calls))
        .route("/api/calls/summary", get(api::calls_summary))
        .route("/api/health", get(api::health))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// Only reached for a path no route matched. A matched path with the wrong
/// method never gets here: the route itself answers `405`.
async fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// Serves `router(state)` on an already-bound listener until it fails.
pub async fn serve_on(listener: tokio::net::TcpListener, state: WebuiState) -> std::io::Result<()> {
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
}

/// Binds `[webui].listen_addr:port` and serves. Returns the bind or serve
/// error; the caller logs it and carries on, because a UI that cannot start
/// must not take the bridge down (FR-004).
pub async fn serve(state: WebuiState) -> std::io::Result<()> {
    let cfg = &state.loaded.config.webui;
    let addr: SocketAddr = format!("{}:{}", bracket(&cfg.listen_addr), cfg.port)
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{e}")))?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "web UI listening");
    serve_on(listener, state).await
}

/// IPv6 literals need brackets in a `SocketAddr` string.
fn bracket(ip: &str) -> String {
    if ip.contains(':') {
        format!("[{ip}]")
    } else {
        ip.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv6_listen_addresses_are_bracketed() {
        assert_eq!(bracket("0.0.0.0"), "0.0.0.0");
        assert_eq!(bracket("::1"), "[::1]");
        let addr: SocketAddr = format!("{}:{}", bracket("::"), 8080).parse().unwrap();
        assert_eq!(addr.port(), 8080);
    }
}
