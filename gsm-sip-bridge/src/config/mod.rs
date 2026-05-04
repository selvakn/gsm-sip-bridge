pub mod secret;

use crate::error::{BridgeError, BridgeResult};
use secret::Secret;
use std::path::Path;
use toml::Value;

const TOP_LEVEL_SECTIONS: &[&str] = &["sip", "bridge", "sms", "metrics", "modules"];
const SIP_KEYS: &[&str] = &[
    "server", "port", "username", "password", "transport", "local_port", "display_name",
    "tls_verify",
];
const BRIDGE_KEYS: &[&str] = &["sip_destination", "sip_dial_timeout_sec"];
const SMS_KEYS: &[&str] = &["enabled", "discord_webhook_url", "db_path"];
const METRICS_KEYS: &[&str] = &["port"];
const MODULES_KEYS: &[&str] = &["retry_interval_sec", "max_concurrent"];
const DEFAULT_SMS_DB_PATH: &str = "/var/lib/gsm-sip-bridge/store.db";

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
pub struct AppConfig {
    pub sip: SipConfig,
    pub bridge: BridgeSection,
    pub sms: SmsConfig,
    pub metrics: MetricsConfig,
    pub modules: ModulesConfig,
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

    Ok(AppConfig { sip, bridge, sms, metrics, modules })
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
                let label = if is_secret { "secret variable" } else { "environment variable" };
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
        return Err(BridgeError::Config(format!("required field {key} is empty")));
    }
    Ok(s)
}

fn as_u16_port(v: &Value, key: &str) -> BridgeResult<u16> {
    let n = as_u64_range(v, key, false, 1..=65535)?;
    Ok(n as u16)
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
                return Err(BridgeError::Config(format!("field {key} must not be negative")));
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
        _ => return Err(BridgeError::Config(format!("field {key} must be an integer"))),
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
                _ => Err(BridgeError::Config(format!("field {key} must be a boolean"))),
            }
        }
        _ => Err(BridgeError::Config(format!("field {key} must be a boolean"))),
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

    let port = sip.get("port").map(|v| as_u16_port(v, "sip.port")).transpose()?.unwrap_or(5060);
    let local_port = sip.get("local_port").map(|v| as_u16_port(v, "sip.local_port")).transpose()?.unwrap_or(5060);

    let transport = match sip.get("transport") {
        Some(v) => match as_string(v, "sip.transport", false)?.to_ascii_lowercase().as_str() {
            "udp" => SipTransport::Udp,
            "tcp" => SipTransport::Tcp,
            "tls" => SipTransport::Tls,
            other => return Err(BridgeError::Config(format!("sip.transport must be udp, tcp, or tls; got {other}"))),
        },
        None => SipTransport::Udp,
    };

    let (tls_verify, had_key) = match sip.get("tls_verify") {
        Some(v) => {
            let s = as_string(v, "sip.tls_verify", false)?;
            let tv = match s.to_ascii_lowercase().as_str() {
                "strict" => TlsVerify::Strict,
                "skip" => TlsVerify::Skip,
                other => return Err(BridgeError::Config(format!("sip.tls_verify must be strict or skip; got {other}"))),
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
            if s.is_empty() { username.clone() } else { s }
        }
        None => username.clone(),
    };

    Ok(SipConfig { server, port, username, password, transport, local_port, display_name, tls_verify })
}

fn parse_bridge(root: &toml::map::Map<String, Value>) -> BridgeResult<BridgeSection> {
    let Some(val) = root.get("bridge") else {
        return Ok(BridgeSection { sip_destination: String::new(), sip_dial_timeout_sec: 30 });
    };
    let t = val.as_table().ok_or_else(|| BridgeError::Config("[bridge] must be a table".into()))?;
    warn_unknown_keys_in(t, BRIDGE_KEYS, "bridge");

    let sip_destination = t.get("sip_destination").map(|v| as_string(v, "bridge.sip_destination", false)).transpose()?.unwrap_or_default();
    let sip_dial_timeout_sec = t.get("sip_dial_timeout_sec").map(|v| as_u64_range(v, "bridge.sip_dial_timeout_sec", false, 5..=120)).transpose()?.unwrap_or(30);

    Ok(BridgeSection { sip_destination, sip_dial_timeout_sec })
}

fn parse_sms(root: &toml::map::Map<String, Value>) -> BridgeResult<SmsConfig> {
    let Some(val) = root.get("sms") else {
        return Ok(SmsConfig {
            enabled: true,
            discord_webhook_url: Secret::new(String::new()),
            db_path: DEFAULT_SMS_DB_PATH.into(),
        });
    };
    let t = val.as_table().ok_or_else(|| BridgeError::Config("[sms] must be a table".into()))?;
    warn_unknown_keys_in(t, SMS_KEYS, "sms");

    let enabled = t.get("enabled").map(|v| as_bool(v, "sms.enabled")).transpose()?.unwrap_or(true);
    let discord_webhook_url = match t.get("discord_webhook_url") {
        Some(v) => Secret::new(as_string(v, "sms.discord_webhook_url", true)?),
        None => Secret::new(String::new()),
    };
    let db_path = match t.get("db_path") {
        Some(v) => {
            let s = as_string(v, "sms.db_path", false)?;
            if s.is_empty() { DEFAULT_SMS_DB_PATH.into() } else { s }
        }
        None => DEFAULT_SMS_DB_PATH.into(),
    };

    Ok(SmsConfig { enabled, discord_webhook_url, db_path })
}

fn parse_metrics(root: &toml::map::Map<String, Value>) -> BridgeResult<MetricsConfig> {
    let mut port = 9091u16;
    if let Some(val) = root.get("metrics") {
        let t = val.as_table().ok_or_else(|| BridgeError::Config("[metrics] must be a table".into()))?;
        warn_unknown_keys_in(t, METRICS_KEYS, "metrics");
        if let Some(v) = t.get("port") {
            port = as_u16_port(v, "metrics.port")?;
        }
    }
    if let Ok(s) = std::env::var("METRICS_PORT") {
        if !s.is_empty() {
            port = s.parse::<u16>().map_err(|_| {
                BridgeError::Config(format!("METRICS_PORT must be 1..=65535, got {s:?}"))
            })?;
            if port == 0 {
                return Err(BridgeError::Config("METRICS_PORT must be in 1..=65535".into()));
            }
        }
    }
    Ok(MetricsConfig { port })
}

fn parse_modules(root: &toml::map::Map<String, Value>) -> BridgeResult<ModulesConfig> {
    let Some(val) = root.get("modules") else {
        return Ok(ModulesConfig { retry_interval_sec: 30, max_concurrent: 8 });
    };
    let t = val.as_table().ok_or_else(|| BridgeError::Config("[modules] must be a table".into()))?;
    warn_unknown_keys_in(t, MODULES_KEYS, "modules");

    let retry_interval_sec = t.get("retry_interval_sec").map(|v| as_u64_range(v, "modules.retry_interval_sec", false, 5..=600)).transpose()?.unwrap_or(30);
    let max_concurrent = t.get("max_concurrent").map(|v| as_u64_range(v, "modules.max_concurrent", false, 1..=8)).transpose()?.unwrap_or(8) as u32;

    Ok(ModulesConfig { retry_interval_sec, max_concurrent })
}
