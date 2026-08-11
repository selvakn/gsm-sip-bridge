//! specs/022-discord-critical-alerts (T012/T018/T024/T028/T030-T032).
//! `wiremock` is the external-service mock convention already declared as a
//! dev-dependency for exactly this purpose — Discord itself is the kind of
//! third-party service Constitution Principle I carves out as impractical to
//! run for real in tests.

use chrono::Utc;
use gsm_sip_bridge::alerts::discord::DiscordClient;
use gsm_sip_bridge::alerts::{dispatch, AlertCategory, CriticalEvent, CriticalEventKind};
use gsm_sip_bridge::config::secret::Secret;
use gsm_sip_bridge::config::{AlertsConfig, CategoryAlertConfig};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn enabled_alerts_config(default_webhook: &str) -> AlertsConfig {
    AlertsConfig {
        default_webhook_url: Secret::new(default_webhook.to_string()),
        instance_name: Some("test-instance".to_string()),
        sms: CategoryAlertConfig {
            enabled: true,
            webhook_url_override: None,
        },
        module_lifecycle: CategoryAlertConfig {
            enabled: true,
            webhook_url_override: None,
        },
        registration_loss: CategoryAlertConfig {
            enabled: true,
            webhook_url_override: None,
        },
        tunnel_failure: CategoryAlertConfig {
            enabled: true,
            webhook_url_override: None,
        },
        missed_call: CategoryAlertConfig {
            enabled: true,
            webhook_url_override: None,
        },
        line_discovery_failed: CategoryAlertConfig {
            enabled: true,
            webhook_url_override: None,
        },
        gm_connection_lost: CategoryAlertConfig {
            enabled: true,
            webhook_url_override: None,
        },
        module_lifecycle_thresholds: Default::default(),
        tunnel_failure_thresholds: Default::default(),
        registration_loss_thresholds: Default::default(),
        gm_connection_lost_thresholds: Default::default(),
    }
}

fn module_lifecycle_event() -> CriticalEvent {
    CriticalEvent {
        category: AlertCategory::ModuleLifecycle,
        unit_id: Some("card0".to_string()),
        description: "SIM unreadable after 5 recovery attempts".to_string(),
        phone_number: None,
        at: Utc::now(),
        kind: CriticalEventKind::Failure,
    }
}

/// The parsed JSON body of the single request the mock server received.
async fn captured_embed(server: &MockServer) -> serde_json::Value {
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1, "exactly one POST expected");
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    body["embeds"][0].clone()
}

fn field_value<'a>(embed: &'a serde_json::Value, name: &str) -> Option<&'a str> {
    embed["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == name)
        .and_then(|f| f["value"].as_str())
}

/// specs/034-alert-identity (US1+US2): a critical-event embed carries the
/// instance name in its footer and a `Phone` field with the resolved number.
#[tokio::test]
async fn send_alert_embed_includes_instance_footer_and_phone_field() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let client = DiscordClient::new(Secret::new(String::new()), "bridge-01".to_string()).unwrap();
    let webhook = format!("{}/webhook", server.uri());

    let event = CriticalEvent {
        category: AlertCategory::ModuleLifecycle,
        unit_id: Some("ec20-A1B2C3".to_string()),
        description: "SIM unreadable".to_string(),
        phone_number: Some("+919000000001".to_string()),
        at: Utc::now(),
        kind: CriticalEventKind::Failure,
    };
    client.send_alert(&webhook, &event).await.unwrap();

    let embed = captured_embed(&server).await;
    assert_eq!(embed["footer"]["text"], "gsm-sip-bridge · bridge-01");
    assert_eq!(field_value(&embed, "Phone"), Some("+919000000001"));
}

/// specs/034-alert-identity (FR-005): an unresolved number renders the literal
/// `unknown`, and the alert still posts.
#[tokio::test]
async fn send_alert_embed_shows_unknown_when_no_phone() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let client = DiscordClient::new(Secret::new(String::new()), "bridge-01".to_string()).unwrap();
    let webhook = format!("{}/webhook", server.uri());

    client
        .send_alert(&webhook, &module_lifecycle_event())
        .await
        .unwrap();

    let embed = captured_embed(&server).await;
    assert_eq!(field_value(&embed, "Phone"), Some("unknown"));
}

