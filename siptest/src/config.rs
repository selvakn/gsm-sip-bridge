//! `siptest.toml` — deliberately standalone rather than reusing the bridge's
//! 4-module strict-parsing config machinery, which would be more indirection
//! than this small a schema justifies. Does reuse the bridge's `env:VAR`
//! secret convention (`gsm_sip_bridge::config::env::resolve_in_place`) so
//! passwords are never committed to the file.

use std::net::SocketAddr;
use std::path::PathBuf;

use gsm_sip_bridge::config::secret::Secret;
use serde::Deserialize;

use crate::error::{SipTestError, SipTestResult};
use crate::safety::SafetyPolicy;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub sip: SipConfig,
    #[serde(default)]
    pub media: MediaConfig,
    #[serde(default)]
    pub call: CallConfig,
    #[serde(default)]
    pub safety: SafetyPolicy,
    #[serde(default)]
    pub retention: RetentionConfig,
    #[serde(default)]
    pub inbound: InboundConfig,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SipConfig {
    pub bridge_host: String,
    #[serde(default = "default_registrar_port")]
    pub registrar_port: u16,
    /// The redirect port siptest *expects* to see in the bridge's `302`
    /// Contact. Used only to log a warning on mismatch — the port actually
    /// used always comes from the response itself (research.md R3).
    #[serde(default = "default_outbound_port")]
    pub outbound_port: u16,
    #[serde(default)]
    pub local_ip: Option<String>,
    #[serde(default = "default_local_port")]
    pub local_port: u16,
    pub username: String,
    pub password: Secret<String>,
    #[serde(default = "default_realm")]
    pub realm: String,
    #[serde(default = "default_register_expires")]
    pub register_expires_secs: u32,
}

fn default_registrar_port() -> u16 {
    5060
}
fn default_outbound_port() -> u16 {
    5072
}
fn default_local_port() -> u16 {
    5065
}
fn default_realm() -> String {
    "gsm-sip-bridge".to_string()
}
fn default_register_expires() -> u32 {
    300
}

impl SipConfig {
    pub fn registrar_addr(&self) -> SipTestResult<SocketAddr> {
        format!("{}:{}", self.bridge_host, self.registrar_port)
            .parse()
            .map_err(|e| SipTestError::Config(format!("invalid bridge_host/registrar_port: {e}")))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MediaConfig {
    #[serde(default = "default_codec")]
    pub codec: String,
    #[serde(default = "default_rtp_port_min")]
    pub rtp_port_min: u16,
    #[serde(default = "default_rtp_port_max")]
    pub rtp_port_max: u16,
    #[serde(default = "default_tone_plan")]
    pub tone_plan: String,
    #[serde(default = "default_recording_dir")]
    pub recording_dir: PathBuf,
    #[serde(default = "default_true")]
    pub record: bool,
}

fn default_codec() -> String {
    "auto".to_string()
}
fn default_rtp_port_min() -> u16 {
    40000
}
fn default_rtp_port_max() -> u16 {
    40100
}
fn default_tone_plan() -> String {
    "grid8".to_string()
}
fn default_recording_dir() -> PathBuf {
    PathBuf::from("/tmp/siptest")
}
fn default_true() -> bool {
    true
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            codec: default_codec(),
            rtp_port_min: default_rtp_port_min(),
            rtp_port_max: default_rtp_port_max(),
            tone_plan: default_tone_plan(),
            recording_dir: default_recording_dir(),
            record: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallConfig {
    #[serde(default = "default_duration")]
    pub default_duration_secs: u32,
    #[serde(default = "default_ring_timeout")]
    pub ring_timeout_secs: u32,
    #[serde(default = "default_require")]
    pub require: String,
}

fn default_duration() -> u32 {
    30
}
fn default_ring_timeout() -> u32 {
    40
}
fn default_require() -> String {
    "packets".to_string()
}

impl Default for CallConfig {
    fn default() -> Self {
        Self {
            default_duration_secs: default_duration(),
            ring_timeout_secs: default_ring_timeout(),
            require: default_require(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RetentionConfig {
    #[serde(default = "default_max_calls_retained")]
    pub max_calls_retained: usize,
}

fn default_max_calls_retained() -> usize {
    50
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            max_calls_retained: default_max_calls_retained(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct InboundConfig {
    #[serde(default = "default_inbound_mode")]
    pub mode: String,
    #[serde(default = "default_answer_delay")]
    pub answer_delay_ms: u32,
    #[serde(default = "default_reject_status")]
    pub reject_status: u16,
    #[serde(default = "default_duration")]
    pub duration_secs: u32,
}

fn default_inbound_mode() -> String {
    "answer".to_string()
}
fn default_answer_delay() -> u32 {
    2000
}
fn default_reject_status() -> u16 {
    486
}

impl Default for InboundConfig {
    fn default() -> Self {
        Self {
            mode: default_inbound_mode(),
            answer_delay_ms: default_answer_delay(),
            reject_status: default_reject_status(),
            duration_secs: default_duration(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiConfig {
    #[serde(default = "default_api_bind")]
    pub bind: String,
}

fn default_api_bind() -> String {
    "127.0.0.1:8099".to_string()
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            bind: default_api_bind(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
}

fn default_log_level() -> String {
    "info".to_string()
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

/// Reads `[logging].level` only — used before the full config load so
/// logging works even when the rest of the file fails to parse, mirroring
/// `gsm_sip_bridge::config::read_log_level`.
pub fn read_log_level(path: &std::path::Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return "info".to_string();
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        return "info".to_string();
    };
    value
        .get("logging")
        .and_then(|l| l.get("level"))
        .and_then(|v| v.as_str())
        .unwrap_or("info")
        .to_string()
}

pub fn load(path: &std::path::Path) -> SipTestResult<Config> {
    let text = std::fs::read_to_string(path)?;
    let mut value: toml::Value =
        toml::from_str(&text).map_err(|e: toml::de::Error| SipTestError::Config(e.to_string()))?;
    gsm_sip_bridge::config::env::resolve_in_place(&mut value, "")?;
    let config: Config = value
        .try_into()
        .map_err(|e: toml::de::Error| SipTestError::Config(e.to_string()))?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_password_resolves_and_is_redacted_in_debug_output() {
        std::env::set_var("SIPTEST_TEST_PASSWORD", "hunter2");
        let toml_text = r#"
[sip]
bridge_host = "192.168.15.10"
username = "1002"
password = "env:SIPTEST_TEST_PASSWORD"
"#;
        let mut value: toml::Value = toml::from_str(toml_text).unwrap();
        gsm_sip_bridge::config::env::resolve_in_place(&mut value, "").unwrap();
        let config: Config = value.try_into().unwrap();
        assert_eq!(config.sip.password.expose_secret(), "hunter2");
        assert_eq!(format!("{:?}", config.sip.password), "[REDACTED]");
        assert!(!format!("{config:?}").contains("hunter2"));
        std::env::remove_var("SIPTEST_TEST_PASSWORD");
    }

    #[test]
    fn defaults_fill_in_when_sections_are_absent() {
        let toml_text = r#"
[sip]
bridge_host = "192.168.15.10"
username = "1002"
password = "plain-for-test"
"#;
        let mut value: toml::Value = toml::from_str(toml_text).unwrap();
        gsm_sip_bridge::config::env::resolve_in_place(&mut value, "").unwrap();
        let config: Config = value.try_into().unwrap();
        assert_eq!(config.sip.registrar_port, 5060);
        assert_eq!(config.media.codec, "auto");
        assert_eq!(config.retention.max_calls_retained, 50);
        assert_eq!(config.api.bind, "127.0.0.1:8099");
    }
}
