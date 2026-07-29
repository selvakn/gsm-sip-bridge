const MIGRATION_DOC: &str = include_str!("../../docs/migrating-from-v4.1.x.md");

#[test]
fn test_migration_doc_contains_all_metric_renames() {
    let expected_v5_metrics = [
        "gsm_sip_bridge_calls_total",
        "gsm_sip_bridge_sip_calls_total",
        "gsm_sip_bridge_sip_registrations_total",
        "gsm_sip_bridge_module_init_total",
        "gsm_sip_bridge_module_retries_total",
        "gsm_sip_bridge_audio_errors_total",
        "gsm_sip_bridge_sip_registered",
        "gsm_sip_bridge_modules_active",
        "gsm_sip_bridge_modules_failed",
        "gsm_sip_bridge_active_calls",
        "gsm_sip_bridge_uptime_seconds",
        "gsm_sip_bridge_sms_received_total",
        "gsm_sip_bridge_sms_forwarded_total",
        "gsm_sip_bridge_sms_db_writes_total",
        "gsm_sip_bridge_call_duration_seconds",
    ];

    for metric in &expected_v5_metrics {
        assert!(
            MIGRATION_DOC.contains(metric),
            "migration doc missing metric rename: {metric}"
        );
    }
}

#[test]
fn test_migration_doc_contains_config_mapping() {
    let required_keys = [
        "server",
        "port",
        "username",
        "password",
        "transport",
        "local_port",
        "display_name",
        "sip_destination",
        "sip_dial_timeout_sec",
        "discord_webhook_url",
        "db_path",
    ];

    for key in &required_keys {
        assert!(
            MIGRATION_DOC.contains(key),
            "migration doc missing config key: {key}"
        );
    }
}

#[test]
fn test_migration_doc_contains_sql_section() {
    assert!(MIGRATION_DOC.contains("CREATE TABLE"));
    assert!(MIGRATION_DOC.contains("INSERT"));
    assert!(MIGRATION_DOC.contains("VACUUM"));
    assert!(MIGRATION_DOC.contains("ATTACH DATABASE"));
}

#[test]
fn test_migration_doc_contains_rollback() {
    assert!(MIGRATION_DOC.contains("Roll-back"));
    assert!(MIGRATION_DOC.contains("docker compose down"));
}

#[test]
fn test_migration_doc_contains_cli_mapping() {
    assert!(MIGRATION_DOC.contains("--config"));
    assert!(MIGRATION_DOC.contains("--verbose"));
}