/// specs/034-alert-identity (US1+US2): an SMS-forward embed likewise carries the
/// instance footer and a `Phone` field with the receiving card's number.
#[tokio::test]
async fn forward_sms_embed_includes_instance_footer_and_phone_field() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let client = DiscordClient::new(
        Secret::new(format!("{}/webhook", server.uri())),
        "bridge-01".to_string(),
    )
    .unwrap();

    client
        .forward_sms(
            "ec20-A1B2C3",
            "+919000000000",
            "hi",
            "2026-08-11T00:00:00Z",
            Some("+919000000001"),
        )
        .await
        .unwrap();

    let embed = captured_embed(&server).await;
    assert_eq!(embed["footer"]["text"], "gsm-sip-bridge · bridge-01");
    assert_eq!(field_value(&embed, "Phone"), Some("+919000000001"));
}

/// T012 (US1): dispatching a `ModuleLifecycle` event posts an embed
/// containing the module id and description to the resolved webhook.
#[tokio::test]
async fn dispatch_posts_module_lifecycle_alert_with_module_id_and_description() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let client =
        DiscordClient::new(Secret::new(String::new()), "test-instance".to_string()).unwrap();
    let config = enabled_alerts_config(&format!("{}/webhook", server.uri()));

    dispatch(&client, &config, module_lifecycle_event()).await;

    server.verify().await;
}

/// T024 (US3): a `TunnelFailure` event is distinct from a `RegistrationLoss`
/// one for the same line — both post, and neither is dropped.
#[tokio::test]
async fn dispatch_posts_tunnel_and_registration_alerts_independently() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(204))
        .expect(2)
        .mount(&server)
        .await;

    let client =
        DiscordClient::new(Secret::new(String::new()), "test-instance".to_string()).unwrap();
    let config = enabled_alerts_config(&format!("{}/webhook", server.uri()));

    dispatch(
        &client,
        &config,
        CriticalEvent {
            category: AlertCategory::TunnelFailure,
            unit_id: Some("line0".to_string()),
            description: "tunnel non-established for 300s".to_string(),
            phone_number: None,
            at: Utc::now(),
            kind: CriticalEventKind::Failure,
        },
    )
    .await;
    dispatch(
        &client,
        &config,
        CriticalEvent {
            category: AlertCategory::RegistrationLoss,
            unit_id: Some("line0".to_string()),
            description: "unregistered for 300s".to_string(),
            phone_number: None,
            at: Utc::now(),
            kind: CriticalEventKind::Failure,
        },
    )
    .await;

    server.verify().await;
}

/// specs/027-discover-retry-health T017/T018: a `LineDiscoveryFailed`
/// `Failure` event, followed later by its paired `Recovered` event for the
/// same identifier, both post — mirroring the existing `TunnelFailure`/
/// `RegistrationLoss` failure-then-recovered pairing other categories
/// already use — and drive the same `gsm_sip_bridge_critical_event_active`
/// gauge every ongoing-health category already reports through (FR-012/
/// SC-006: no bespoke metric needed, see data-model.md's revision note).
#[tokio::test]
async fn dispatch_posts_line_discovery_failed_and_its_recovery_pair() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(204))
        .expect(2)
        .mount(&server)
        .await;

    let client =
        DiscordClient::new(Secret::new(String::new()), "test-instance".to_string()).unwrap();
    let config = enabled_alerts_config(&format!("{}/webhook", server.uri()));
    let identifier = "/dev/ttyUSB3-recovery-pair-test";

    dispatch(
        &client,
        &config,
        CriticalEvent {
            category: AlertCategory::LineDiscoveryFailed,
            unit_id: Some(identifier.to_string()),
            description:
                "configured VoWiFi line /dev/ttyUSB3 was not found after 3m of retrying discovery"
                    .to_string(),
            phone_number: None,
            at: Utc::now(),
            kind: CriticalEventKind::Failure,
        },
    )
    .await;
    assert_eq!(
        gsm_sip_bridge::metrics::CRITICAL_EVENT_ACTIVE
            .with_label_values(&["line_discovery_failed", identifier])
            .get(),
        1.0,
        "gauge must be 1 once the Failure event is dispatched"
    );

    dispatch(
        &client,
        &config,
        CriticalEvent {
            category: AlertCategory::LineDiscoveryFailed,
            unit_id: Some(identifier.to_string()),
            description: "configured VoWiFi line /dev/ttyUSB3 was found and started after previously being reported as not found".to_string(),
            phone_number: None,
            at: Utc::now(),
            kind: CriticalEventKind::Recovered,
        },
    )
    .await;
    assert_eq!(
        gsm_sip_bridge::metrics::CRITICAL_EVENT_ACTIVE
            .with_label_values(&["line_discovery_failed", identifier])
            .get(),
        0.0,
        "gauge must clear back to 0 once the Recovered event is dispatched"
    );

    server.verify().await;
}

