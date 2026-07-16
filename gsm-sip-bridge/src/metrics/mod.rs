pub mod server;

use once_cell::sync::Lazy;
use prometheus::{
    opts, register_counter_vec, register_gauge, register_gauge_vec, register_histogram_vec,
    CounterVec, Gauge, GaugeVec, HistogramVec, Registry,
};

pub static REGISTRY: Lazy<Registry> = Lazy::new(Registry::new);

pub static CALLS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!("gsm_sip_bridge_calls_total", "Total GSM calls observed"),
        &["module", "status"]
    )
    .unwrap()
});

pub static SIP_CALLS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!(
            "gsm_sip_bridge_sip_calls_total",
            "Outbound SIP calls per module"
        ),
        &["module", "status"]
    )
    .unwrap()
});

pub static CALL_DURATION_SECONDS: Lazy<HistogramVec> = Lazy::new(|| {
    register_histogram_vec!(
        "gsm_sip_bridge_call_duration_seconds",
        "Call duration in seconds",
        &["module"],
        vec![1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 1800.0]
    )
    .unwrap()
});

pub static ACTIVE_CALLS: Lazy<GaugeVec> = Lazy::new(|| {
    register_gauge_vec!(
        opts!(
            "gsm_sip_bridge_active_calls",
            "Currently active calls per module"
        ),
        &["module"]
    )
    .unwrap()
});

pub static SIP_REGISTRATIONS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!(
            "gsm_sip_bridge_sip_registrations_total",
            "SIP registration outcomes"
        ),
        &["status"]
    )
    .unwrap()
});

pub static SIP_REGISTERED: Lazy<Gauge> = Lazy::new(|| {
    register_gauge!(opts!(
        "gsm_sip_bridge_sip_registered",
        "1 if SIP registered, 0 otherwise"
    ))
    .unwrap()
});

pub static MODULE_INIT_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!("gsm_sip_bridge_module_init_total", "Module init attempts"),
        &["module", "status", "reason"]
    )
    .unwrap()
});

pub static MODULE_RETRIES_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!(
            "gsm_sip_bridge_module_retries_total",
            "Module retry attempts"
        ),
        &["module"]
    )
    .unwrap()
});

pub static MODULES_ACTIVE: Lazy<Gauge> = Lazy::new(|| {
    register_gauge!(opts!(
        "gsm_sip_bridge_modules_active",
        "Count of active modules"
    ))
    .unwrap()
});

pub static MODULES_FAILED: Lazy<Gauge> = Lazy::new(|| {
    register_gauge!(opts!(
        "gsm_sip_bridge_modules_failed",
        "Count of failed modules pending retry"
    ))
    .unwrap()
});

pub static AUDIO_ERRORS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!(
            "gsm_sip_bridge_audio_errors_total",
            "Audio errors per module"
        ),
        &["module", "kind"]
    )
    .unwrap()
});

pub static SMS_RECEIVED_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!(
            "gsm_sip_bridge_sms_received_total",
            "SMS messages read from SIM"
        ),
        &["module"]
    )
    .unwrap()
});

pub static SMS_FORWARDED_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!(
            "gsm_sip_bridge_sms_forwarded_total",
            "Discord forwarding outcomes"
        ),
        &["module", "outcome"]
    )
    .unwrap()
});

pub static SMS_DB_WRITES_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!(
            "gsm_sip_bridge_sms_db_writes_total",
            "SMS row write attempts"
        ),
        &["outcome"]
    )
    .unwrap()
});

pub static STORE_WRITES_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!(
            "gsm_sip_bridge_store_writes_total",
            "All writes to the store"
        ),
        &["table", "outcome"]
    )
    .unwrap()
});

pub static STORE_QUEUE_DEPTH: Lazy<Gauge> = Lazy::new(|| {
    register_gauge!(opts!(
        "gsm_sip_bridge_store_queue_depth",
        "Pending work items for the DB writer thread"
    ))
    .unwrap()
});

pub static UPTIME_SECONDS: Lazy<Gauge> = Lazy::new(|| {
    register_gauge!(opts!(
        "gsm_sip_bridge_uptime_seconds",
        "Seconds since process start"
    ))
    .unwrap()
});

pub static SCHEDULED_RESTART_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!(
            "gsm_sip_bridge_scheduled_restart_total",
            "Scheduled-restart attempts per slot and outcome"
        ),
        &["slot", "outcome"]
    )
    .unwrap()
});

/// 1 if this VoWiFi line's ePDG tunnel (CHILD_SA) is up, 0 otherwise —
/// labeled `card_id` (specs/013-multi-card-vowifi FR-017; no VoWiFi metric
/// existed at all before this feature, single-line or otherwise).
pub static VOWIFI_TUNNEL_UP: Lazy<GaugeVec> = Lazy::new(|| {
    register_gauge_vec!(
        opts!(
            "gsm_sip_bridge_vowifi_tunnel_up",
            "1 if this VoWiFi line's ePDG tunnel is up, 0 otherwise"
        ),
        &["card_id"]
    )
    .unwrap()
});

/// 1 if this VoWiFi line's IMS-AKA registration is active, 0 otherwise.
pub static VOWIFI_REGISTERED: Lazy<GaugeVec> = Lazy::new(|| {
    register_gauge_vec!(
        opts!(
            "gsm_sip_bridge_vowifi_registered",
            "1 if this VoWiFi line's IMS registration is active, 0 otherwise"
        ),
        &["card_id"]
    )
    .unwrap()
});

/// Bridged-call outcomes per VoWiFi line (FR-017's per-line call
/// attribution, mirrored in metrics form).
pub static VOWIFI_CALLS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!(
            "gsm_sip_bridge_vowifi_calls_total",
            "VoWiFi call outcomes per line"
        ),
        &["card_id", "outcome"]
    )
    .unwrap()
});

pub static BUILD_INFO: Lazy<GaugeVec> = Lazy::new(|| {
    register_gauge_vec!(
        opts!("gsm_sip_bridge_build_info", "Build metadata"),
        &["version", "git_sha", "pjsip_version", "rust_version"]
    )
    .unwrap()
});

pub fn register_build_info() {
    BUILD_INFO
        .with_label_values(&[
            env!("CARGO_PKG_VERSION"),
            option_env!("GIT_SHA").unwrap_or("unknown"),
            "2.16",
            env!("CARGO_PKG_RUST_VERSION").len().to_string().as_str(), // placeholder
        ])
        .set(1.0);
}
