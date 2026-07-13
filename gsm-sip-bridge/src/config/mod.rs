pub mod secret;

use crate::error::{BridgeError, BridgeResult};
use secret::Secret;
use std::path::Path;
use toml::Value;

const TOP_LEVEL_SECTIONS: &[&str] = &[
    "sip",
    "bridge",
    "sms",
    "metrics",
    "modules",
    "resilience",
    "control",
    "audio",
    "scheduled_restart",
    "vowifi",
];
const SIP_KEYS: &[&str] = &[
    "server",
    "port",
    "username",
    "password",
    "transport",
    "local_port",
    "display_name",
    "tls_verify",
];
const BRIDGE_KEYS: &[&str] = &["sip_destination", "sip_dial_timeout_sec"];
const SMS_KEYS: &[&str] = &["enabled", "discord_webhook_url", "db_path"];
const METRICS_KEYS: &[&str] = &["port"];
const MODULES_KEYS: &[&str] = &["retry_interval_sec", "max_concurrent"];
const RESILIENCE_KEYS: &[&str] = &[
    "initial_backoff_sec",
    "max_backoff_sec",
    "max_retries",
    "network_loss_timeout_sec",
    "network_poll_interval_sec",
];
const CONTROL_KEYS: &[&str] = &["socket_path"];
const AUDIO_KEYS: &[&str] = &[
    "profile",
    "vad",
    "rx_gain",
    "tx_level",
    "eec_mode",
    "snd_rec_latency_ms",
    "snd_play_latency_ms",
    "rt_audio_prio",
];
const SCHEDULED_RESTART_KEYS: &[&str] = &[
    "enabled",
    "cron",
    "start_jitter_seconds",
    "inter_card_gap_seconds",
    "inter_card_gap_jitter_seconds",
];
const VOWIFI_KEYS: &[&str] = &[
    "enabled",
    "mcc",
    "mnc",
    "modem_port",
    "use_tcp",
    "sec_agree",
    "pcscf_source_path",
    "veth_local_addr",
    "veth_peer_addr",
    "control_port",
    "wideband",
    "apn",
    "netns",
    "epdg_fqdn",
    "epdg_ip",
    "src_addr",
    "keepalive_interval_sec",
    "veth_sip_iface",
    "veth_ims_iface",
    "tunnel_engine",
    "strongswan_tun_iface",
    "strongswan_if_id",
    "vpcd_host",
    "vpcd_port",
    "imsi_override",
];
const DEFAULT_SMS_DB_PATH: &str = "/var/lib/gsm-sip-bridge/store.db";
pub const DEFAULT_CONTROL_SOCKET: &str = "/tmp/gsm-sip-bridge.sock";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SipTransport {
    Udp,
    Tcp,
    Tls,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TlsVerify {
    Strict,
    Skip,
}

#[derive(Clone, Debug)]
pub struct SipConfig {
    pub server: String,
    pub port: u16,
    pub username: String,
    pub password: Secret<String>,
    pub transport: SipTransport,
    pub local_port: u16,
    pub display_name: String,
    pub tls_verify: TlsVerify,
}

#[derive(Clone, Debug)]
pub struct BridgeSection {
    pub sip_destination: String,
    pub sip_dial_timeout_sec: u64,
}

#[derive(Clone, Debug)]
pub struct SmsConfig {
    pub enabled: bool,
    pub discord_webhook_url: Secret<String>,
    pub db_path: String,
}

#[derive(Clone, Debug)]
pub struct MetricsConfig {
    pub port: u16,
}

#[derive(Clone, Debug)]
pub struct ModulesConfig {
    pub retry_interval_sec: u64,
    pub max_concurrent: u32,
}

#[derive(Clone, Debug)]
pub struct ResilienceConfig {
    pub initial_backoff_sec: u64,
    pub max_backoff_sec: u64,
    pub max_retries: u32,
    pub network_loss_timeout_sec: u64,
    pub network_poll_interval_sec: u64,
}

impl Default for ResilienceConfig {
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

#[derive(Clone, Debug)]
pub struct ControlConfig {
    pub socket_path: String,
}

/// Selects the audio latency preset.  `lan` targets same-machine / local-network SIP servers
/// where there is no packet jitter.  `wan` adds headroom for internet SIP trunks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioProfile {
    Lan,
    Wan,
}

/// The concrete numeric knobs derived from an `AudioProfile`.
#[derive(Clone, Debug)]
pub struct AudioProfileSettings {
    /// `ArrayQueue` depth for the capture and playback rings (frames of 20 ms each).
    pub ring_capacity: usize,
    /// PJMEDIA jitter-buffer initial pre-fill in milliseconds.
    pub jb_init_ms: i32,
    /// PJMEDIA jitter-buffer minimum pre-fetch frames.
    pub jb_min_pre: i32,
    /// PJMEDIA jitter-buffer hard ceiling in milliseconds.
    pub jb_max_ms: i32,
}

impl AudioProfileSettings {
    pub fn for_profile(profile: &AudioProfile) -> Self {
        match profile {
            AudioProfile::Lan => Self {
                ring_capacity: 4,
                jb_init_ms: 20,
                jb_min_pre: 1,
                jb_max_ms: 40,
            },
            AudioProfile::Wan => Self {
                ring_capacity: 16,
                jb_init_ms: 60,
                jb_min_pre: 2,
                jb_max_ms: 200,
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct AudioConfig {
    pub profile: AudioProfile,
    pub settings: AudioProfileSettings,
    /// When `true`, PJMEDIA VAD and noise suppression are active on the capture path.
    /// Disable only for diagnostics; leave enabled in production.
    pub vad: bool,
    /// EC20 downlink digital gain sent as `AT+QRXGAIN=<val>` during module init.
    /// Controls how loud SIP audio sounds on the GSM caller's end (SIP→GSM direction).
    /// `None` (default) leaves the modem's firmware default untouched.
    /// Range 0–65535; default varies by audio mode (typically ~32768).
    pub rx_gain: Option<u32>,
    /// EC20 echo-canceller mode word sent as `AT+QEEC=2,<val>` during module init.
    /// Controls which EC subsystems (AEC, DENS noise suppressor, NLPP) are active.
    /// `None` (default) leaves the modem's firmware default untouched.
    /// `Some(0)` disables all EC — recommended for USB audio bridges where there
    /// is no acoustic echo path and the EC only introduces noise artefacts.
    /// Range 0–65535.
    pub eec_mode: Option<u32>,
    /// PJSUA conference-bridge software gain applied to the capture→SIP path
    /// (`pjsua_conf_adjust_tx_level`).  1.0 = unity, <1.0 attenuates, >1.0 amplifies.
    /// Range 0.0–2.0, default 1.0.
    pub tx_level: f32,
    /// ALSA capture (GSM→SIP) ring-buffer depth in milliseconds, passed to PJMEDIA as
    /// `snd_rec_latency`. Larger values absorb scheduling jitter / XRUNs at the cost of
    /// added one-way latency. Range 20–2000; default 150 (PJSUA default is 100).
    pub snd_rec_latency_ms: u32,
    /// ALSA playback (SIP→GSM) ring-buffer depth in milliseconds, passed to PJMEDIA as
    /// `snd_play_latency`. Range 20–2000; default 150 (PJSUA default is 140).
    pub snd_play_latency_ms: u32,
    /// `SCHED_FIFO` priority to apply to PJMEDIA's `media` (sound-device) thread once a
    /// call's audio device is open. `0` (default) leaves it at `SCHED_OTHER`. Range 1–99;
    /// 10–30 is recommended. Requires `CAP_SYS_NICE` (privileged container); best-effort,
    /// failures are logged and never fatal.
    pub rt_audio_prio: u32,
}

/// Default ALSA capture latency (ms) — a modest bump over PJSUA's 100 ms to tolerate
/// containerized scheduling jitter without adding excessive one-way delay.
pub const DEFAULT_SND_REC_LATENCY_MS: u32 = 150;
/// Default ALSA playback latency (ms).
pub const DEFAULT_SND_PLAY_LATENCY_MS: u32 = 150;

impl Default for AudioConfig {
    fn default() -> Self {
        let profile = AudioProfile::Lan;
        let settings = AudioProfileSettings::for_profile(&profile);
        Self {
            profile,
            settings,
            vad: true,
            rx_gain: None,
            eec_mode: None,
            tx_level: 1.0,
            snd_rec_latency_ms: DEFAULT_SND_REC_LATENCY_MS,
            snd_play_latency_ms: DEFAULT_SND_PLAY_LATENCY_MS,
            rt_audio_prio: 0,
        }
    }
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            socket_path: DEFAULT_CONTROL_SOCKET.to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScheduledRestartConfig {
    pub enabled: bool,
    pub cron: String,
    pub start_jitter_seconds: u64,
    pub inter_card_gap_seconds: u64,
    pub inter_card_gap_jitter_seconds: u64,
}

impl Default for ScheduledRestartConfig {
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

impl ScheduledRestartConfig {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

/// Configuration for the inbound VoWiFi-to-SIP bridge (feature
/// `011-vowifi-sip-bridge`) — a second, independent inbound call path
/// alongside the existing circuit-switched GSM-to-SIP bridge. See
/// `specs/011-vowifi-sip-bridge/plan.md`. Disabled by default: this section
/// only matters when running one of the `vowifi-ims-agent`/`vowifi-sip-agent`
/// subcommands (started automatically by `docker/entrypoint.sh` when
/// enabled), not for the normal daemon path.
#[derive(Clone, Debug)]
pub struct VowifiConfig {
    /// Master switch — the two `vowifi-*-agent` subcommands refuse to start
    /// (see `main.rs`) when this is `false`, so an operator who hasn't
    /// provisioned VoWiFi can't accidentally bring the mode up.
    pub enabled: bool,
    /// Mobile Country Code of the home network, e.g. `"404"`.
    pub mcc: String,
    /// Mobile Network Code of the home network, e.g. `"094"` (Airtel).
    pub mnc: String,
    /// Serial AT port for the modem whose SIM authenticates the IMS
    /// registration (same device the existing `ims-register`/`ims-call`
    /// CLI tools use), e.g. `/dev/ttyUSB6`.
    pub modem_port: String,
    /// Use TCP (not UDP) for the SIP transport to the P-CSCF. `true` is the
    /// combination that reached `200 OK` on Airtel (see `ims::mod` docs).
    pub use_tcp: bool,
    /// Advertise `Require: sec-agree` / `Security-Client` and negotiate Gm
    /// IPsec. Required by networks (e.g. Vi) that reject a plain REGISTER;
    /// also the combination that worked on Airtel.
    pub sec_agree: bool,
    /// Path Agent A reads the tunnel-assigned P-CSCF address from —
    /// `docker/entrypoint.sh` writes this once the SWu tunnel is up.
    pub pcscf_source_path: String,
    /// Agent A's address on the dedicated veth link (the `ims`-netns end).
    pub veth_local_addr: String,
    /// Agent B's address on the dedicated veth link (the default-netns end).
    pub veth_peer_addr: String,
    /// TCP port the Agent A↔B control channel listens on/connects to over
    /// the veth link (`contracts/agent-control-protocol.md`).
    pub control_port: u16,
    /// Carry the carrier's wideband audio all the way to the PBX instead of
    /// narrowing it to 8 kHz at the first hop.
    ///
    /// With this on, Agent A prefers the carrier's AMR-WB (16 kHz) over its
    /// PCMU, hands it to Agent B as `L16/16000` over the veth link, and Agent B
    /// runs a 16 kHz PJMEDIA conference bridge offering G.722 to the PBX. With
    /// it off, every leg is 8 kHz — the behavior before wideband existed.
    ///
    /// Narrowband calls are unaffected either way: a carrier that offers only
    /// PCMU or AMR-NB (both 8 kHz) is answered and bridged exactly as before,
    /// with the veth link staying on PCMU. Turn this off only if the PBX
    /// mishandles a G.722 offer.
    pub wideband: bool,
    /// APN used by the `swu` engine's dialer (specs/011-vowifi-sip-bridge).
    pub apn: String,
    /// Network namespace the ePDG tunnel lives in — created by
    /// `docker/entrypoint.sh`, used by both engines.
    pub netns: String,
    /// ePDG FQDN, resolved to `epdg_ip` via DNS by `docker/entrypoint.sh`.
    /// Defaults to the 3GPP-standard derivation from `mcc`/`mnc` when not
    /// set explicitly in `[vowifi]`.
    pub epdg_fqdn: String,
    /// Skip DNS resolution and dial this ePDG IP directly. `None` (the
    /// default) means resolve `epdg_fqdn` at startup.
    pub epdg_ip: Option<String>,
    /// Force this as the tunnel's local source address instead of letting
    /// the kernel/charon pick one via routing to the ePDG. `None` (the
    /// default) means auto-select.
    pub src_addr: Option<String>,
    /// Idle-tunnel keepalive interval (seconds) — a TCP connect to the
    /// P-CSCF's SIP port, since operators commonly filter ICMP over the
    /// tunnel (confirmed on Vodafone India).
    pub keepalive_interval_sec: u64,
    /// Name of the veth interface end in the container's default netns
    /// (Agent B's side).
    pub veth_sip_iface: String,
    /// Name of the veth interface end inside `netns` (Agent A's side).
    pub veth_ims_iface: String,
    /// ePDG tunnel engine: `"strongswan"` (the default — proper IKE
    /// rekeying/re-auth/DPD/MOBIKE, netns survives reconnects) or `"swu"`
    /// (the original SWu-IKEv2 Python dialer, kept as an explicit fallback
    /// — see specs/012-strongswan-epdg).
    pub tunnel_engine: String,
    /// XFRM interface name the strongswan engine creates inside `netns`.
    pub strongswan_tun_iface: String,
    /// XFRM interface's `if_id`, pinned so it (and `netns`) survive
    /// reconnects (specs/012-strongswan-epdg FR-005/FR-011).
    pub strongswan_if_id: u32,
    /// Host running the vpcd virtual smart-card reader (pcscd's vpcd
    /// driver) that `vowifi-usim-bridge` connects to.
    pub vpcd_host: String,
    /// TCP port vpcd listens on.
    pub vpcd_port: u16,
    /// Use this IMSI instead of reading it from the SIM via `vowifi-imsi`
    /// (AT+CIMI) — a test/diagnostic escape hatch for the strongswan
    /// engine's swanctl connection rendering.
    pub imsi_override: Option<String>,
}

impl Default for VowifiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mcc: String::new(),
            mnc: String::new(),
            modem_port: "/dev/ttyUSB6".to_string(),
            use_tcp: true,
            sec_agree: true,
            pcscf_source_path: "/tmp/pcscf".to_string(),
            veth_local_addr: "10.99.0.1".to_string(),
            veth_peer_addr: "10.99.0.2".to_string(),
            control_port: 7050,
            wideband: true,
            apn: "ims".to_string(),
            netns: "ims".to_string(),
            epdg_fqdn: String::new(),
            epdg_ip: None,
            src_addr: None,
            keepalive_interval_sec: 20,
            veth_sip_iface: "veth-sip".to_string(),
            veth_ims_iface: "veth-ims".to_string(),
            tunnel_engine: "strongswan".to_string(),
            strongswan_tun_iface: "tun23".to_string(),
            strongswan_if_id: 23,
            vpcd_host: "127.0.0.1".to_string(),
            vpcd_port: 35963,
            imsi_override: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub sip: SipConfig,
    pub bridge: BridgeSection,
    pub sms: SmsConfig,
    pub metrics: MetricsConfig,
    pub modules: ModulesConfig,
    pub resilience: ResilienceConfig,
    pub control: ControlConfig,
    pub audio: AudioConfig,
    pub scheduled_restart: ScheduledRestartConfig,
    pub vowifi: VowifiConfig,
}

pub fn load_config(path: &Path) -> BridgeResult<AppConfig> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| BridgeError::Config(format!("config file {}: {e}", path.display())))?;

    let root: Value = contents.parse().map_err(BridgeError::from)?;
    let table = root
        .as_table()
        .ok_or_else(|| BridgeError::Config("config root must be a table".into()))?;

    warn_unknown_keys_in(table, TOP_LEVEL_SECTIONS, "root");
    let sip = parse_sip(table)?;
    let bridge = parse_bridge(table)?;
    let sms = parse_sms(table)?;
    let metrics = parse_metrics(table)?;
    let modules = parse_modules(table)?;
    let resilience = parse_resilience(table)?;
    let control = parse_control(table)?;
    let audio = parse_audio(table)?;
    let scheduled_restart = parse_scheduled_restart(table);
    let vowifi = parse_vowifi(table)?;

    Ok(AppConfig {
        sip,
        bridge,
        sms,
        metrics,
        modules,
        resilience,
        control,
        audio,
        scheduled_restart,
        vowifi,
    })
}

fn warn_unknown_keys_in(table: &toml::map::Map<String, Value>, allowed: &[&str], section: &str) {
    for key in table.keys() {
        if !allowed.contains(&key.as_str()) {
            tracing::warn!(section = section, key = %key, "unknown config key");
        }
    }
}

fn resolve_env_reference(raw: &str, config_key: &str, is_secret: bool) -> BridgeResult<String> {
    if let Some(var_name) = raw.strip_prefix("env:") {
        if var_name.is_empty() {
            return Err(BridgeError::Config(format!(
                "{config_key}: env: reference is missing variable name"
            )));
        }
        match std::env::var(var_name) {
            Ok(value) if !value.is_empty() => Ok(value),
            _ => {
                let label = if is_secret {
                    "secret variable"
                } else {
                    "environment variable"
                };
                Err(BridgeError::Config(format!(
                    "{label} {var_name} is unset or empty (referenced from {config_key})"
                )))
            }
        }
    } else {
        Ok(raw.to_string())
    }
}

fn as_string(v: &Value, key: &str, secret: bool) -> BridgeResult<String> {
    match v {
        Value::String(s) => resolve_env_reference(s, key, secret),
        _ => Err(BridgeError::Config(format!("field {key} must be a string"))),
    }
}

fn require_string(
    table: &toml::map::Map<String, Value>,
    field: &str,
    key: &str,
    secret: bool,
) -> BridgeResult<String> {
    let v = table
        .get(field)
        .ok_or_else(|| BridgeError::Config(format!("required field {key} is missing")))?;
    let s = as_string(v, key, secret)?;
    if s.is_empty() {
        return Err(BridgeError::Config(format!(
            "required field {key} is empty"
        )));
    }
    Ok(s)
}

fn as_u16_port(v: &Value, key: &str) -> BridgeResult<u16> {
    let n = as_u64_range(v, key, false, 1..=65535)?;
    Ok(n as u16)
}

fn as_u32(v: &Value, key: &str) -> BridgeResult<u32> {
    let n = as_u64_range(v, key, false, 0..=u32::MAX as u64)?;
    Ok(n as u32)
}

/// Like `as_string`, but an absent key or an empty/blank-after-`env:`
/// resolution value both mean "unset" (`None`) rather than an empty string —
/// used for `[vowifi]` fields whose absence means "auto-detect"
/// (`epdg_ip`, `src_addr`, `imsi_override`).
fn as_optional_string(
    t: &toml::map::Map<String, Value>,
    field: &str,
    key: &str,
) -> BridgeResult<Option<String>> {
    t.get(field)
        .map(|v| as_string(v, key, false))
        .transpose()
        .map(|opt| opt.filter(|s| !s.is_empty()))
}

fn as_u64_range(
    v: &Value,
    key: &str,
    secret: bool,
    range: std::ops::RangeInclusive<u64>,
) -> BridgeResult<u64> {
    let n = match v {
        Value::Integer(i) => {
            if *i < 0 {
                return Err(BridgeError::Config(format!(
                    "field {key} must not be negative"
                )));
            }
            *i as u64
        }
        Value::String(s) => {
            let resolved = resolve_env_reference(s, key, secret)?;
            resolved.parse::<u64>().map_err(|_| {
                BridgeError::Config(format!(
                    "field {key} must be an integer in {}..={}",
                    range.start(),
                    range.end()
                ))
            })?
        }
        _ => {
            return Err(BridgeError::Config(format!(
                "field {key} must be an integer"
            )))
        }
    };
    if !range.contains(&n) {
        return Err(BridgeError::Config(format!(
            "field {key} must be in {}..={}",
            range.start(),
            range.end()
        )));
    }
    Ok(n)
}

fn as_bool(v: &Value, key: &str) -> BridgeResult<bool> {
    match v {
        Value::Boolean(b) => Ok(*b),
        Value::String(s) => {
            let resolved = resolve_env_reference(s, key, false)?;
            match resolved.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => Ok(true),
                "false" | "0" | "no" => Ok(false),
                _ => Err(BridgeError::Config(format!(
                    "field {key} must be a boolean"
                ))),
            }
        }
        _ => Err(BridgeError::Config(format!(
            "field {key} must be a boolean"
        ))),
    }
}

fn as_integer(v: &Value, key: &str) -> BridgeResult<i64> {
    match v {
        Value::Integer(n) => Ok(*n),
        Value::String(s) => {
            let resolved = resolve_env_reference(s, key, false)?;
            resolved
                .parse::<i64>()
                .map_err(|_| BridgeError::Config(format!("field {key} must be an integer")))
        }
        _ => Err(BridgeError::Config(format!(
            "field {key} must be an integer"
        ))),
    }
}

fn as_float(v: &Value, key: &str) -> BridgeResult<f64> {
    match v {
        Value::Float(f) => Ok(*f),
        Value::Integer(n) => Ok(*n as f64),
        Value::String(s) => {
            let resolved = resolve_env_reference(s, key, false)?;
            resolved
                .parse::<f64>()
                .map_err(|_| BridgeError::Config(format!("field {key} must be a number")))
        }
        _ => Err(BridgeError::Config(format!("field {key} must be a number"))),
    }
}

fn parse_sip(root: &toml::map::Map<String, Value>) -> BridgeResult<SipConfig> {
    let sip = root
        .get("sip")
        .ok_or_else(|| BridgeError::Config("required section [sip] is missing".into()))?
        .as_table()
        .ok_or_else(|| BridgeError::Config("[sip] must be a table".into()))?;

    warn_unknown_keys_in(sip, SIP_KEYS, "sip");

    let server = require_string(sip, "server", "sip.server", false)?;
    let username = require_string(sip, "username", "sip.username", false)?;
    let password = Secret::new(require_string(sip, "password", "sip.password", true)?);

    let port = sip
        .get("port")
        .map(|v| as_u16_port(v, "sip.port"))
        .transpose()?
        .unwrap_or(5060);
    let local_port = sip
        .get("local_port")
        .map(|v| as_u16_port(v, "sip.local_port"))
        .transpose()?
        .unwrap_or(5060);

    let transport = match sip.get("transport") {
        Some(v) => match as_string(v, "sip.transport", false)?
            .to_ascii_lowercase()
            .as_str()
        {
            "udp" => SipTransport::Udp,
            "tcp" => SipTransport::Tcp,
            "tls" => SipTransport::Tls,
            other => {
                return Err(BridgeError::Config(format!(
                    "sip.transport must be udp, tcp, or tls; got {other}"
                )))
            }
        },
        None => SipTransport::Udp,
    };

    let (tls_verify, had_key) = match sip.get("tls_verify") {
        Some(v) => {
            let s = as_string(v, "sip.tls_verify", false)?;
            let tv = match s.to_ascii_lowercase().as_str() {
                "strict" => TlsVerify::Strict,
                "skip" => TlsVerify::Skip,
                other => {
                    return Err(BridgeError::Config(format!(
                        "sip.tls_verify must be strict or skip; got {other}"
                    )))
                }
            };
            (tv, true)
        }
        None => (TlsVerify::Strict, false),
    };

    if transport != SipTransport::Tls && had_key && tls_verify == TlsVerify::Skip {
        tracing::warn!("sip.tls_verify=skip has no effect when sip.transport is not tls");
    }

    let display_name = match sip.get("display_name") {
        Some(v) => {
            let s = as_string(v, "sip.display_name", false)?;
            if s.is_empty() {
                username.clone()
            } else {
                s
            }
        }
        None => username.clone(),
    };

    Ok(SipConfig {
        server,
        port,
        username,
        password,
        transport,
        local_port,
        display_name,
        tls_verify,
    })
}

fn parse_bridge(root: &toml::map::Map<String, Value>) -> BridgeResult<BridgeSection> {
    let Some(val) = root.get("bridge") else {
        return Ok(BridgeSection {
            sip_destination: String::new(),
            sip_dial_timeout_sec: 30,
        });
    };
    let t = val
        .as_table()
        .ok_or_else(|| BridgeError::Config("[bridge] must be a table".into()))?;
    warn_unknown_keys_in(t, BRIDGE_KEYS, "bridge");

    let sip_destination = t
        .get("sip_destination")
        .map(|v| as_string(v, "bridge.sip_destination", false))
        .transpose()?
        .unwrap_or_default();
    let sip_dial_timeout_sec = t
        .get("sip_dial_timeout_sec")
        .map(|v| as_u64_range(v, "bridge.sip_dial_timeout_sec", false, 5..=120))
        .transpose()?
        .unwrap_or(30);

    Ok(BridgeSection {
        sip_destination,
        sip_dial_timeout_sec,
    })
}

fn parse_sms(root: &toml::map::Map<String, Value>) -> BridgeResult<SmsConfig> {
    let Some(val) = root.get("sms") else {
        return Ok(SmsConfig {
            enabled: true,
            discord_webhook_url: Secret::new(String::new()),
            db_path: DEFAULT_SMS_DB_PATH.into(),
        });
    };
    let t = val
        .as_table()
        .ok_or_else(|| BridgeError::Config("[sms] must be a table".into()))?;
    warn_unknown_keys_in(t, SMS_KEYS, "sms");

    let enabled = t
        .get("enabled")
        .map(|v| as_bool(v, "sms.enabled"))
        .transpose()?
        .unwrap_or(true);
    let discord_webhook_url = match t.get("discord_webhook_url") {
        Some(v) => Secret::new(as_string(v, "sms.discord_webhook_url", true)?),
        None => Secret::new(String::new()),
    };
    let db_path = match t.get("db_path") {
        Some(v) => {
            let s = as_string(v, "sms.db_path", false)?;
            if s.is_empty() {
                DEFAULT_SMS_DB_PATH.into()
            } else {
                s
            }
        }
        None => DEFAULT_SMS_DB_PATH.into(),
    };

    Ok(SmsConfig {
        enabled,
        discord_webhook_url,
        db_path,
    })
}

fn parse_metrics(root: &toml::map::Map<String, Value>) -> BridgeResult<MetricsConfig> {
    let mut port = 9091u16;
    if let Some(val) = root.get("metrics") {
        let t = val
            .as_table()
            .ok_or_else(|| BridgeError::Config("[metrics] must be a table".into()))?;
        warn_unknown_keys_in(t, METRICS_KEYS, "metrics");
        if let Some(v) = t.get("port") {
            port = as_u16_port(v, "metrics.port")?;
        }
    }
    Ok(MetricsConfig { port })
}

fn parse_modules(root: &toml::map::Map<String, Value>) -> BridgeResult<ModulesConfig> {
    let Some(val) = root.get("modules") else {
        return Ok(ModulesConfig {
            retry_interval_sec: 30,
            max_concurrent: 8,
        });
    };
    let t = val
        .as_table()
        .ok_or_else(|| BridgeError::Config("[modules] must be a table".into()))?;
    warn_unknown_keys_in(t, MODULES_KEYS, "modules");

    let retry_interval_sec = t
        .get("retry_interval_sec")
        .map(|v| as_u64_range(v, "modules.retry_interval_sec", false, 5..=600))
        .transpose()?
        .unwrap_or(30);
    let max_concurrent = t
        .get("max_concurrent")
        .map(|v| as_u64_range(v, "modules.max_concurrent", false, 1..=8))
        .transpose()?
        .unwrap_or(8) as u32;

    Ok(ModulesConfig {
        retry_interval_sec,
        max_concurrent,
    })
}

fn parse_resilience(root: &toml::map::Map<String, Value>) -> BridgeResult<ResilienceConfig> {
    let Some(val) = root.get("resilience") else {
        return Ok(ResilienceConfig::default());
    };
    let t = val
        .as_table()
        .ok_or_else(|| BridgeError::Config("[resilience] must be a table".into()))?;
    warn_unknown_keys_in(t, RESILIENCE_KEYS, "resilience");

    let initial_backoff_sec = t
        .get("initial_backoff_sec")
        .map(|v| as_u64_range(v, "resilience.initial_backoff_sec", false, 1..=600))
        .transpose()?
        .unwrap_or(5);
    let max_backoff_sec = t
        .get("max_backoff_sec")
        .map(|v| as_u64_range(v, "resilience.max_backoff_sec", false, 1..=3600))
        .transpose()?
        .unwrap_or(120);
    let max_retries = t
        .get("max_retries")
        .map(|v| as_u64_range(v, "resilience.max_retries", false, 1..=1000))
        .transpose()?
        .unwrap_or(10) as u32;
    let network_loss_timeout_sec = t
        .get("network_loss_timeout_sec")
        .map(|v| as_u64_range(v, "resilience.network_loss_timeout_sec", false, 10..=600))
        .transpose()?
        .unwrap_or(60);
    let network_poll_interval_sec = t
        .get("network_poll_interval_sec")
        .map(|v| as_u64_range(v, "resilience.network_poll_interval_sec", false, 5..=300))
        .transpose()?
        .unwrap_or(30);

    Ok(ResilienceConfig {
        initial_backoff_sec,
        max_backoff_sec,
        max_retries,
        network_loss_timeout_sec,
        network_poll_interval_sec,
    })
}

fn parse_audio(root: &toml::map::Map<String, Value>) -> BridgeResult<AudioConfig> {
    let Some(val) = root.get("audio") else {
        return Ok(AudioConfig::default());
    };
    let t = val
        .as_table()
        .ok_or_else(|| BridgeError::Config("[audio] must be a table".into()))?;
    warn_unknown_keys_in(t, AUDIO_KEYS, "audio");

    let profile = match t.get("profile") {
        Some(v) => match as_string(v, "audio.profile", false)?
            .to_ascii_lowercase()
            .as_str()
        {
            "lan" => AudioProfile::Lan,
            "wan" => AudioProfile::Wan,
            other => {
                return Err(BridgeError::Config(format!(
                    "audio.profile must be \"lan\" or \"wan\"; got \"{other}\""
                )))
            }
        },
        None => AudioProfile::Lan,
    };

    let settings = AudioProfileSettings::for_profile(&profile);
    let vad = t
        .get("vad")
        .map(|v| as_bool(v, "audio.vad"))
        .transpose()?
        .unwrap_or(true);

    let rx_gain = match t.get("rx_gain") {
        Some(v) => {
            let n = as_integer(v, "audio.rx_gain")?;
            if !(0..=65535).contains(&n) {
                return Err(BridgeError::Config(format!(
                    "audio.rx_gain must be 0–65535; got {n}"
                )));
            }
            Some(n as u32)
        }
        None => None,
    };

    let tx_level = match t.get("tx_level") {
        Some(v) => {
            let f = as_float(v, "audio.tx_level")?;
            if !(0.0..=2.0).contains(&f) {
                return Err(BridgeError::Config(format!(
                    "audio.tx_level must be 0.0–2.0; got {f}"
                )));
            }
            f as f32
        }
        None => 1.0,
    };

    let eec_mode = match t.get("eec_mode") {
        Some(v) => {
            let n = as_integer(v, "audio.eec_mode")?;
            if !(0..=65535).contains(&n) {
                return Err(BridgeError::Config(format!(
                    "audio.eec_mode must be 0–65535; got {n}"
                )));
            }
            Some(n as u32)
        }
        None => None,
    };

    let snd_rec_latency_ms = parse_latency_ms(t, "snd_rec_latency_ms", DEFAULT_SND_REC_LATENCY_MS)?;
    let snd_play_latency_ms =
        parse_latency_ms(t, "snd_play_latency_ms", DEFAULT_SND_PLAY_LATENCY_MS)?;

    let rt_audio_prio = match t.get("rt_audio_prio") {
        Some(v) => {
            let n = as_integer(v, "audio.rt_audio_prio")?;
            // 0 disables; 1–99 are the valid SCHED_FIFO priorities.
            if n != 0 && !(1..=99).contains(&n) {
                return Err(BridgeError::Config(format!(
                    "audio.rt_audio_prio must be 0 (off) or 1–99; got {n}"
                )));
            }
            n as u32
        }
        None => 0,
    };

    Ok(AudioConfig {
        profile,
        settings,
        vad,
        rx_gain,
        tx_level,
        eec_mode,
        snd_rec_latency_ms,
        snd_play_latency_ms,
        rt_audio_prio,
    })
}

/// Parse an ALSA latency knob (milliseconds) from the `[audio]` table, validating the
/// 20–2000 ms range and falling back to `default` when the key is absent.
fn parse_latency_ms(
    t: &toml::map::Map<String, Value>,
    key: &str,
    default: u32,
) -> BridgeResult<u32> {
    match t.get(key) {
        Some(v) => {
            let n = as_integer(v, &format!("audio.{key}"))?;
            if !(20..=2000).contains(&n) {
                return Err(BridgeError::Config(format!(
                    "audio.{key} must be 20–2000 (ms); got {n}"
                )));
            }
            Ok(n as u32)
        }
        None => Ok(default),
    }
}

fn parse_scheduled_restart(root: &toml::map::Map<String, Value>) -> ScheduledRestartConfig {
    let defaults = ScheduledRestartConfig::default();

    let Some(val) = root.get("scheduled_restart") else {
        return defaults;
    };
    let Some(t) = val.as_table() else {
        tracing::error!(
            "[scheduled_restart] must be a table; scheduled restart disabled for this run"
        );
        return ScheduledRestartConfig::disabled();
    };
    warn_unknown_keys_in(t, SCHEDULED_RESTART_KEYS, "scheduled_restart");

    let enabled = match t.get("enabled") {
        None => defaults.enabled,
        Some(v) => match as_bool(v, "scheduled_restart.enabled") {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(error = %e, "scheduled restart disabled");
                return ScheduledRestartConfig::disabled();
            }
        },
    };

    let cron = match t.get("cron") {
        None => defaults.cron.clone(),
        Some(v) => match as_string(v, "scheduled_restart.cron", false) {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => {
                tracing::error!(
                    "scheduled_restart.cron is empty; scheduled restart disabled for this run"
                );
                return ScheduledRestartConfig::disabled();
            }
            Err(e) => {
                tracing::error!(error = %e, "scheduled restart disabled");
                return ScheduledRestartConfig::disabled();
            }
        },
    };

    let start_jitter_seconds = match t.get("start_jitter_seconds") {
        None => defaults.start_jitter_seconds,
        Some(v) => match as_u64_range(
            v,
            "scheduled_restart.start_jitter_seconds",
            false,
            0..=86400,
        ) {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(error = %e, "scheduled restart disabled");
                return ScheduledRestartConfig::disabled();
            }
        },
    };

    let inter_card_gap_seconds = match t.get("inter_card_gap_seconds") {
        None => defaults.inter_card_gap_seconds,
        Some(v) => match as_u64_range(
            v,
            "scheduled_restart.inter_card_gap_seconds",
            false,
            0..=3600,
        ) {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(error = %e, "scheduled restart disabled");
                return ScheduledRestartConfig::disabled();
            }
        },
    };

    let inter_card_gap_jitter_seconds = match t.get("inter_card_gap_jitter_seconds") {
        None => defaults.inter_card_gap_jitter_seconds,
        Some(v) => match as_u64_range(
            v,
            "scheduled_restart.inter_card_gap_jitter_seconds",
            false,
            0..=3600,
        ) {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(error = %e, "scheduled restart disabled");
                return ScheduledRestartConfig::disabled();
            }
        },
    };

    if inter_card_gap_jitter_seconds > inter_card_gap_seconds {
        tracing::error!(
            jitter = inter_card_gap_jitter_seconds,
            gap = inter_card_gap_seconds,
            "scheduled_restart.inter_card_gap_jitter_seconds must be <= inter_card_gap_seconds; scheduled restart disabled for this run"
        );
        return ScheduledRestartConfig::disabled();
    }

    // Validate cron expression: we use the cron crate's 7-field syntax; map our
    // 5-field input by prepending "0 " (seconds) and appending " *" (year).
    let translated = format!("0 {cron} *");
    if let Err(e) = translated.parse::<cron::Schedule>() {
        tracing::error!(
            cron = %cron,
            error = %e,
            "scheduled_restart.cron is not a valid 5-field cron expression; scheduled restart disabled for this run"
        );
        return ScheduledRestartConfig::disabled();
    }

    ScheduledRestartConfig {
        enabled,
        cron,
        start_jitter_seconds,
        inter_card_gap_seconds,
        inter_card_gap_jitter_seconds,
    }
}

