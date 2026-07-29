//! Resolving `env:VAR_NAME` indirection, before deserialisation.
//!
//! Any string value in `config.toml` may be written `env:SOME_VAR`, meaning
//! "read this from the process environment instead" — the mechanism that
//! keeps secrets out of the file operators commit.
//!
//! This runs as a pass over the parsed [`toml::Value`] *before* it is handed
//! to serde, rather than as a custom `Deserialize` on each field. That is what
//! makes the serde migration tractable at all: the indirection applies to
//! every string field in every section, so doing it per-field would mean
//! wrapping ~40 fields in a newtype and would silently miss any field added
//! later. One pass over the document cannot miss anything.

use crate::error::{BridgeError, BridgeResult};
use toml::Value;

/// Key paths whose value is a secret, for error-message purposes only.
///
/// The distinction matters because the failure is reported to an operator
/// reading container logs: naming an unset *secret* variable tells them to
/// check their `.env`/orchestrator secret store, while naming an ordinary one
/// points at plain configuration. The value itself is never logged either way.
const SECRET_KEY_PATHS: &[&str] = &[
    "sip.password",
    "sms.discord_webhook_url",
    "alerts.discord_webhook_url",
    "alerts.sms.discord_webhook_url",
    "alerts.module_lifecycle.discord_webhook_url",
    "alerts.registration_loss.discord_webhook_url",
    "alerts.tunnel_failure.discord_webhook_url",
    "alerts.missed_call.discord_webhook_url",
];

fn is_secret(path: &str) -> bool {
    SECRET_KEY_PATHS.contains(&path)
}

/// Rewrites every `env:VAR` string in `value` to the variable's contents.
///
/// `path` accumulates the dotted key path (`sip.password`) so a failure names
/// the setting that referenced the variable, not just the variable.
pub fn resolve_in_place(value: &mut Value, path: &str) -> BridgeResult<()> {
    match value {
        Value::String(s) => {
            if let Some(resolved) = resolve_one(s, path)? {
                *s = resolved;
            }
        }
        Value::Table(t) => {
            for (k, v) in t.iter_mut() {
                let child = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                resolve_in_place(v, &child)?;
            }
        }
        Value::Array(a) => {
            for v in a.iter_mut() {
                // Array elements keep the array's own path: `[[vowifi.line]]`
                // entries are all `vowifi.line`, and an index would not help
                // an operator find the offending entry any faster than the
                // variable name does.
                resolve_in_place(v, path)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// `Some(resolved)` when `raw` was an `env:` reference, `None` when it is a
/// literal to be left alone.
fn resolve_one(raw: &str, key: &str) -> BridgeResult<Option<String>> {
    let Some(var_name) = raw.strip_prefix("env:") else {
        return Ok(None);
    };
    if var_name.is_empty() {
        return Err(BridgeError::Config(format!(
            "{key}: env: reference is missing variable name"
        )));
    }
    match std::env::var(var_name) {
        Ok(value) if !value.is_empty() => Ok(Some(value)),
        // An empty variable is treated as unset on purpose: an exported-but-
        // blank `SIP_PASSWORD` is far more likely to be a broken deployment
        // than an intentionally empty password, and silently registering with
        // one produces a confusing 401 loop instead of a clear startup error.
        _ => {
            let label = if is_secret(key) {
                "secret variable"
            } else {
                "environment variable"
            };
            Err(BridgeError::Config(format!(
                "{label} {var_name} is unset or empty (referenced from {key})"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(toml_src: &str) -> BridgeResult<Value> {
        let mut v: Value = toml_src.parse().unwrap();
        resolve_in_place(&mut v, "")?;
        Ok(v)
    }

    #[test]
    fn a_literal_string_is_left_alone() {
        let v = resolve(
            r#"[sip]
server = "pbx.example.com""#,
        )
        .unwrap();
        assert_eq!(v["sip"]["server"].as_str().unwrap(), "pbx.example.com");
    }

    #[test]
    fn an_env_reference_is_replaced_by_the_variables_contents() {
        std::env::set_var("GSB_ENV_TEST_OK", "s3cret");
        let v = resolve(
            r#"[sip]
password = "env:GSB_ENV_TEST_OK""#,
        )
        .unwrap();
        assert_eq!(v["sip"]["password"].as_str().unwrap(), "s3cret");
        std::env::remove_var("GSB_ENV_TEST_OK");
    }

    /// The pass reaches every string, at any depth — that is the whole reason
    /// it is a document pass rather than a per-field wrapper.
    #[test]
    fn nested_tables_and_arrays_are_resolved_too() {
        std::env::set_var("GSB_ENV_TEST_NESTED", "from-env");
        let v = resolve(
            r#"
[alerts.module_lifecycle]
discord_webhook_url = "env:GSB_ENV_TEST_NESTED"

[[vowifi.line]]
imsi_override = "env:GSB_ENV_TEST_NESTED"
"#,
        )
        .unwrap();

        assert_eq!(
            v["alerts"]["module_lifecycle"]["discord_webhook_url"]
                .as_str()
                .unwrap(),
            "from-env"
        );
        assert_eq!(
            v["vowifi"]["line"][0]["imsi_override"].as_str().unwrap(),
            "from-env"
        );
        std::env::remove_var("GSB_ENV_TEST_NESTED");
    }

    #[test]
    fn an_unset_variable_names_both_the_variable_and_the_setting() {
        std::env::remove_var("GSB_ENV_TEST_MISSING");
        let err = resolve(
            r#"[sip]
password = "env:GSB_ENV_TEST_MISSING""#,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("GSB_ENV_TEST_MISSING"), "{err}");
        assert!(err.contains("sip.password"), "{err}");
    }

    /// An exported-but-blank variable is a broken deployment far more often
    /// than an intentionally empty value, and registering with an empty
    /// password produces a confusing 401 loop rather than a clear failure.
    #[test]
    fn an_empty_variable_is_treated_as_unset() {
        std::env::set_var("GSB_ENV_TEST_EMPTY", "");
        assert!(resolve(
            r#"[sip]
password = "env:GSB_ENV_TEST_EMPTY""#
        )
        .is_err());
        std::env::remove_var("GSB_ENV_TEST_EMPTY");
    }

    #[test]
    fn a_bare_env_prefix_with_no_variable_name_is_rejected() {
        let err = resolve(
            r#"[sip]
password = "env:""#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("missing variable name"), "{err}");
    }

    /// Secrets and ordinary settings get different wording, so the operator
    /// knows whether to look in their secret store or their config.
    #[test]
    fn a_secret_and_a_plain_setting_are_described_differently() {
        std::env::remove_var("GSB_ENV_TEST_LABEL");

        let secret = resolve(
            r#"[sip]
password = "env:GSB_ENV_TEST_LABEL""#,
        )
        .unwrap_err()
        .to_string();
        let plain = resolve(
            r#"[vowifi]
epdg_fqdn = "env:GSB_ENV_TEST_LABEL""#,
        )
        .unwrap_err()
        .to_string();

        assert!(secret.contains("secret variable"), "{secret}");
        assert!(plain.contains("environment variable"), "{plain}");
    }

    /// A value that merely *contains* `env:` is not a reference — only a
    /// value that starts with it.
    #[test]
    fn only_a_leading_env_prefix_counts_as_a_reference() {
        let v = resolve(
            r#"[sip]
server = "sip.env:example.com""#,
        )
        .unwrap();
        assert_eq!(v["sip"]["server"].as_str().unwrap(), "sip.env:example.com");
    }
}
