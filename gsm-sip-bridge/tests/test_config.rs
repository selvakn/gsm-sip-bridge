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
fn test_unknown_keys_are_rejected_and_all_are_reported_at_once() {
    // Previously these only produced a `tracing::warn!` and startup
    // continued. A typo'd key therefore silently did nothing: `max_line = 2`
    // (missing the `s`) left the real setting at its default, and the single
    // WARN was buried in a container's modem-probing startup noise — often
    // emitted before the configured log level had even been applied.
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
    let err = load_config(f.path()).expect_err("an unknown key must fail the load");
    let msg = err.to_string();

    // Every offender is named in one error, so an operator with several
    // typos learns about all of them in one run rather than one per restart.
    assert!(msg.contains("sip.future_key"), "got: {msg}");
    assert!(msg.contains("unknown_section"), "got: {msg}");
    // And the message says where to look.
    assert!(msg.contains("docs/configuration.md"), "got: {msg}");
}

/// The error names the section too, not just the bare key — `max_lines` is a
/// real key in both `[vowifi]` and `[volte]`, so an unqualified name would
/// not tell the operator which section to fix.
#[test]
fn test_an_unknown_key_is_reported_with_its_section() {
    std::env::set_var("TEST_UNK2_PASSWORD", "p");

    let f = write_config(
        r#"
[sip]
server = "127.0.0.1"
username = "user"
password = "env:TEST_UNK2_PASSWORD"

[vowifi]
max_line = 2
"#,
    );

    let msg = load_config(f.path()).unwrap_err().to_string();
    assert!(msg.contains("vowifi.max_line"), "got: {msg}");
}

/// A config using only real keys still loads — the check must not have become
/// so strict that a valid deployment is rejected.
#[test]
fn test_the_shipped_example_config_still_loads() {
    std::env::set_var("SIP_PASSWORD", "p");
    std::env::set_var("DISCORD_WEBHOOK_URL", "https://discord.com/api/webhooks/x");

    let example = include_str!("../../config.toml.example");
    let f = write_config(example);

    load_config(f.path())
        .expect("config.toml.example must load — it is what operators copy to start from");
}

/// specs/026-disable-circuit-switched FR-002: the single highest-risk
/// assertion in the feature. `[cs]` is the first *opt-out* flag in the file —
/// every other `enabled` key defaults to `false` (opt-in). If `RawCs` ever
/// grows a derived `Default` instead of a hand-written one, this silently
/// disables circuit switching for every existing deployment on upgrade.
#[test]
fn cs_defaults_to_enabled_when_section_absent() {
    std::env::set_var("TEST_CS_ABSENT_PASSWORD", "p");

    let f = write_config(
        r#"
[sip]
server = "127.0.0.1"
username = "user"
password = "env:TEST_CS_ABSENT_PASSWORD"
"#,
    );

    let cfg = load_config(f.path()).unwrap();
    assert!(
        cfg.cs.enabled,
        "a config written before this feature existed must behave identically after upgrade"
    );
}

#[test]
fn cs_defaults_to_enabled_when_section_present_but_key_absent() {
    std::env::set_var("TEST_CS_PRESENT_PASSWORD", "p");

    let f = write_config(
        r#"
[sip]
server = "127.0.0.1"
username = "user"
password = "env:TEST_CS_PRESENT_PASSWORD"

[cs]
"#,
    );

    let cfg = load_config(f.path()).unwrap();
    assert!(cfg.cs.enabled);
}

#[test]
fn cs_enabled_false_is_honoured() {
    std::env::set_var("TEST_CS_FALSE_PASSWORD", "p");

    let f = write_config(
        r#"
[sip]
server = "127.0.0.1"
username = "user"
password = "env:TEST_CS_FALSE_PASSWORD"

[cs]
enabled = false
"#,
    );

    let cfg = load_config(f.path()).unwrap();
    assert!(!cfg.cs.enabled);
}

