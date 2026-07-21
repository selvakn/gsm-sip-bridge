mod common;

use gsm_sip_bridge::metrics;

fn init_metrics() {
    metrics::CALLS_TOTAL
        .with_label_values(&["test", "answered", "cs"])
        .inc();
    metrics::SIP_CALLS_TOTAL
        .with_label_values(&["test", "success", "cs"])
        .inc();
    metrics::CALL_DURATION_SECONDS
        .with_label_values(&["test", "cs"])
        .observe(1.0);
    metrics::ACTIVE_CALLS
        .with_label_values(&["test", "cs"])
        .set(0.0);
    metrics::SIP_REGISTRATIONS_TOTAL
        .with_label_values(&["success"])
        .inc();
    metrics::SIP_REGISTERED.set(0.0);
    metrics::MODULE_INIT_TOTAL
        .with_label_values(&["test", "success", "none"])
        .inc();
    metrics::MODULE_RETRIES_TOTAL
        .with_label_values(&["test"])
        .inc();
    metrics::MODULES_ACTIVE.set(0.0);
    metrics::MODULES_FAILED.set(0.0);
    metrics::AUDIO_ERRORS_TOTAL
        .with_label_values(&["test", "underrun"])
        .inc();
    metrics::SMS_RECEIVED_TOTAL
        .with_label_values(&["test", "cs"])
        .inc();
    metrics::SMS_FORWARDED_TOTAL
        .with_label_values(&["test", "sent", "cs"])
        .inc();
    metrics::SMS_DB_WRITES_TOTAL
        .with_label_values(&["success"])
        .inc();
    metrics::UPTIME_SECONDS.set(1.0);
    metrics::BUILD_INFO
        .with_label_values(&["5.0.0", "test", "2.16", "1.80.0"])
        .set(1.0);
}

#[test]
fn test_v4_to_v5_metric_rename_coverage() {
    init_metrics();

    let encoder = prometheus::TextEncoder::new();
    let families = prometheus::gather();
    let output = encoder.encode_to_string(&families).unwrap();

    let v4_to_v5_renames = [
        ("gsm_bridge_calls_total", "gsm_sip_bridge_calls_total"),
        (
            "gsm_bridge_sip_calls_total",
            "gsm_sip_bridge_sip_calls_total",
        ),
        (
            "gsm_bridge_sip_registrations_total",
            "gsm_sip_bridge_sip_registrations_total",
        ),
        (
            "gsm_bridge_module_init_total",
            "gsm_sip_bridge_module_init_total",
        ),
        (
            "gsm_bridge_module_retries_total",
            "gsm_sip_bridge_module_retries_total",
        ),
        (
            "gsm_bridge_audio_errors_total",
            "gsm_sip_bridge_audio_errors_total",
        ),
        ("gsm_bridge_sip_registered", "gsm_sip_bridge_sip_registered"),
        ("gsm_bridge_modules_active", "gsm_sip_bridge_modules_active"),
        ("gsm_bridge_modules_failed", "gsm_sip_bridge_modules_failed"),
        ("gsm_bridge_active_calls", "gsm_sip_bridge_active_calls"),
        ("gsm_bridge_uptime_seconds", "gsm_sip_bridge_uptime_seconds"),
        (
            "gsm_bridge_sms_received_total",
            "gsm_sip_bridge_sms_received_total",
        ),
        (
            "gsm_bridge_sms_forwarded_total",
            "gsm_sip_bridge_sms_forwarded_total",
        ),
        (
            "gsm_bridge_sms_db_writes_total",
            "gsm_sip_bridge_sms_db_writes_total",
        ),
        (
            "gsm_bridge_call_duration_seconds",
            "gsm_sip_bridge_call_duration_seconds",
        ),
    ];

    for (v4_name, v5_name) in &v4_to_v5_renames {
        assert!(
            !output.contains(v4_name),
            "old v4 metric name still present: {v4_name}"
        );
        assert!(
            output.contains(v5_name),
            "v5 metric name missing: {v5_name}"
        );
    }
}
