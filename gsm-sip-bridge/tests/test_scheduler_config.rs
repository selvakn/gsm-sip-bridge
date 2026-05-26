//! Integration tests for User Story 2 (operator configures schedule & jitter).
//!
//! Verifies that:
//!   - Defaults apply when the `[scheduled_restart]` section is omitted.
//!   - `enabled = false` disables the feature.
//!   - Invalid cron disables the scheduler but does not abort daemon startup.
//!   - Out-of-range or inconsistent values disable the scheduler.
//!
//! These tests exercise `config::load_config` end-to-end, which is what `main()`
//! calls during real startup.

use gsm_sip_bridge::config::load_config;
use std::io::Write;
use tempfile::NamedTempFile;

const SIP_BLOCK: &str = r#"
[sip]
server = "sip.example.com"
username = "bridge"
password = "secret"
"#;

fn write_config(extra: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(SIP_BLOCK.as_bytes()).unwrap();
    f.write_all(b"\n").unwrap();
    f.write_all(extra.as_bytes()).unwrap();
    f
}

#[test]
fn scheduled_restart_defaults_when_section_omitted() {
    let f = write_config("");
    let cfg = load_config(f.path()).expect("config must load");
    let s = &cfg.scheduled_restart;
    assert!(s.enabled, "scheduler must default to enabled");
    assert_eq!(s.cron, "0 1 * * *");
    assert_eq!(s.start_jitter_seconds, 600);
    assert_eq!(s.inter_card_gap_seconds, 30);
    assert_eq!(s.inter_card_gap_jitter_seconds, 15);
}

#[test]
fn scheduled_restart_explicit_disable_respected() {
    let f = write_config(
        r#"
[scheduled_restart]
enabled = false
"#,
    );
    let cfg = load_config(f.path()).unwrap();
    assert!(!cfg.scheduled_restart.enabled);
}

#[test]
fn scheduled_restart_invalid_cron_disables_feature_but_daemon_continues() {
    let f = write_config(
        r#"
[scheduled_restart]
cron = "0 25 * * *"
"#,
    );
    // The daemon MUST still load the config; only the scheduler is disabled.
    let cfg = load_config(f.path()).expect("daemon must continue past invalid cron");
    assert!(
        !cfg.scheduled_restart.enabled,
        "invalid cron must disable scheduled_restart"
    );
    // The rest of the bridge config is still intact.
    assert_eq!(cfg.sip.server, "sip.example.com");
}

#[test]
fn scheduled_restart_custom_cron_applied() {
    let f = write_config(
        r#"
[scheduled_restart]
enabled = true
cron = "30 2 * * 1-5"
start_jitter_seconds = 0
inter_card_gap_seconds = 60
inter_card_gap_jitter_seconds = 30
"#,
    );
    let cfg = load_config(f.path()).unwrap();
    let s = &cfg.scheduled_restart;
    assert!(s.enabled);
    assert_eq!(s.cron, "30 2 * * 1-5");
    assert_eq!(s.start_jitter_seconds, 0);
    assert_eq!(s.inter_card_gap_seconds, 60);
    assert_eq!(s.inter_card_gap_jitter_seconds, 30);
}

#[test]
fn scheduled_restart_jitter_greater_than_gap_disables() {
    let f = write_config(
        r#"
[scheduled_restart]
inter_card_gap_seconds = 5
inter_card_gap_jitter_seconds = 20
"#,
    );
    let cfg = load_config(f.path()).unwrap();
    assert!(
        !cfg.scheduled_restart.enabled,
        "jitter > gap must disable scheduler"
    );
}

#[test]
fn scheduled_restart_start_jitter_out_of_range_disables() {
    let f = write_config(
        r#"
[scheduled_restart]
start_jitter_seconds = 99999999
"#,
    );
    let cfg = load_config(f.path()).unwrap();
    assert!(!cfg.scheduled_restart.enabled);
}

#[test]
fn scheduled_restart_unknown_key_warned_but_does_not_disable() {
    // Unknown keys should produce a tracing::warn but the config still parses.
    let f = write_config(
        r#"
[scheduled_restart]
enabled = true
unknown_field = "ignored"
"#,
    );
    let cfg = load_config(f.path()).unwrap();
    assert!(
        cfg.scheduled_restart.enabled,
        "unknown key alone must not disable the scheduler"
    );
}
