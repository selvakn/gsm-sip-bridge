//! The `config.toml` document as serde sees it: one struct per section,
//! containing **exactly** that section's TOML keys and nothing else.
//!
//! # Why these are separate from the runtime types
//!
//! Several runtime config structs carry fields that are not settings at all.
//! `VowifiConfig` has `netns`, `veth_local_addr`, `strongswan_if_id`,
//! `vpcd_port` and more, every one of them *derived from a line's index* by
//! `line::resources` — the struct doubles as the per-line resolved view.
//! `AudioConfig` has `settings`, computed from `profile`.
//!
//! That is exactly why the parser could not simply grow
//! `#[serde(deny_unknown_fields)]`. `deny_unknown_fields` rejects keys absent
//! from the **struct**, so deriving it on `VowifiConfig` would have started
//! *accepting* `netns = "whatever"` in `[vowifi]` — letting an operator
//! hand-assign a namespace that every line then derives over, or worse, that
//! collides with another line's. Splitting the parsed shape from the runtime
//! shape is what makes the strictness safe.
//!
//! Each raw struct converts into its runtime counterpart via `From`, filling
//! derived fields with their placeholder defaults exactly as before — they are
//! overwritten per line by `vowifi::discovery::resolve_one_line` and
//! `volte::discovery::resolve_one_volte_line`.

use super::secret::Secret;
use serde::Deserialize;

