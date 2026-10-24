//! The Config page's document (specs/122-management-webui, research R11).
//!
//! Built from the TOML as parsed **before** `env:NAME` resolution, so a secret
//! supplied through the environment is only ever a reference here, never its
//! value. A literal secret written straight into the file is caught by key
//! name. Redaction by construction first, by pattern second: the resolved
//! `AppConfig` is never serialised for display.

use serde_json::{json, Map, Value};

const REDACTED: &str = "[REDACTED]";

/// Keys that contain `auth` but only name an algorithm.
const NOT_SECRET: &[&str] = &["gm_auth_alg"];

/// Whether a value stored under `key` must not be shown.
///
/// A substring match on the lowercased key, deliberately broad: showing too
/// little costs an operator a trip to the file, showing too much leaks a
/// password. `tests/test_webui_config.rs` checks that every `Secret` field in
/// the parser matches, so a new secret with an unlucky name fails the build.
pub fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    if NOT_SECRET.contains(&key.as_str()) {
        return false;
    }
    [
        "password",
        "secret",
        "token",
        "webhook",
        "api_key",
        "credential",
        "auth",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

/// `env:NAME` is shown as written. It is the whole point of the indirection:
/// the reference is safe to show and says where the value came from.
fn is_env_reference(value: &str) -> bool {
    value.starts_with("env:")
}

fn convert(key: Option<&str>, value: &toml::Value) -> Value {
    match value {
        toml::Value::String(s) => {
            if key.is_some_and(is_secret_key) && !is_env_reference(s) {
                json!(REDACTED)
            } else {
                json!(s)
            }
        }
        toml::Value::Integer(i) => json!(i),
        toml::Value::Float(f) => json!(f),
        toml::Value::Boolean(b) => json!(b),
        toml::Value::Datetime(d) => json!(d.to_string()),
        // An array belongs to the key above it, so `webhooks = ["a", "b"]`
        // is redacted element by element.
        toml::Value::Array(items) => Value::Array(items.iter().map(|v| convert(key, v)).collect()),
        toml::Value::Table(table) => Value::Object(
            table
                .iter()
                .map(|(k, v)| (k.clone(), convert(Some(k), v)))
                .collect::<Map<_, _>>(),
        ),
    }
}

/// The document as shown on the Config page.
pub fn redacted(document: &toml::Value) -> Value {
    convert(None, document)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(s: &str) -> toml::Value {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn secret_keys_are_matched_case_insensitively_by_substring() {
        for key in [
            "password",
            "discord_webhook_url",
            "API_KEY",
            "client_secret",
            "auth_token",
            "credentials",
            "SipPassword",
        ] {
            assert!(is_secret_key(key), "{key}");
        }
        for key in [
            "port",
            "enabled",
            "listen_addr",
            "gm_auth_alg",
            "gm_cipher_alg",
            "db_path",
        ] {
            assert!(!is_secret_key(key), "{key}");
        }
    }

    #[test]
    fn literal_secrets_are_redacted_and_env_references_are_kept() {
        let shown = redacted(&doc(r#"
            [sip]
            password = "hunter2"
            server = "pbx.example.com"
            [alerts]
            discord_webhook_url = "env:DISCORD_WEBHOOK_URL"
            "#));
        assert_eq!(shown["sip"]["password"], "[REDACTED]");
        assert_eq!(shown["sip"]["server"], "pbx.example.com");
        assert_eq!(
            shown["alerts"]["discord_webhook_url"],
            "env:DISCORD_WEBHOOK_URL"
        );
    }

    #[test]
    fn tables_inside_arrays_are_walked() {
        let shown = redacted(&doc(r#"
            [[sip_server.account]]
            username = "1001"
            password = "phone-secret"
            [[sip_server.account]]
            username = "1002"
            password = "env:PHONE_1002_PASSWORD"
            "#));
        let accounts = shown["sip_server"]["account"].as_array().unwrap();
        assert_eq!(accounts[0]["username"], "1001");
        assert_eq!(accounts[0]["password"], "[REDACTED]");
        assert_eq!(accounts[1]["password"], "env:PHONE_1002_PASSWORD");
    }

    #[test]
    fn scalars_keep_their_types() {
        let shown = redacted(&doc("[metrics]\nport = 9091\nratio = 0.5\nflag = true\n"));
        assert_eq!(shown["metrics"]["port"], 9091);
        assert_eq!(shown["metrics"]["ratio"], 0.5);
        assert_eq!(shown["metrics"]["flag"], true);
    }
}
