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
    assert!(err.contains("password"), "error should mention password: {err}");
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
