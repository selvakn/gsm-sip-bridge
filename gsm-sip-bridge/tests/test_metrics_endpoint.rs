mod common;

use gsm_sip_bridge::metrics;
use std::sync::Once;

static INIT: Once = Once::new();

fn init_all_metrics() {
    INIT.call_once(|| {});
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
    // VoWiFi-specific health (specs/014-vowifi-metrics-restore) — never
    // touched by the circuit-switched path, so these must be exercisable
    // (and absent from the disabled-path assertions below) independently.
    metrics::VOWIFI_REGISTERED
        .with_label_values(&["test"])
        .set(0.0);
    metrics::VOWIFI_REGISTRATIONS_TOTAL
        .with_label_values(&["test", "success"])
        .inc();
    metrics::VOWIFI_TUNNEL_UP
        .with_label_values(&["test"])
        .set(0.0);
    metrics::VOWIFI_BRIDGE_FAILURES_TOTAL
        .with_label_values(&["test", "ring_timeout"])
        .inc();
    metrics::AGENT_UP
        .with_label_values(&["ims", "test"])
        .set(0.0);
    metrics::AGENT_LAST_REPORT_SECONDS
        .with_label_values(&["ims", "test"])
        .set(0.0);
    metrics::OBSERVABILITY_EVENTS_DROPPED_TOTAL
        .with_label_values(&["ims", "test"])
        .inc_by(0.0);
}

#[test]
fn test_build_info_metric_has_labels() {
    // Deliberately does not reset() BUILD_INFO before setting this test's own
    // label combination: other tests in this file run concurrently against
    // the same global registry and may be reading/setting it at the same
    // time, so a reset() here would be a race that intermittently wipes
    // their series out from under them.
    init_all_metrics();
    metrics::BUILD_INFO
        .with_label_values(&["5.0.0", "abc1234", "2.16", "1.80.0"])
        .set(1.0);

    let encoder = prometheus::TextEncoder::new();
    let families = prometheus::gather();
    let output = encoder.encode_to_string(&families).unwrap();

    assert!(output.contains("gsm_sip_bridge_build_info"));
    assert!(output.contains("version=\"5.0.0\""));
    assert!(output.contains("git_sha=\"abc1234\""));
}

#[test]
fn test_all_metrics_registered() {
    init_all_metrics();

    let encoder = prometheus::TextEncoder::new();
    let families = prometheus::gather();
    let output = encoder.encode_to_string(&families).unwrap();

    let expected_metrics = [
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
        "gsm_sip_bridge_build_info",
        "gsm_sip_bridge_vowifi_registered",
        "gsm_sip_bridge_vowifi_registrations_total",
        "gsm_sip_bridge_vowifi_tunnel_up",
        "gsm_sip_bridge_vowifi_bridge_failures_total",
        "gsm_sip_bridge_agent_up",
        "gsm_sip_bridge_agent_last_report_seconds",
        "gsm_sip_bridge_observability_events_dropped_total",
    ];

    for metric in &expected_metrics {
        assert!(
            output.contains(metric),
            "missing metric: {metric}\n\nActual output:\n{output}"
        );
    }
}

/// Amended SC-006 / FR-022 (specs/014-vowifi-metrics-restore): with no
/// VoWiFi traffic, the six call/SMS metrics circuit-switched calls already
/// used must report the *same values* they always did — the only change
/// permitted is the added `transport="cs"` dimension on every series. A
/// dashboard panel querying these metrics without constraining `transport`
/// sees identical numbers to the pre-change build.
#[test]
fn test_cs_only_traffic_carries_cs_transport_and_no_vowifi_series() {
    init_all_metrics();

    let encoder = prometheus::TextEncoder::new();
    let families = prometheus::gather();
    let output = encoder.encode_to_string(&families).unwrap();

    for line in output.lines() {
        if line.starts_with("gsm_sip_bridge_calls_total{")
            || line.starts_with("gsm_sip_bridge_sip_calls_total{")
            || line.starts_with("gsm_sip_bridge_call_duration_seconds_sum{")
            || line.starts_with("gsm_sip_bridge_active_calls{")
            || line.starts_with("gsm_sip_bridge_sms_received_total{")
            || line.starts_with("gsm_sip_bridge_sms_forwarded_total{")
        {
            assert!(
                line.contains("transport=\"cs\"") && line.contains("module=\"test\""),
                "circuit-switched-only series must carry transport=\"cs\": {line}"
            );
            assert!(
                !line.contains("transport=\"vowifi\""),
                "no vowifi series should exist when nothing reported VoWiFi traffic: {line}"
            );
        }
    }
}
