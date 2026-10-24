// specs/122-management-webui, User Story 6: the UI is closed to everyone who
// does not hold the password. Real router, real listener, real HTTP client.

mod common;

use common::{Fixture, PASSWORD};
use serde_json::json;

/// Every `/api/*` route in contracts/http-api.md.
const API_ROUTES: &[&str] = &[
    "/api/overview",
    "/api/health",
    "/api/calls/summary",
    "/api/calls",
    "/api/sms",
    "/api/events",
    "/api/config",
];

async fn login_with(f: &Fixture, password: &str) -> reqwest::Response {
    f.client
        .post(f.url("/api/login"))
        .json(&json!({ "password": password }))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn every_api_route_refuses_a_caller_without_a_cookie() {
    let f = Fixture::start("").await;
    for route in API_ROUTES {
        let response = f.client.get(f.url(route)).send().await.unwrap();
        assert_eq!(response.status(), 401, "{route}");
        assert_eq!(
            response.text().await.unwrap(),
            r#"{"error":"unauthenticated"}"#,
            "{route} must say nothing beyond the refusal"
        );
    }
}

#[tokio::test]
async fn a_page_request_without_a_cookie_is_sent_to_the_login_page() {
    let f = Fixture::start("").await;
    let response = f.client.get(f.url("/")).send().await.unwrap();
    assert_eq!(response.status(), 303);
    assert_eq!(response.headers()["location"], "/login");

    // The login page itself is reachable.
    let login = f.client.get(f.url("/login")).send().await.unwrap();
    assert_eq!(login.status(), 200);
}

#[tokio::test]
async fn a_wrong_password_is_refused() {
    let f = Fixture::start("").await;
    let response = login_with(&f, "not-the-password").await;
    assert_eq!(response.status(), 401);
    assert!(response.headers().get("set-cookie").is_none());
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"error":"invalid_credentials"}"#
    );
}

#[tokio::test]
async fn the_right_password_sets_a_hardened_cookie_that_grants_access() {
    let f = Fixture::start("").await;
    let response = login_with(&f, PASSWORD).await;
    assert_eq!(response.status(), 204);
    let set_cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .to_string();
    for attribute in ["HttpOnly", "SameSite=Strict", "Path=/", "Max-Age=604800"] {
        assert!(
            set_cookie.contains(attribute),
            "{attribute} in {set_cookie}"
        );
    }
    assert!(set_cookie.starts_with("gsb_session="));

    let cookie = set_cookie.split(';').next().unwrap();
    let (status, _) = f.get_json(cookie, "/api/health").await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn five_wrong_attempts_lock_the_address_out_even_for_the_right_password() {
    let f = Fixture::start("").await;
    for _ in 0..5 {
        assert_eq!(login_with(&f, "nope").await.status(), 401);
    }
    let locked = login_with(&f, "nope").await;
    assert_eq!(locked.status(), 429);
    let retry_after: u64 = locked.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=300).contains(&retry_after));
    let body: serde_json::Value = locked.json().await.unwrap();
    assert_eq!(body["error"], "too_many_attempts");

    let still_locked = login_with(&f, PASSWORD).await;
    assert_eq!(
        still_locked.status(),
        429,
        "the lockout outranks a correct password"
    );
}

#[tokio::test]
async fn logout_invalidates_the_cookie() {
    let f = Fixture::start("").await;
    let cookie = f.login().await;
    assert_eq!(f.get_json(&cookie, "/api/health").await.0, 200);

    let response = f
        .client
        .post(f.url("/api/logout"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert!(response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .contains("Max-Age=0"));

    assert_eq!(f.get_json(&cookie, "/api/health").await.0, 401);
}

#[tokio::test]
async fn a_restart_forgets_every_session() {
    let first = Fixture::start("").await;
    let cookie = first.login().await;
    assert_eq!(first.get_json(&cookie, "/api/health").await.0, 200);

    // A fresh router over the same config: sessions live in memory only.
    let loaded = gsm_sip_bridge::config::load_config_with_snapshot(&first.config_path).unwrap();
    let state = gsm_sip_bridge::webui::WebuiState::new(loaded, None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let _ = gsm_sip_bridge::webui::serve_on(listener, state).await;
    });

    let response = first
        .client
        .get(format!("{base}/api/health"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    server.abort();
}

#[tokio::test]
async fn security_headers_are_on_every_response() {
    let f = Fixture::start("").await;
    let cookie = f.login().await;
    for (path, authenticated) in [
        ("/login", false),
        ("/assets/app.js", false),
        ("/api/health", true),
        ("/api/overview", false), // a 401 carries them too
    ] {
        let mut request = f.client.get(f.url(path));
        if authenticated {
            request = request.header("cookie", &cookie);
        }
        let response = request.send().await.unwrap();
        let h = response.headers();
        assert_eq!(
            h["content-security-policy"],
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; frame-ancestors 'none'",
            "{path}"
        );
        assert_eq!(h["x-content-type-options"], "nosniff", "{path}");
        assert_eq!(h["referrer-policy"], "no-referrer", "{path}");
        if path.starts_with("/api/") {
            assert_eq!(h["cache-control"], "no-store", "{path}");
        }
    }
    let asset = f
        .client
        .get(f.url("/assets/style.css"))
        .send()
        .await
        .unwrap();
    assert_eq!(asset.headers()["cache-control"], "no-cache");
    assert!(asset.headers().get("etag").is_some());
}

#[tokio::test]
async fn writes_are_refused() {
    let f = Fixture::start("").await;
    let cookie = f.login().await;
    for path in ["/api/health", "/"] {
        let response = f
            .client
            .put(f.url(path))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 405, "PUT {path}");
    }
    let post = f
        .client
        .post(f.url("/api/health"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 405);
}

#[tokio::test]
async fn unknown_paths_are_not_found_and_metrics_is_never_served() {
    let f = Fixture::start("").await;
    let cookie = f.login().await;
    for path in ["/metrics", "/nope", "/api/nope"] {
        let response = f
            .client
            .get(f.url(path))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "{path}");
    }
}
