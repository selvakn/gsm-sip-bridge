mod common;

use gsm_sip_bridge::config::load_config;
use std::io::Write;
use tempfile::NamedTempFile;

fn write_config(content: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f
}

#[test]
fn test_load_full_config() {
    std::env::set_var("TEST_SIP_PASSWORD", "secret123");
    std::env::set_var("TEST_DISCORD_URL", "https://discord.com/api/webhooks/test");

    let config = r#"
[sip]
server = "pbx.example.com"
port = 5060
username = "bridge"
password = "env:TEST_SIP_PASSWORD"
transport = "udp"
local_port = 5060
display_name = "GSM Bridge"
tls_verify = "strict"

[bridge]
sip_destination = ""
sip_dial_timeout_sec = 30

[sms]
enabled = true
discord_webhook_url = "env:TEST_DISCORD_URL"
db_path = "/tmp/test-store.db"

[metrics]
port = 9091

[modules]
retry_interval_sec = 30
max_concurrent = 8
"#;

    let f = write_config(config);
    let result = load_config(f.path());
    assert!(result.is_ok(), "config load failed: {:?}", result.err());

    let cfg = result.unwrap();
    assert_eq!(cfg.sip.server, "pbx.example.com");
    assert_eq!(cfg.sip.port, 5060);
    assert_eq!(cfg.sip.username, "bridge");
    assert_eq!(cfg.sip.password.expose_secret(), "secret123");
    assert_eq!(cfg.bridge.sip_dial_timeout_sec, 30);
    assert!(cfg.sms.enabled);
    assert_eq!(
        cfg.sms.discord_webhook_url.expose_secret(),
        "https://discord.com/api/webhooks/test"
    );
    assert_eq!(cfg.modules.max_concurrent, 8);
}

#[test]
fn test_load_minimal_config() {
    std::env::set_var("TEST_MINIMAL_PASSWORD", "pass");

    let config = r#"
[sip]
server = "127.0.0.1"
username = "user"
password = "env:TEST_MINIMAL_PASSWORD"
"#;

    let f = write_config(config);
    let cfg = load_config(f.path()).unwrap();
    assert_eq!(cfg.sip.port, 5060);
    assert_eq!(cfg.metrics.port, 9091);
    assert_eq!(cfg.modules.retry_interval_sec, 30);
}

#[test]
fn test_missing_required_field() {
    let config = r#"
[sip]
server = "127.0.0.1"
username = "user"
"#;

    let f = write_config(config);
    let result = load_config(f.path());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("password"),
        "error should mention password: {err}"
    );
}

#[test]
fn test_out_of_range_value() {
    std::env::set_var("TEST_RANGE_PASSWORD", "p");

    let config = r#"
[sip]
server = "127.0.0.1"
username = "user"
password = "env:TEST_RANGE_PASSWORD"

[bridge]
sip_dial_timeout_sec = 999
"#;

    let f = write_config(config);
    let result = load_config(f.path());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("5..=120"), "error should show range: {err}");
}

#[test]
fn test_unset_env_var() {
    std::env::remove_var("NONEXISTENT_VAR_FOR_TEST");

    let config = r#"
[sip]
server = "127.0.0.1"
username = "user"
password = "env:NONEXISTENT_VAR_FOR_TEST"
"#;

    let f = write_config(config);
    let result = load_config(f.path());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("NONEXISTENT_VAR_FOR_TEST"),
        "error should name the var: {err}"
    );
}

/// specs/022-discord-critical-alerts FR-001 backward compatibility: with no
/// `[alerts]` section at all, `alerts.sms` is seeded from the legacy
/// `[sms]` keys (as its own webhook override, since it was never shared
/// before this feature), and the four new categories default to disabled
/// with their documented default thresholds.
#[test]
fn test_alerts_defaults_seeded_from_legacy_sms_when_alerts_section_absent() {
    std::env::set_var("TEST_ALERTS_PASSWORD", "p");
    std::env::set_var(
        "TEST_ALERTS_LEGACY_WEBHOOK",
        "https://discord.com/api/webhooks/legacy",
    );

    let config = r#"
[sip]
server = "127.0.0.1"
username = "user"
password = "env:TEST_ALERTS_PASSWORD"

[sms]
enabled = true
discord_webhook_url = "env:TEST_ALERTS_LEGACY_WEBHOOK"
"#;

    let f = write_config(config);
    let cfg = load_config(f.path()).unwrap();

    assert!(cfg.alerts.sms.enabled);
    assert_eq!(
        cfg.alerts
            .sms
            .webhook_url_override
            .as_ref()
            .map(|s| s.expose_secret().as_str()),
        Some("https://discord.com/api/webhooks/legacy")
    );
    assert!(!cfg.alerts.module_lifecycle.enabled);
    assert!(!cfg.alerts.registration_loss.enabled);
    assert!(!cfg.alerts.tunnel_failure.enabled);
    assert!(!cfg.alerts.missed_call.enabled);
    assert_eq!(
        cfg.alerts
            .module_lifecycle_thresholds
            .at_worker_unresponsive_sec,
        60
    );
    assert_eq!(cfg.alerts.tunnel_failure_thresholds.unhealthy_sec, 300);
    assert_eq!(cfg.alerts.registration_loss_thresholds.unhealthy_sec, 300);
}

