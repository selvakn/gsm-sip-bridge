//! Keeps `config.toml.example` and `docs/configuration.md` honest about what
//! the parser actually accepts.
//!
//! This matters more than it used to. An unknown key is now a hard startup
//! error, so a key the parser accepts but nothing documents is one an operator
//! can only discover by reading `src/config/mod.rs` — and a key the docs
//! promise but the parser does not accept now *fails the container* rather
//! than warning.
//!
//! Same `include_str!`-and-assert pattern as `test_migration_guide.rs`, which
//! already does this for metric renames.

use gsm_sip_bridge::config::{
    ALERTS_KEYS, ALERTS_MODULE_LIFECYCLE_KEYS, ALERTS_REGISTRATION_LOSS_KEYS,
    ALERTS_TUNNEL_FAILURE_KEYS, AUDIO_KEYS, BRIDGE_KEYS, CONTROL_KEYS, LOGGING_KEYS, METRICS_KEYS,
    MODEM_AUDIO_KEYS, MODULES_KEYS, RESILIENCE_KEYS, SCHEDULED_RESTART_KEYS, SIP_KEYS, SMS_KEYS,
    TOP_LEVEL_SECTIONS, VOLTE_KEYS, VOLTE_LINE_KEYS, VOWIFI_KEYS, VOWIFI_LINE_KEYS,
};

const REFERENCE: &str = include_str!("../../docs/configuration.md");
const EXAMPLE: &str = include_str!("../../config.toml.example");

/// Every section/key list the parser enforces, with the name it is known by.
fn all_key_lists() -> Vec<(&'static str, &'static [&'static str])> {
    vec![
        ("sip", SIP_KEYS),
        ("bridge", BRIDGE_KEYS),
        ("sms", SMS_KEYS),
        ("metrics", METRICS_KEYS),
        ("modules", MODULES_KEYS),
        ("resilience", RESILIENCE_KEYS),
        ("control", CONTROL_KEYS),
        ("audio", AUDIO_KEYS),
        ("modem_audio", MODEM_AUDIO_KEYS),
        ("scheduled_restart", SCHEDULED_RESTART_KEYS),
        ("logging", LOGGING_KEYS),
        ("alerts", ALERTS_KEYS),
        ("alerts.module_lifecycle", ALERTS_MODULE_LIFECYCLE_KEYS),
        ("alerts.tunnel_failure", ALERTS_TUNNEL_FAILURE_KEYS),
        ("alerts.registration_loss", ALERTS_REGISTRATION_LOSS_KEYS),
        ("vowifi", VOWIFI_KEYS),
        ("vowifi.line", VOWIFI_LINE_KEYS),
        ("volte", VOLTE_KEYS),
        ("volte.line", VOLTE_LINE_KEYS),
    ]
}

/// Whether `key` names a nested table rather than a settable value —
/// `[[vowifi.line]]`, `[alerts.missed_call]`, and so on. Such a key appears in
/// its parent's list so the unknown-key check permits the nested table, but it
/// is documented as a sub-table heading, not as a value row.
///
/// Derived from the reference rather than hardcoded: the first version of this
/// was a hand-maintained list, and it was already missing `alerts.missed_call`
/// the moment it was written — which is precisely the failure mode this whole
/// file exists to prevent.
fn is_structural(section: &str, key: &str) -> bool {
    REFERENCE.contains(&format!("`[{section}.{key}]`"))
        || REFERENCE.contains(&format!("`[[{section}.{key}]]`"))
}

#[test]
fn every_accepted_key_is_documented_in_the_configuration_reference() {
    let mut undocumented = Vec::new();

    for (section, keys) in all_key_lists() {
        for key in keys {
            if is_structural(section, key) {
                continue;
            }
            // Matched as a backticked table entry (`| \`key\` | type | ...`),
            // not as a bare substring: keys like `port` and `enabled` appear
            // in ordinary prose all over the reference, so a substring check
            // would pass no matter what and the test would prove nothing.
            if !REFERENCE.contains(&format!("| `{key}`")) {
                undocumented.push(format!("[{section}] {key}"));
            }
        }
    }

    assert!(
        undocumented.is_empty(),
        "these keys are accepted by the parser but absent from \
         docs/configuration.md.\nAn undocumented key is one an operator can \
         only find by reading src/config/mod.rs:\n  {}",
        undocumented.join("\n  ")
    );
}

#[test]
fn every_top_level_section_is_documented() {
    let missing: Vec<&str> = TOP_LEVEL_SECTIONS
        .iter()
        .copied()
        .filter(|s| !REFERENCE.contains(&format!("[{s}]")))
        .collect();

    assert!(
        missing.is_empty(),
        "sections accepted but undocumented: {missing:?}"
    );
}

/// The example is what operators copy to start from, so a section missing
/// from it is one they will not know exists.
#[test]
fn every_top_level_section_appears_in_the_example_config() {
    let missing: Vec<&str> = TOP_LEVEL_SECTIONS
        .iter()
        .copied()
        .filter(|s| {
            // Sections may be shown commented out, which still tells the
            // reader they exist.
            !EXAMPLE.contains(&format!("[{s}]")) && !EXAMPLE.contains(&format!("# [{s}]"))
        })
        .collect();

    assert!(
        missing.is_empty(),
        "sections absent from config.toml.example: {missing:?}"
    );
}

/// The reverse direction: a key the example sets must be one the parser
/// accepts. Since an unknown key is now a hard error, an example that set a
/// stale key would fail every fresh deployment on first start.
///
/// `tests/test_config.rs::test_the_shipped_example_config_still_loads` proves
/// this end-to-end by actually loading it; this test localises the failure to
/// the offending key rather than a generic parse error.
#[test]
fn every_key_the_example_sets_is_one_the_parser_accepts() {
    let doc: toml::Value = EXAMPLE
        .parse()
        .expect("config.toml.example must be valid TOML");
    let table = doc.as_table().expect("example root must be a table");

    let mut unknown = Vec::new();
    for (section, value) in table {
        if !TOP_LEVEL_SECTIONS.contains(&section.as_str()) {
            unknown.push(section.clone());
            continue;
        }
        let Some(keys) = all_key_lists()
            .into_iter()
            .find(|(name, _)| name == section)
            .map(|(_, k)| k)
        else {
            continue;
        };
        if let Some(t) = value.as_table() {
            for key in t.keys() {
                if !keys.contains(&key.as_str()) {
                    unknown.push(format!("{section}.{key}"));
                }
            }
        }
    }

    assert!(
        unknown.is_empty(),
        "config.toml.example sets keys the parser rejects, which would fail \
         every fresh deployment on first start: {unknown:?}"
    );
}