/// Declares a config section: the serde struct **and** its key list, from one
/// definition.
///
/// Every section is `deny_unknown_fields` + `default` — an unrecognised key is
/// an error (a typo used to silently do nothing), and an omitted section or
/// key falls back to the documented default.
///
/// The generated `KEYS` is what `tests/test_config_docs.rs` checks the
/// reference and example against. Emitting it from the same definition as the
/// fields is the point: the previous design had a hand-maintained `*_KEYS`
/// const beside each struct, so adding a field meant remembering to add it in
/// two places, and forgetting made the key silently unsettable.
macro_rules! section {
    (
        $(#[$m:meta])*
        $vis:vis struct $name:ident {
            $(
                $(#[$fm:meta])*
                $fvis:vis $field:ident : $ty:ty
            ),* $(,)?
        }
    ) => {
        $(#[$m])*
        #[derive(Debug, Clone, Deserialize)]
        #[serde(deny_unknown_fields, default)]
        $vis struct $name {
            $(
                $(#[$fm])*
                $fvis $field : $ty,
            )*
        }

        impl $name {
            /// Every TOML key this section accepts, in declaration order.
            pub const KEYS: &'static [&'static str] = &[
                $( section!(@key $field $(#[$fm])*) ),*
            ];
        }
    };

    // Field names *are* TOML keys — no `#[serde(rename)]` anywhere, precisely
    // so this cannot disagree with what serde accepts.
    (@key $field:ident $(#[$fm:meta])*) => { stringify!($field) };
}

// ---------------------------------------------------------------- [sip] ----

section! {
    pub struct RawSip {
        pub server: String,
        pub port: u16,
        pub username: String,
        pub password: Secret<String>,
        pub transport: String,
        pub local_port: u16,
        /// Absent means "use `username`", which cannot be expressed as a
        /// `Default` because it depends on another field.
        pub display_name: Option<String>,
        pub tls_verify: String,
    }
}

impl Default for RawSip {
    fn default() -> Self {
        Self {
            server: String::new(),
            port: 5060,
            username: String::new(),
            password: Secret::new(String::new()),
            transport: "udp".to_string(),
            local_port: 5060,
            display_name: None,
            tls_verify: "strict".to_string(),
        }
    }
}

// ------------------------------------------------------------- [bridge] ----

section! {
    pub struct RawBridge {
        pub sip_destination: String,
        pub sip_dial_timeout_sec: u64,
    }
}

impl Default for RawBridge {
    fn default() -> Self {
        Self {
            sip_destination: String::new(),
            sip_dial_timeout_sec: 30,
        }
    }
}

// ---------------------------------------------------------------- [sms] ----

section! {
    pub struct RawSms {
        pub enabled: bool,
        pub discord_webhook_url: Secret<String>,
        pub db_path: String,
    }
}

impl Default for RawSms {
    fn default() -> Self {
        Self {
            enabled: true,
            discord_webhook_url: Secret::new(String::new()),
            db_path: super::DEFAULT_SMS_DB_PATH.to_string(),
        }
    }
}

// ------------------------------------------------------------ [metrics] ----

section! {
    pub struct RawMetrics {
        pub port: u16,
        pub agent_report_interval_seconds: u64,
    }
}

impl Default for RawMetrics {
    fn default() -> Self {
        Self {
            port: 9091,
            agent_report_interval_seconds: 15,
        }
    }
}

// ------------------------------------------------------------ [modules] ----

section! {
    pub struct RawModules {
        pub retry_interval_sec: u64,
        pub max_concurrent: u64,
    }
}

impl Default for RawModules {
    fn default() -> Self {
        Self {
            retry_interval_sec: 30,
            max_concurrent: 4,
        }
    }
}

// --------------------------------------------------------- [resilience] ----

section! {
    pub struct RawResilience {
        pub initial_backoff_sec: u64,
        pub max_backoff_sec: u64,
        pub max_retries: u64,
        pub network_loss_timeout_sec: u64,
        pub network_poll_interval_sec: u64,
    }
}

impl Default for RawResilience {
    fn default() -> Self {
        Self {
            initial_backoff_sec: 5,
            max_backoff_sec: 120,
            max_retries: 10,
            network_loss_timeout_sec: 60,
            network_poll_interval_sec: 30,
        }
    }
}

// ------------------------------------------------------------ [control] ----

section! {
    pub struct RawControl {
        pub socket_path: String,
    }
}

impl Default for RawControl {
    fn default() -> Self {
        Self {
            socket_path: super::DEFAULT_CONTROL_SOCKET.to_string(),
        }
    }
}

// ------------------------------------------------------------ [logging] ----

section! {
    pub struct RawLogging {
        pub level: String,
    }
}

impl Default for RawLogging {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

// -------------------------------------------------------------- [audio] ----

section! {
    /// `settings` is absent on purpose — it is computed from `profile` by
    /// `AudioProfileSettings::for_profile`, not configured.
    pub struct RawAudio {
        pub profile: String,
        pub vad: bool,
        pub snd_rec_latency_ms: u32,
        pub snd_play_latency_ms: u32,
    }
}

impl Default for RawAudio {
    fn default() -> Self {
        Self {
            profile: "lan".to_string(),
            vad: true,
            snd_rec_latency_ms: super::DEFAULT_SND_REC_LATENCY_MS,
            snd_play_latency_ms: super::DEFAULT_SND_PLAY_LATENCY_MS,
        }
    }
}

// -------------------------------------------------------- [modem_audio] ----

section! {
    pub struct RawModemAudio {
        pub rx_gain: Option<u32>,
        pub tx_level: f32,
        pub eec_mode: Option<u32>,
        pub rt_audio_prio: u32,
    }
}

impl Default for RawModemAudio {
    fn default() -> Self {
        Self {
            rx_gain: None,
            tx_level: 1.0,
            eec_mode: None,
            rt_audio_prio: 0,
        }
    }
}

// -------------------------------------------------- [scheduled_restart] ----

section! {
    pub struct RawScheduledRestart {
        pub enabled: bool,
        pub cron: String,
        pub start_jitter_seconds: u64,
        pub inter_card_gap_seconds: u64,
        pub inter_card_gap_jitter_seconds: u64,
    }
}

impl Default for RawScheduledRestart {
    fn default() -> Self {
        Self {
            enabled: true,
            cron: "0 1 * * *".to_string(),
            start_jitter_seconds: 600,
            inter_card_gap_seconds: 30,
            inter_card_gap_jitter_seconds: 15,
        }
    }
}

// ------------------------------------------------------------- [alerts] ----

section! {
    #[derive(Default)]
    pub struct RawAlertCategory {
        /// `None` distinguishes "not configured" from "explicitly false" —
        /// `[alerts.sms]` inherits `[sms].enabled` when unset.
        pub enabled: Option<bool>,
        pub discord_webhook_url: Option<Secret<String>>,
    }
}

section! {
    #[derive(Default)]
    pub struct RawModuleLifecycle {
        pub enabled: Option<bool>,
        pub discord_webhook_url: Option<Secret<String>>,
        pub at_worker_unresponsive_sec: Option<u64>,
    }
}

section! {
    #[derive(Default)]
    pub struct RawUnhealthyCategory {
        pub enabled: Option<bool>,
        pub discord_webhook_url: Option<Secret<String>>,
        pub unhealthy_sec: Option<u64>,
    }
}

section! {
    #[derive(Default)]
    pub struct RawAlerts {
        pub discord_webhook_url: Option<Secret<String>>,
        pub sms: Option<RawAlertCategory>,
        pub module_lifecycle: Option<RawModuleLifecycle>,
        pub registration_loss: Option<RawUnhealthyCategory>,
        pub tunnel_failure: Option<RawUnhealthyCategory>,
        pub missed_call: Option<RawAlertCategory>,
    }
}

// ------------------------------------------------------------- [vowifi] ----

section! {
    /// Exactly the `[vowifi]` keys. Every per-line-derived field of
    /// `VowifiConfig` (`netns`, `veth_*`, `strongswan_*`, `mcc`, `mnc`,
    /// `modem_port`, the `*_override`s, `pcsc_reader`) is deliberately absent:
    /// including one here would make it settable, which is the bug this split
    /// exists to prevent.
    pub struct RawVowifi {
        pub enabled: bool,
        pub use_tcp: bool,
        pub sec_agree: bool,
        pub pcscf_source_path: String,
        pub control_port: u16,
        pub wideband: bool,
        pub apn: String,
        pub epdg_fqdn: String,
        pub epdg_ip: Option<String>,
        pub src_addr: Option<String>,
        pub keepalive_interval_sec: u64,
        pub tunnel_engine: String,
        pub vpcd_host: String,
        pub vpcd_port: u16,
        pub max_lines: u32,
        /// `[[vowifi.line]]`. Named `line`, not `lines`, so the field name
        /// *is* the TOML key — a `#[serde(rename)]` would be invisible to the
        /// macro-generated `KEYS` (a captured `meta` fragment cannot be
        /// re-matched), silently putting the wrong name in the key list.
        pub line: Vec<RawVowifiLine>,
    }
}

impl Default for RawVowifi {
    fn default() -> Self {
        Self {
            enabled: false,
            use_tcp: true,
            sec_agree: true,
            pcscf_source_path: "/tmp/pcscf".to_string(),
            control_port: 7050,
            wideband: true,
            apn: "ims".to_string(),
            epdg_fqdn: String::new(),
            epdg_ip: None,
            src_addr: None,
            keepalive_interval_sec: 20,
            tunnel_engine: "strongswan".to_string(),
            vpcd_host: "127.0.0.1".to_string(),
            vpcd_port: 15963,
            max_lines: 8,
            line: Vec::new(),
        }
    }
}

section! {
    #[derive(Default)]
    pub struct RawVowifiLine {
        pub modem_serial: Option<String>,
        pub modem_port: Option<String>,
        pub mcc: Option<String>,
        pub mnc: Option<String>,
        pub imsi_override: Option<String>,
        pub imei_override: Option<String>,
        pub pcsc_reader: bool,
    }
}

// -------------------------------------------------------------- [volte] ----

section! {
    /// Exactly the `[volte]` keys — see [`RawVowifi`] on why the derived
    /// namespace/veth fields are absent.
    pub struct RawVolte {
        pub enabled: bool,
        pub pcscf_source_path: String,
        pub status_path: String,
        pub lock_path: String,
        pub bridge_inbound: bool,
        pub max_lines: u32,
        /// `[[volte.line]]` — see [`RawVowifi::line`] on the naming.
        pub line: Vec<RawVolteLine>,
    }
}

impl Default for RawVolte {
    fn default() -> Self {
        Self {
            enabled: false,
            pcscf_source_path: "/tmp/pcscf-0".to_string(),
            status_path: "/tmp/volte-registration-status".to_string(),
            lock_path: "/tmp/volte-registration.lock".to_string(),
            bridge_inbound: false,
            max_lines: 8,
            line: Vec::new(),
        }
    }
}

section! {
    #[derive(Default)]
    pub struct RawVolteLine {
        pub modem_serial: Option<String>,
        pub modem_port: Option<String>,
        pub cid: Option<u8>,
        pub apn: Option<String>,
        pub pcscf: Option<String>,
        pub iface: Option<String>,
        pub msisdn: Option<String>,
    }
}

// --------------------------------------------------------- [sip_server] ----

section! {
    /// Exactly the `[sip_server]` keys.
    ///
    /// The opt-in mode in which the bridge *is* the SIP server — IP phones
    /// REGISTER to it and it INVITEs the registered phone — instead of
    /// registering to an external PBX (spec 024). Off by default; several
    /// `[sip]` keys become errors when it is on, see `build::build_sip_server`.
    pub struct RawSipServer {
        pub enabled: bool,
        pub listen_addr: String,
        pub listen_port: u16,
        pub realm: String,
        /// Which account inbound calls ring. Exactly one; other accounts may
        /// register but are never called.
        pub ring_aor: String,
        pub min_expires: u32,
        pub max_expires: u32,
        pub nonce_lifetime_sec: u64,
        /// `[[sip_server.account]]` — singular because the field name *is* the
        /// TOML key, the same reason `RawVowifi::line` is not `lines`.
        pub account: Vec<RawSipServerAccount>,
    }
}

impl Default for RawSipServer {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_addr: "0.0.0.0".to_string(),
            listen_port: 5060,
            realm: "gsm-sip-bridge".to_string(),
            ring_aor: String::new(),
            min_expires: 60,
            max_expires: 3600,
            nonce_lifetime_sec: 120,
            account: Vec::new(),
        }
    }
}

section! {
    pub struct RawSipServerAccount {
        pub username: String,
        pub password: Secret<String>,
    }
}

// Hand-written rather than derived: `Secret<String>` deliberately has no
// `Default` impl, so an unset secret cannot be conjured silently.
impl Default for RawSipServerAccount {
    fn default() -> Self {
        Self {
            username: String::new(),
            password: Secret::new(String::new()),
        }
    }
}

// ------------------------------------------------- unknown-key checking ----

/// Every section's accepted keys, keyed by the section's TOML path.
///
/// Generated from the structs above, not hand-maintained — that is what the
/// `section!` macro's `KEYS` exists for.
pub fn section_key_lists() -> Vec<(&'static str, &'static [&'static str])> {
    vec![
        ("sip", RawSip::KEYS),
        ("bridge", RawBridge::KEYS),
        ("sms", RawSms::KEYS),
        ("metrics", RawMetrics::KEYS),
        ("modules", RawModules::KEYS),
        ("resilience", RawResilience::KEYS),
        ("control", RawControl::KEYS),
        ("audio", RawAudio::KEYS),
        ("modem_audio", RawModemAudio::KEYS),
        ("scheduled_restart", RawScheduledRestart::KEYS),
        ("logging", RawLogging::KEYS),
        ("alerts", RawAlerts::KEYS),
        ("alerts.sms", RawAlertCategory::KEYS),
        ("alerts.missed_call", RawAlertCategory::KEYS),
        ("alerts.module_lifecycle", RawModuleLifecycle::KEYS),
        ("alerts.registration_loss", RawUnhealthyCategory::KEYS),
        ("alerts.tunnel_failure", RawUnhealthyCategory::KEYS),
        ("vowifi", RawVowifi::KEYS),
        ("vowifi.line", RawVowifiLine::KEYS),
        ("volte", RawVolte::KEYS),
        ("volte.line", RawVolteLine::KEYS),
        ("sip_server", RawSipServer::KEYS),
        ("sip_server.account", RawSipServerAccount::KEYS),
    ]
}

/// Collects every unrecognised key in the document, section-qualified.
///
/// serde's own `deny_unknown_fields` would catch these, but it reports only
/// the *first* one and its message names neither the section nor the file — so
/// an operator with three typos needs three restarts to find them all, and
/// `max_lines` (a real key in two different sections) gives no clue which one
/// is wrong. This walk exists purely for the error message; serde still
/// enforces the same rule immediately afterwards.
pub fn collect_unknown_keys(root: &toml::Value) -> Vec<String> {
    let mut unknown = Vec::new();
    let Some(table) = root.as_table() else {
        return unknown;
    };
    let lists = section_key_lists();
    let top: Vec<&str> = lists
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| !n.contains('.'))
        .collect();

    for (section, value) in table {
        if !top.contains(&section.as_str()) {
            unknown.push(section.clone());
            continue;
        }
        check_section(section, value, &lists, &mut unknown);
    }
    unknown.sort();
    unknown
}

fn check_section(
    path: &str,
    value: &toml::Value,
    lists: &[(&'static str, &'static [&'static str])],
    unknown: &mut Vec<String>,
) {
    let Some(keys) = lists.iter().find(|(n, _)| *n == path).map(|(_, k)| *k) else {
        return;
    };
    let Some(table) = value.as_table() else {
        return;
    };
    for (key, child) in table {
        if !keys.contains(&key.as_str()) {
            unknown.push(format!("{path}.{key}"));
            continue;
        }
        // Recurse into nested tables and `[[array]]` entries that have their
        // own key list (`alerts.sms`, `vowifi.line`, ...).
        let child_path = format!("{path}.{key}");
        if lists.iter().any(|(n, _)| *n == child_path) {
            match child {
                toml::Value::Array(entries) => {
                    for e in entries {
                        check_section(&child_path, e, lists, unknown);
                    }
                }
                other => check_section(&child_path, other, lists, unknown),
            }
        }
    }
}

// --------------------------------------------------------------- root ------

section! {
    #[derive(Default)]
    pub struct RawConfig {
        pub sip: RawSip,
        pub bridge: RawBridge,
        pub sms: RawSms,
        pub metrics: RawMetrics,
        pub modules: RawModules,
        pub resilience: RawResilience,
        pub control: RawControl,
        pub audio: RawAudio,
        pub modem_audio: RawModemAudio,
        pub scheduled_restart: RawScheduledRestart,
        pub vowifi: RawVowifi,
        pub volte: RawVolte,
        pub logging: RawLogging,
        pub alerts: RawAlerts,
        pub sip_server: RawSipServer,
    }
}