/// specs/027-discover-retry-health FR-010/SC-004: `line_discovery_failed`
/// disabled makes zero HTTP calls, same as every other category.
#[tokio::test]
async fn dispatch_skips_disabled_line_discovery_failed_without_any_http_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&server)
        .await;

    let client =
        DiscordClient::new(Secret::new(String::new()), "test-instance".to_string()).unwrap();
    let mut config = enabled_alerts_config(&format!("{}/webhook", server.uri()));
    config.line_discovery_failed.enabled = false;

    dispatch(
        &client,
        &config,
        CriticalEvent {
            category: AlertCategory::LineDiscoveryFailed,
            unit_id: Some("/dev/ttyUSB3".to_string()),
            description: "configured VoWiFi line /dev/ttyUSB3 was not found".to_string(),
            phone_number: None,
            at: Utc::now(),
            kind: CriticalEventKind::Failure,
        },
    )
    .await;

    server.verify().await;
}

/// T028 (US4): a `MissedCall` event posts an embed with the caller/line
/// details baked into its description.
#[tokio::test]
async fn dispatch_posts_missed_call_alert() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let client =
        DiscordClient::new(Secret::new(String::new()), "test-instance".to_string()).unwrap();
    let config = enabled_alerts_config(&format!("{}/webhook", server.uri()));

    dispatch(
        &client,
        &config,
        CriticalEvent {
            category: AlertCategory::MissedCall,
            unit_id: Some("card0".to_string()),
            description: "call from +911234567890 was never answered".to_string(),
            phone_number: None,
            at: Utc::now(),
            kind: CriticalEventKind::Failure,
        },
    )
    .await;

    server.verify().await;
}

/// T030 (US5): a disabled category makes zero HTTP calls.
#[tokio::test]
async fn dispatch_skips_disabled_category_without_any_http_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/webhook"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&server)
        .await;

    let client =
        DiscordClient::new(Secret::new(String::new()), "test-instance".to_string()).unwrap();
    let mut config = enabled_alerts_config(&format!("{}/webhook", server.uri()));
    config.missed_call.enabled = false;

    dispatch(
        &client,
        &config,
        CriticalEvent {
            category: AlertCategory::MissedCall,
            unit_id: Some("card0".to_string()),
            description: "call from +911234567890 was never answered".to_string(),
            phone_number: None,
            at: Utc::now(),
            kind: CriticalEventKind::Failure,
        },
    )
    .await;

    server.verify().await;
}

/// T031 (US5): a category-specific webhook override routes only that
/// category's alerts to the override, not the shared default.
#[tokio::test]
async fn dispatch_uses_category_webhook_override_not_shared_default() {
    let default_server = MockServer::start().await;
    let override_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/default"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&default_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/override"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&override_server)
        .await;

    let client =
        DiscordClient::new(Secret::new(String::new()), "test-instance".to_string()).unwrap();
    let mut config = enabled_alerts_config(&format!("{}/default", default_server.uri()));
    config.module_lifecycle.webhook_url_override =
        Some(Secret::new(format!("{}/override", override_server.uri())));

    dispatch(&client, &config, module_lifecycle_event()).await;

    default_server.verify().await;
    override_server.verify().await;
}

/// T032 (US5)/FR-001 regression guard: with no default webhook and no
/// override configured for a category, no Discord call is attempted.
#[tokio::test]
async fn dispatch_skips_when_no_webhook_resolves_at_all() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&server)
        .await;

    let client =
        DiscordClient::new(Secret::new(String::new()), "test-instance".to_string()).unwrap();
    let config = enabled_alerts_config("");

    dispatch(&client, &config, module_lifecycle_event()).await;
}
