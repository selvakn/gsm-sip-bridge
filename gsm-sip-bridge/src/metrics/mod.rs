pub mod ingest;
pub mod server;

use once_cell::sync::Lazy;
use prometheus::{
    opts, register_counter_vec, register_gauge, register_gauge_vec, register_histogram_vec,
    CounterVec, Gauge, GaugeVec, HistogramVec, Opts, Registry,
};

pub static REGISTRY: Lazy<Registry> = Lazy::new(Registry::new);

/// Every call/SMS metric below carries a `transport` label (`"cs"` or
/// `"vowifi"`, see `store::Transport`) so circuit-switched and VoWiFi traffic
/// share one series family instead of needing separate metric names and
/// separate dashboard panels (specs/014-vowifi-metrics-restore).
pub static CALLS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!("gsm_sip_bridge_calls_total", "Total GSM calls observed"),
        &["module", "status", "transport"]
    )
    .unwrap()
});

pub static SIP_CALLS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!(
            "gsm_sip_bridge_sip_calls_total",
            "Outbound SIP calls per module"
        ),
        &["module", "status", "transport"]
    )
    .unwrap()
});

pub static CALL_DURATION_SECONDS: Lazy<HistogramVec> = Lazy::new(|| {
    register_histogram_vec!(
        "gsm_sip_bridge_call_duration_seconds",
        "Call duration in seconds",
        &["module", "transport"],
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
        &["module", "transport"]
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

/// 1 when the host-side VoLTE registration is currently accepted.
/// Deliberately separate from `SIP_REGISTERED` (the PBX-side registration) and
/// from the VoWiFi agent's gauge — an operator needs to see which of the three
/// is down, not an aggregate.
pub static VOLTE_REGISTERED: Lazy<Gauge> = Lazy::new(|| {
    let g = Gauge::new(
        "gsm_bridge_volte_registered",
        "1 when the host-side IMS registration over LTE is accepted, 0 otherwise",
    )
    .expect("metric");
    REGISTRY.register(Box::new(g.clone())).expect("register");
    g
});

/// 1 when the IMS PDN is attached and routable.
pub static VOLTE_PDN_UP: Lazy<Gauge> = Lazy::new(|| {
    let g = Gauge::new(
        "gsm_bridge_volte_pdn_up",
        "1 when the LTE IMS PDN is attached and has a default route, 0 otherwise",
    )
    .expect("metric");
    REGISTRY.register(Box::new(g.clone())).expect("register");
    g
});

/// Registration attempts by outcome, so a flapping renewal is visible as a
/// rate rather than only in logs.
pub static VOLTE_REGISTRATIONS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    let c = CounterVec::new(
        Opts::new(
            "gsm_bridge_volte_registrations_total",
            "Host-side VoLTE IMS registration attempts by outcome",
        ),
        &["outcome"],
    )
    .expect("metric");
    REGISTRY.register(Box::new(c.clone())).expect("register");
    c
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
        &["module", "transport"]
    )
    .unwrap()
});

pub static SMS_FORWARDED_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!(
            "gsm_sip_bridge_sms_forwarded_total",
            "Discord forwarding outcomes"
        ),
        &["module", "outcome", "transport"]
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

// --- VoWiFi-specific health (specs/013-multi-card-vowifi,
// specs/014-vowifi-metrics-restore) ------------------------------------------
// Labeled `module` — the same `derive_module_id`-derived card identity every
// other per-card metric above uses (specs/013's `card_id` is that same
// value; consolidated onto one label name here rather than introducing a
// second vocabulary for the same concept). Reported by Agent A
// (`ims::agent`) over the observability protocol (`metrics::ingest`,
// `observability::reporter`) rather than written directly in Agent A's own
// process — Agent A serves no scrape endpoint of its own, so a direct write
// there lands in a registry nothing reads.

/// 1 if this VoWiFi line's IMS-AKA registration is active, 0 otherwise.
pub static VOWIFI_REGISTERED: Lazy<GaugeVec> = Lazy::new(|| {
    register_gauge_vec!(
        opts!(
            "gsm_sip_bridge_vowifi_registered",
            "1 if this VoWiFi line's IMS registration is active, 0 otherwise"
        ),
        &["module"]
    )
    .unwrap()
});

/// 1 if this VoWiFi line's ePDG tunnel is up, 0 otherwise — a liveness
/// proxy (Agent A has a P-CSCF assignment and a live transport to it), not
/// raw IKE/ESP SA state (research.md §R6).
pub static VOWIFI_TUNNEL_UP: Lazy<GaugeVec> = Lazy::new(|| {
    register_gauge_vec!(
        opts!(
            "gsm_sip_bridge_vowifi_tunnel_up",
            "1 if this VoWiFi line's ePDG tunnel is up, 0 otherwise"
        ),
        &["module"]
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

pub static VOWIFI_REGISTRATIONS_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!(
            "gsm_sip_bridge_vowifi_registrations_total",
            "VoWiFi IMS registration attempts by outcome"
        ),
        &["module", "status"]
    )
    .unwrap()
});

pub static VOWIFI_BRIDGE_FAILURES_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!(
            "gsm_sip_bridge_vowifi_bridge_failures_total",
            "Inbound VoWiFi calls that failed to bridge, by reason"
        ),
        &["module", "reason"]
    )
    .unwrap()
});

// --- Agent liveness (specs/014-vowifi-metrics-restore) ---------------------
// Owned by `metrics::ingest`, evaluated at scrape time in `metrics::server`.
// Labeled by both `agent` (process kind: ims/sip) and `module` (card
// identity): specs/013-multi-card-vowifi means there can be several
// `vowifi-ims-agent` processes (one per line) and one `vowifi-sip-agent`
// process reporting on behalf of several lines, so a single process-kind
// label is no longer enough to identify *which* line's liveness a series
// describes.

pub static AGENT_UP: Lazy<GaugeVec> = Lazy::new(|| {
    register_gauge_vec!(
        opts!(
            "gsm_sip_bridge_agent_up",
            "1 if this agent/module has reported within the last 3 report intervals"
        ),
        &["agent", "module"]
    )
    .unwrap()
});

pub static AGENT_LAST_REPORT_SECONDS: Lazy<GaugeVec> = Lazy::new(|| {
    register_gauge_vec!(
        opts!(
            "gsm_sip_bridge_agent_last_report_seconds",
            "Age, in seconds, of this agent/module's most recent report"
        ),
        &["agent", "module"]
    )
    .unwrap()
});

pub static OBSERVABILITY_EVENTS_DROPPED_TOTAL: Lazy<CounterVec> = Lazy::new(|| {
    register_counter_vec!(
        opts!(
            "gsm_sip_bridge_observability_events_dropped_total",
            "Observability reports discarded by an agent's bounded buffer on overflow"
        ),
        &["agent", "module"]
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