fn parse_control(root: &toml::map::Map<String, Value>) -> BridgeResult<ControlConfig> {
    let Some(val) = root.get("control") else {
        return Ok(ControlConfig::default());
    };
    let t = val
        .as_table()
        .ok_or_else(|| BridgeError::Config("[control] must be a table".into()))?;
    warn_unknown_keys_in(t, CONTROL_KEYS, "control");

    let socket_path = t
        .get("socket_path")
        .map(|v| as_string(v, "control.socket_path", false))
        .transpose()?
        .unwrap_or_else(|| DEFAULT_CONTROL_SOCKET.to_string());

    Ok(ControlConfig { socket_path })
}

fn parse_vowifi(root: &toml::map::Map<String, Value>) -> BridgeResult<VowifiConfig> {
    let Some(val) = root.get("vowifi") else {
        return Ok(VowifiConfig::default());
    };
    let t = val
        .as_table()
        .ok_or_else(|| BridgeError::Config("[vowifi] must be a table".into()))?;
    warn_unknown_keys_in(t, VOWIFI_KEYS, "vowifi");

    let defaults = VowifiConfig::default();

    let enabled = t
        .get("enabled")
        .map(|v| as_bool(v, "vowifi.enabled"))
        .transpose()?
        .unwrap_or(defaults.enabled);
    let mcc = t
        .get("mcc")
        .map(|v| as_string(v, "vowifi.mcc", false))
        .transpose()?
        .unwrap_or(defaults.mcc);
    let mnc = t
        .get("mnc")
        .map(|v| as_string(v, "vowifi.mnc", false))
        .transpose()?
        .unwrap_or(defaults.mnc);

    if enabled && (mcc.is_empty() || mnc.is_empty()) {
        return Err(BridgeError::Config(
            "vowifi.mcc and vowifi.mnc are required when vowifi.enabled = true".into(),
        ));
    }

    let modem_port = t
        .get("modem_port")
        .map(|v| as_string(v, "vowifi.modem_port", false))
        .transpose()?
        .unwrap_or(defaults.modem_port);
    let use_tcp = t
        .get("use_tcp")
        .map(|v| as_bool(v, "vowifi.use_tcp"))
        .transpose()?
        .unwrap_or(defaults.use_tcp);
    let sec_agree = t
        .get("sec_agree")
        .map(|v| as_bool(v, "vowifi.sec_agree"))
        .transpose()?
        .unwrap_or(defaults.sec_agree);
    let pcscf_source_path = t
        .get("pcscf_source_path")
        .map(|v| as_string(v, "vowifi.pcscf_source_path", false))
        .transpose()?
        .unwrap_or(defaults.pcscf_source_path);
    let veth_local_addr = t
        .get("veth_local_addr")
        .map(|v| as_string(v, "vowifi.veth_local_addr", false))
        .transpose()?
        .unwrap_or(defaults.veth_local_addr);
    let veth_peer_addr = t
        .get("veth_peer_addr")
        .map(|v| as_string(v, "vowifi.veth_peer_addr", false))
        .transpose()?
        .unwrap_or(defaults.veth_peer_addr);
    let control_port = t
        .get("control_port")
        .map(|v| as_u16_port(v, "vowifi.control_port"))
        .transpose()?
        .unwrap_or(defaults.control_port);
    let wideband = t
        .get("wideband")
        .map(|v| as_bool(v, "vowifi.wideband"))
        .transpose()?
        .unwrap_or(defaults.wideband);

    let apn = t
        .get("apn")
        .map(|v| as_string(v, "vowifi.apn", false))
        .transpose()?
        .unwrap_or(defaults.apn);
    let netns = t
        .get("netns")
        .map(|v| as_string(v, "vowifi.netns", false))
        .transpose()?
        .unwrap_or(defaults.netns);
    let epdg_fqdn = t
        .get("epdg_fqdn")
        .map(|v| as_string(v, "vowifi.epdg_fqdn", false))
        .transpose()?
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("epdg.epc.mnc{mnc}.mcc{mcc}.pub.3gppnetwork.org"));
    let epdg_ip = as_optional_string(t, "epdg_ip", "vowifi.epdg_ip")?;
    let src_addr = as_optional_string(t, "src_addr", "vowifi.src_addr")?;
    let keepalive_interval_sec = t
        .get("keepalive_interval_sec")
        .map(|v| as_u64_range(v, "vowifi.keepalive_interval_sec", false, 1..=3600))
        .transpose()?
        .unwrap_or(defaults.keepalive_interval_sec);
    let veth_sip_iface = t
        .get("veth_sip_iface")
        .map(|v| as_string(v, "vowifi.veth_sip_iface", false))
        .transpose()?
        .unwrap_or(defaults.veth_sip_iface);
    let veth_ims_iface = t
        .get("veth_ims_iface")
        .map(|v| as_string(v, "vowifi.veth_ims_iface", false))
        .transpose()?
        .unwrap_or(defaults.veth_ims_iface);
    let tunnel_engine = t
        .get("tunnel_engine")
        .map(|v| as_string(v, "vowifi.tunnel_engine", false))
        .transpose()?
        .unwrap_or(defaults.tunnel_engine);
    if tunnel_engine != "swu" && tunnel_engine != "strongswan" {
        return Err(BridgeError::Config(format!(
            "vowifi.tunnel_engine must be \"swu\" or \"strongswan\", got {tunnel_engine:?}"
        )));
    }
    let strongswan_tun_iface = t
        .get("strongswan_tun_iface")
        .map(|v| as_string(v, "vowifi.strongswan_tun_iface", false))
        .transpose()?
        .unwrap_or(defaults.strongswan_tun_iface);
    let strongswan_if_id = t
        .get("strongswan_if_id")
        .map(|v| as_u32(v, "vowifi.strongswan_if_id"))
        .transpose()?
        .unwrap_or(defaults.strongswan_if_id);
    let vpcd_host = t
        .get("vpcd_host")
        .map(|v| as_string(v, "vowifi.vpcd_host", false))
        .transpose()?
        .unwrap_or(defaults.vpcd_host);
    let vpcd_port = t
        .get("vpcd_port")
        .map(|v| as_u16_port(v, "vowifi.vpcd_port"))
        .transpose()?
        .unwrap_or(defaults.vpcd_port);
    let imsi_override = as_optional_string(t, "imsi_override", "vowifi.imsi_override")?;

    Ok(VowifiConfig {
        enabled,
        mcc,
        mnc,
        modem_port,
        use_tcp,
        sec_agree,
        pcscf_source_path,
        veth_local_addr,
        veth_peer_addr,
        control_port,
        wideband,
        apn,
        netns,
        epdg_fqdn,
        epdg_ip,
        src_addr,
        keepalive_interval_sec,
        veth_sip_iface,
        veth_ims_iface,
        tunnel_engine,
        strongswan_tun_iface,
        strongswan_if_id,
        vpcd_host,
        vpcd_port,
        imsi_override,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> AppConfig {
        let root: toml::Value = toml.parse().unwrap();
        let table = root.as_table().unwrap();
        let sip = parse_sip(table).unwrap();
        let bridge = parse_bridge(table).unwrap();
        let sms = parse_sms(table).unwrap();
        let metrics = parse_metrics(table).unwrap();
        let modules = parse_modules(table).unwrap();
        let resilience = parse_resilience(table).unwrap();
        let control = parse_control(table).unwrap();
        let audio = parse_audio(table).unwrap();
        let scheduled_restart = parse_scheduled_restart(table);
        let vowifi = parse_vowifi(table).unwrap();
        AppConfig {
            sip,
            bridge,
            sms,
            metrics,
            modules,
            resilience,
            control,
            audio,
            scheduled_restart,
            vowifi,
        }
    }

    const MINIMAL_TOML: &str = r#"
[sip]
server = "sip.example.com"
username = "user"
password = "pass"
"#;

    #[test]
    fn resilience_defaults_when_section_absent() {
        let cfg = parse(MINIMAL_TOML);
        assert_eq!(cfg.resilience.initial_backoff_sec, 5);
        assert_eq!(cfg.resilience.max_backoff_sec, 120);
        assert_eq!(cfg.resilience.max_retries, 10);
        assert_eq!(cfg.resilience.network_loss_timeout_sec, 60);
        assert_eq!(cfg.resilience.network_poll_interval_sec, 30);
    }

    #[test]
    fn resilience_overrides_applied() {
        let toml = format!(
            "{}\n[resilience]\ninitial_backoff_sec = 10\nmax_retries = 3\n",
            MINIMAL_TOML
        );
        let cfg = parse(&toml);
        assert_eq!(cfg.resilience.initial_backoff_sec, 10);
        assert_eq!(cfg.resilience.max_retries, 3);
        assert_eq!(cfg.resilience.max_backoff_sec, 120); // default preserved
    }

    #[test]
    fn control_default_socket_path() {
        let cfg = parse(MINIMAL_TOML);
        assert_eq!(cfg.control.socket_path, "/tmp/gsm-sip-bridge.sock");
    }

    #[test]
    fn control_custom_socket_path() {
        let toml = format!(
            "{}\n[control]\nsocket_path = \"/run/gsm/ctrl.sock\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&toml);
        assert_eq!(cfg.control.socket_path, "/run/gsm/ctrl.sock");
    }

    #[test]
    fn audio_defaults_to_lan_when_section_absent() {
        let cfg = parse(MINIMAL_TOML);
        assert_eq!(cfg.audio.profile, AudioProfile::Lan);
        assert_eq!(cfg.audio.settings.ring_capacity, 4);
        assert_eq!(cfg.audio.settings.jb_init_ms, 20);
        assert_eq!(cfg.audio.settings.jb_min_pre, 1);
        assert_eq!(cfg.audio.settings.jb_max_ms, 40);
        assert!(cfg.audio.vad, "VAD must default to enabled");
    }

    #[test]
    fn audio_vad_can_be_disabled() {
        let toml = format!("{}\n[audio]\nvad = false\n", MINIMAL_TOML);
        let cfg = parse(&toml);
        assert!(!cfg.audio.vad);
    }

    #[test]
    fn audio_vad_defaults_true_when_key_absent() {
        let toml = format!("{}\n[audio]\nprofile = \"lan\"\n", MINIMAL_TOML);
        let cfg = parse(&toml);
        assert!(cfg.audio.vad);
    }

    #[test]
    fn audio_lan_profile_explicit() {
        let toml = format!("{}\n[audio]\nprofile = \"lan\"\n", MINIMAL_TOML);
        let cfg = parse(&toml);
        assert_eq!(cfg.audio.profile, AudioProfile::Lan);
        assert_eq!(cfg.audio.settings.ring_capacity, 4);
    }

    #[test]
    fn audio_wan_profile() {
        let toml = format!("{}\n[audio]\nprofile = \"wan\"\n", MINIMAL_TOML);
        let cfg = parse(&toml);
        assert_eq!(cfg.audio.profile, AudioProfile::Wan);
        assert_eq!(cfg.audio.settings.ring_capacity, 16);
        assert_eq!(cfg.audio.settings.jb_init_ms, 60);
        assert_eq!(cfg.audio.settings.jb_min_pre, 2);
        assert_eq!(cfg.audio.settings.jb_max_ms, 200);
    }

    #[test]
    fn scheduled_restart_defaults_when_section_absent() {
        let cfg = parse(MINIMAL_TOML);
        assert!(cfg.scheduled_restart.enabled);
        assert_eq!(cfg.scheduled_restart.cron, "0 1 * * *");
        assert_eq!(cfg.scheduled_restart.start_jitter_seconds, 600);
        assert_eq!(cfg.scheduled_restart.inter_card_gap_seconds, 30);
        assert_eq!(cfg.scheduled_restart.inter_card_gap_jitter_seconds, 15);
    }

    #[test]
    fn scheduled_restart_disabled_via_flag() {
        let toml = format!("{}\n[scheduled_restart]\nenabled = false\n", MINIMAL_TOML);
        let cfg = parse(&toml);
        assert!(!cfg.scheduled_restart.enabled);
    }

    #[test]
    fn scheduled_restart_custom_cron_applied() {
        let toml = format!(
            "{}\n[scheduled_restart]\ncron = \"30 2 * * 1-5\"\nstart_jitter_seconds = 0\n",
            MINIMAL_TOML
        );
        let cfg = parse(&toml);
        assert_eq!(cfg.scheduled_restart.cron, "30 2 * * 1-5");
        assert_eq!(cfg.scheduled_restart.start_jitter_seconds, 0);
        assert!(cfg.scheduled_restart.enabled);
    }

    #[test]
    fn scheduled_restart_invalid_cron_disables_feature() {
        let toml = format!(
            "{}\n[scheduled_restart]\ncron = \"0 25 * * *\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&toml);
        assert!(
            !cfg.scheduled_restart.enabled,
            "invalid cron must disable the feature"
        );
    }

    #[test]
    fn scheduled_restart_jitter_greater_than_gap_disables() {
        let toml = format!(
            "{}\n[scheduled_restart]\ninter_card_gap_seconds = 10\ninter_card_gap_jitter_seconds = 20\n",
            MINIMAL_TOML
        );
        let cfg = parse(&toml);
        assert!(!cfg.scheduled_restart.enabled);
    }

    #[test]
    fn scheduled_restart_jitter_out_of_range_disables() {
        let toml = format!(
            "{}\n[scheduled_restart]\nstart_jitter_seconds = 999999\n",
            MINIMAL_TOML
        );
        let cfg = parse(&toml);
        assert!(!cfg.scheduled_restart.enabled);
    }

    #[test]
    fn scheduled_restart_empty_cron_disables() {
        let toml = format!("{}\n[scheduled_restart]\ncron = \"\"\n", MINIMAL_TOML);
        let cfg = parse(&toml);
        assert!(!cfg.scheduled_restart.enabled);
    }

    #[test]
    fn audio_unknown_profile_returns_error() {
        let root: toml::Value = format!("{}\n[audio]\nprofile = \"fiber\"\n", MINIMAL_TOML)
            .parse()
            .unwrap();
        let table = root.as_table().unwrap();
        let result = parse_audio(table);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("audio.profile must be"));
    }

    #[test]
    fn audio_snd_latency_defaults_when_omitted() {
        let root: toml::Value = format!("{}\n[audio]\nprofile = \"lan\"\n", MINIMAL_TOML)
            .parse()
            .unwrap();
        let audio = parse_audio(root.as_table().unwrap()).unwrap();
        assert_eq!(audio.snd_rec_latency_ms, DEFAULT_SND_REC_LATENCY_MS);
        assert_eq!(audio.snd_play_latency_ms, DEFAULT_SND_PLAY_LATENCY_MS);
    }

    #[test]
    fn audio_snd_latency_custom_values_parsed() {
        let root: toml::Value = format!(
            "{}\n[audio]\nsnd_rec_latency_ms = 300\nsnd_play_latency_ms = 250\n",
            MINIMAL_TOML
        )
        .parse()
        .unwrap();
        let audio = parse_audio(root.as_table().unwrap()).unwrap();
        assert_eq!(audio.snd_rec_latency_ms, 300);
        assert_eq!(audio.snd_play_latency_ms, 250);
    }

    #[test]
    fn audio_snd_latency_out_of_range_returns_error() {
        let root: toml::Value = format!("{}\n[audio]\nsnd_rec_latency_ms = 5\n", MINIMAL_TOML)
            .parse()
            .unwrap();
        let result = parse_audio(root.as_table().unwrap());
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("audio.snd_rec_latency_ms must be 20–2000"));
    }

    #[test]
    fn audio_rt_audio_prio_defaults_off() {
        let root: toml::Value = format!("{}\n[audio]\nprofile = \"lan\"\n", MINIMAL_TOML)
            .parse()
            .unwrap();
        let audio = parse_audio(root.as_table().unwrap()).unwrap();
        assert_eq!(audio.rt_audio_prio, 0);
    }

    #[test]
    fn audio_rt_audio_prio_valid_value_parsed() {
        let root: toml::Value = format!("{}\n[audio]\nrt_audio_prio = 20\n", MINIMAL_TOML)
            .parse()
            .unwrap();
        let audio = parse_audio(root.as_table().unwrap()).unwrap();
        assert_eq!(audio.rt_audio_prio, 20);
    }

    #[test]
    fn audio_rt_audio_prio_out_of_range_returns_error() {
        let root: toml::Value = format!("{}\n[audio]\nrt_audio_prio = 150\n", MINIMAL_TOML)
            .parse()
            .unwrap();
        let result = parse_audio(root.as_table().unwrap());
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("audio.rt_audio_prio must be 0 (off) or 1–99"));
    }

    #[test]
    fn vowifi_disabled_by_default_when_section_absent() {
        let cfg = parse(MINIMAL_TOML);
        assert!(!cfg.vowifi.enabled);
        assert_eq!(cfg.vowifi.modem_port, "/dev/ttyUSB6");
        assert!(cfg.vowifi.use_tcp);
        assert!(cfg.vowifi.sec_agree);
        assert_eq!(cfg.vowifi.control_port, 7050);
    }

    #[test]
    fn vowifi_enabled_requires_mcc_and_mnc() {
        let toml = format!("{}\n[vowifi]\nenabled = true\n", MINIMAL_TOML);
        let root: toml::Value = toml.parse().unwrap();
        let result = parse_vowifi(root.as_table().unwrap());
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("vowifi.mcc and vowifi.mnc are required"));
    }

    #[test]
    fn vowifi_enabled_with_mcc_mnc_parses() {
        let toml = format!(
            "{}\n[vowifi]\nenabled = true\nmcc = \"404\"\nmnc = \"094\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&toml);
        assert!(cfg.vowifi.enabled);
        assert_eq!(cfg.vowifi.mcc, "404");
        assert_eq!(cfg.vowifi.mnc, "094");
    }

    #[test]
    fn vowifi_custom_veth_and_control_port() {
        let toml = format!(
            "{}\n[vowifi]\nveth_local_addr = \"10.1.1.1\"\nveth_peer_addr = \"10.1.1.2\"\ncontrol_port = 9999\n",
            MINIMAL_TOML
        );
        let cfg = parse(&toml);
        assert_eq!(cfg.vowifi.veth_local_addr, "10.1.1.1");
        assert_eq!(cfg.vowifi.veth_peer_addr, "10.1.1.2");
        assert_eq!(cfg.vowifi.control_port, 9999);
    }

    #[test]
    fn vowifi_tunnel_engine_defaults_to_strongswan() {
        let cfg = parse(MINIMAL_TOML);
        assert_eq!(cfg.vowifi.tunnel_engine, "strongswan");
        assert_eq!(cfg.vowifi.strongswan_tun_iface, "tun23");
        assert_eq!(cfg.vowifi.strongswan_if_id, 23);
        assert_eq!(cfg.vowifi.netns, "ims");
        assert_eq!(cfg.vowifi.apn, "ims");
        assert_eq!(cfg.vowifi.keepalive_interval_sec, 20);
        assert_eq!(cfg.vowifi.vpcd_host, "127.0.0.1");
        assert_eq!(cfg.vowifi.vpcd_port, 35963);
        assert_eq!(cfg.vowifi.epdg_ip, None);
        assert_eq!(cfg.vowifi.src_addr, None);
        assert_eq!(cfg.vowifi.imsi_override, None);
    }

    #[test]
    fn vowifi_tunnel_engine_rejects_unknown_value() {
        let toml = format!("{}\n[vowifi]\ntunnel_engine = \"bogus\"\n", MINIMAL_TOML);
        let root: toml::Value = toml.parse().unwrap();
        let result = parse_vowifi(root.as_table().unwrap());
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("vowifi.tunnel_engine must be"));
    }

    #[test]
    fn vowifi_epdg_fqdn_derived_from_mcc_mnc_when_unset() {
        let toml = format!(
            "{}\n[vowifi]\nenabled = true\nmcc = \"404\"\nmnc = \"094\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&toml);
        assert_eq!(
            cfg.vowifi.epdg_fqdn,
            "epdg.epc.mnc094.mcc404.pub.3gppnetwork.org"
        );
    }

    #[test]
    fn vowifi_epdg_fqdn_override_respected() {
        let toml = format!(
            "{}\n[vowifi]\nenabled = true\nmcc = \"404\"\nmnc = \"094\"\nepdg_fqdn = \"epdg.example.org\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&toml);
        assert_eq!(cfg.vowifi.epdg_fqdn, "epdg.example.org");
    }

    #[test]
    fn vowifi_optional_overrides_parsed() {
        let toml = format!(
            "{}\n[vowifi]\nepdg_ip = \"1.2.3.4\"\nsrc_addr = \"9.9.9.9\"\nimsi_override = \"404940123456789\"\ntunnel_engine = \"swu\"\n",
            MINIMAL_TOML
        );
        let cfg = parse(&toml);
        assert_eq!(cfg.vowifi.epdg_ip.as_deref(), Some("1.2.3.4"));
        assert_eq!(cfg.vowifi.src_addr.as_deref(), Some("9.9.9.9"));
        assert_eq!(cfg.vowifi.imsi_override.as_deref(), Some("404940123456789"));
        assert_eq!(cfg.vowifi.tunnel_engine, "swu");
    }
}