/// An explicit `[alerts.sms]` table wins over the legacy `[sms]` keys.
#[test]
fn test_alerts_sms_table_present_overrides_legacy_sms_section() {
    std::env::set_var("TEST_ALERTS_SMS_PASSWORD", "p");

    let config = r#"
[sip]
server = "127.0.0.1"
username = "user"
password = "env:TEST_ALERTS_SMS_PASSWORD"

[sms]
enabled = true
discord_webhook_url = "https://discord.com/api/webhooks/legacy"

[alerts.sms]
enabled = false
"#;

    let f = write_config(config);
    let cfg = load_config(f.path()).unwrap();

    assert!(!cfg.alerts.sms.enabled, "[alerts.sms] must win over [sms]");
}

/// Per-category enable + webhook override, and the shared default webhook
/// used by any category without its own override.
#[test]
fn test_alerts_per_category_enable_and_webhook_override() {
    std::env::set_var("TEST_ALERTS_CAT_PASSWORD", "p");

    let config = r#"
[sip]
server = "127.0.0.1"
username = "user"
password = "env:TEST_ALERTS_CAT_PASSWORD"

[alerts]
discord_webhook_url = "https://discord.com/api/webhooks/default"

[alerts.module_lifecycle]
enabled = true
discord_webhook_url = "https://discord.com/api/webhooks/module-lifecycle"
at_worker_unresponsive_sec = 90

[alerts.tunnel_failure]
enabled = true
unhealthy_sec = 120
"#;

    let f = write_config(config);
    let cfg = load_config(f.path()).unwrap();

    assert_eq!(
        cfg.alerts.default_webhook_url.expose_secret(),
        "https://discord.com/api/webhooks/default"
    );
    assert!(cfg.alerts.module_lifecycle.enabled);
    assert_eq!(
        cfg.alerts
            .module_lifecycle
            .webhook_url_override
            .as_ref()
            .map(|s| s.expose_secret().as_str()),
        Some("https://discord.com/api/webhooks/module-lifecycle")
    );
    assert_eq!(
        cfg.alerts
            .module_lifecycle_thresholds
            .at_worker_unresponsive_sec,
        90
    );
    assert!(cfg.alerts.tunnel_failure.enabled);
    assert!(cfg.alerts.tunnel_failure.webhook_url_override.is_none());
    assert_eq!(cfg.alerts.tunnel_failure_thresholds.unhealthy_sec, 120);
    assert!(!cfg.alerts.registration_loss.enabled);
}

/// An out-of-range threshold falls back to the default rather than failing
/// the whole config load (spec edge case: a malformed alerts config must
/// never keep the bridge from starting).
#[test]
fn test_alerts_out_of_range_threshold_falls_back_to_default() {
    std::env::set_var("TEST_ALERTS_RANGE_PASSWORD", "p");

    let config = r#"
[sip]
server = "127.0.0.1"
username = "user"
password = "env:TEST_ALERTS_RANGE_PASSWORD"

[alerts.module_lifecycle]
enabled = true
at_worker_unresponsive_sec = 99999
"#;

    let f = write_config(config);
    let result = load_config(f.path());
    assert!(
        result.is_ok(),
        "an out-of-range alerts threshold must not fail config load: {:?}",
        result.err()
    );
    let cfg = result.unwrap();
    assert!(
        cfg.alerts.module_lifecycle.enabled,
        "enabled flag unaffected"
    );
    assert_eq!(
        cfg.alerts
            .module_lifecycle_thresholds
            .at_worker_unresponsive_sec,
        60,
        "falls back to the default threshold"
    );
}

#[test]
fn test_unknown_key_does_not_fail() {
    std::env::set_var("TEST_UNK_PASSWORD", "p");

    let config = r#"
[sip]
server = "127.0.0.1"
username = "user"
password = "env:TEST_UNK_PASSWORD"
future_key = "something"

[unknown_section]
x = 1
"#;

    let f = write_config(config);
    let result = load_config(f.path());
    assert!(result.is_ok(), "unknown keys should not cause failure");
}