#[test]
fn cs_unknown_key_is_rejected() {
    std::env::set_var("TEST_CS_UNKNOWN_PASSWORD", "p");

    let f = write_config(
        r#"
[sip]
server = "127.0.0.1"
username = "user"
password = "env:TEST_CS_UNKNOWN_PASSWORD"

[cs]
bogus = 1
"#,
    );

    let msg = load_config(f.path()).unwrap_err().to_string();
    assert!(msg.contains("cs.bogus"), "got: {msg}");
}

/// FR-003: every combination of `[cs]`, `[vowifi]`, `[volte]` enable flags is
/// valid configuration — enabling one MUST NOT implicitly change another.
#[test]
fn cs_vowifi_volte_every_combination_is_accepted() {
    for (cs, vowifi, volte) in [
        (true, false, false),
        (false, false, false),
        (true, true, false),
        (false, true, false),
        (true, false, true),
        (false, false, true),
        // Both true is rejected by supervise::orchestrate, not by config
        // loading — load_config itself must still accept it.
        (true, true, true),
        (false, true, true),
    ] {
        std::env::set_var("TEST_CS_COMBO_PASSWORD", "p");
        let f = write_config(&format!(
            r#"
[sip]
server = "127.0.0.1"
username = "user"
password = "env:TEST_CS_COMBO_PASSWORD"

[cs]
enabled = {cs}

[vowifi]
enabled = {vowifi}

[volte]
enabled = {volte}
"#
        ));

        let cfg = load_config(f.path())
            .unwrap_or_else(|e| panic!("cs={cs} vowifi={vowifi} volte={volte} rejected: {e}"));
        assert_eq!(cfg.cs.enabled, cs);
        assert_eq!(cfg.vowifi.enabled, vowifi);
        assert_eq!(cfg.volte.enabled, volte);
    }
}

/// FR-025: circuit-switched-specific tuning stays valid and has no bearing on
/// whether the config loads when the path is disabled.
#[test]
fn cs_disabled_leaves_related_sections_valid() {
    std::env::set_var("TEST_CS_INERT_PASSWORD", "p");

    let f = write_config(
        r#"
[sip]
server = "127.0.0.1"
username = "user"
password = "env:TEST_CS_INERT_PASSWORD"

[cs]
enabled = false

[modules]
retry_interval_sec = 45
max_concurrent = 3

[resilience]
initial_backoff_sec = 10
max_backoff_sec = 200
max_retries = 5
network_loss_timeout_sec = 90
network_poll_interval_sec = 45

[scheduled_restart]
enabled = true
cron = "0 2 * * *"

[modem_audio]
tx_level = 0.8
"#,
    );

    let cfg = load_config(f.path()).unwrap();
    assert!(!cfg.cs.enabled);
    assert_eq!(cfg.modules.retry_interval_sec, 45);
    assert_eq!(cfg.modules.max_concurrent, 3);
}

/// specs/026-disable-circuit-switched User Story 2: every configuration that
/// predates this feature — every shipped sample except the one this feature
/// itself added to demonstrate `[cs]` — must keep behaving exactly as
/// before after upgrade. Loads every real sample config in
/// `sample_configs/` (not fabricated fixtures) and asserts each resolves
/// `cs.enabled == true` unless it explicitly names `[cs]` itself, the
/// load-bearing regression check for "a config with no [cs] section is
/// unaffected".
#[test]
fn every_shipped_sample_config_defaults_circuit_switching_to_enabled() {
    std::env::set_var("SIP_PASSWORD", "p");
    std::env::set_var("DISCORD_WEBHOOK_URL", "https://discord.com/api/webhooks/x");
    std::env::set_var("PHONE_1001_PASSWORD", "p");

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../sample_configs");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("sample_configs/ must exist") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        let cfg =
            load_config(&path).unwrap_or_else(|e| panic!("{} failed to load: {e}", path.display()));
        if raw.contains("[cs]") {
            // Explicitly opts into the new section — not a pre-existing
            // config, so it's exempt from the "unaffected by upgrade" claim
            // this test otherwise makes.
            checked += 1;
            continue;
        }
        assert!(
            cfg.cs.enabled,
            "{} has no [cs] section, so it must default to enabled — a regression here means \
             upgrading silently disables circuit switching on this deployment shape",
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "expected at least one sample config in {}",
        dir.display()
    );
}
