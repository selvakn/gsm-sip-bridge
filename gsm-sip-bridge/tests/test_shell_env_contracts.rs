//! Regression tests for the `*-shell-env` printers.
//!
//! These render `KEY=value` lines meant to be `eval`'d by a shell. Both of
//! their original in-tree consumers are gone — `entrypoint.sh`'s
//! orchestration became `supervise` (specs/021) and `healthcheck.sh` became
//! `commands::healthcheck` — so they are now an operator diagnostic ("what
//! did this container actually resolve?") rather than an internal contract.
//!
//! They are still worth pinning. Anything `eval`'d is a shell-injection
//! surface, the array-length invariant below is the kind of thing that fails
//! silently, and an operator debugging a live line at 2am should not have to
//! wonder whether the output is trustworthy.
//!
//! They had no coverage at all until recently, for a purely structural
//! reason: the printers lived in `src/main.rs`, and a binary crate's items
//! cannot be imported from `tests/`. They now live in
//! `gsm_sip_bridge::commands`.

use gsm_sip_bridge::commands::{config as config_cmd, discover, volte};
use gsm_sip_bridge::config::{AppConfig, VolteConfig};
use gsm_sip_bridge::vowifi::discovery::{LineResolution, LineResolutionEntry};
use std::io::Write;
use tempfile::NamedTempFile;

/// Parses `KEY='value'` / `KEY=(...)` lines into a lookup, so assertions name
/// the variable rather than a line number.
fn vars(rendered: &str) -> std::collections::HashMap<String, String> {
    rendered
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn entry(index: u32, card_id: &str) -> LineResolutionEntry {
    LineResolutionEntry {
        index,
        card_id: card_id.to_string(),
        modem_port: format!("/dev/ttyUSB{index}"),
        netns: format!("ims{index}"),
        control_port: 7100 + index as u16,
        veth_local_addr: format!("10.9.{index}.1/30"),
        veth_peer_addr: format!("10.9.{index}.2/30"),
        vpcd_port: 15963 + index as u16,
        strongswan_if_id: 42 + index,
        strongswan_tun_iface: format!("ipsec-{index}"),
        pcscf_source_path: "/tmp/pcscf".to_string(),
        mcc: "404".to_string(),
        mnc: "043".to_string(),
        pcsc_reader: false,
        configured_identifier: None,
        msisdn: None,
        config: Default::default(),
    }
}

#[test]
fn discover_shell_env_emits_one_array_element_per_line_in_order() {
    let resolution = LineResolution {
        circuit_switched_excluded_ports: vec!["/dev/ttyUSB9".to_string()],
        lines: vec![entry(0, "ec20-AAAAAA"), entry(1, "ec20-BBBBBB")],
        failed: vec![],
    };

    let v = vars(&discover::render_discover_shell_env(&resolution));

    assert_eq!(v["LINE_COUNT"], "2");
    assert_eq!(v["LINE_CARD_ID"], "('ec20-AAAAAA' 'ec20-BBBBBB')");
    assert_eq!(v["LINE_MODEM_PORT"], "('/dev/ttyUSB0' '/dev/ttyUSB1')");
    assert_eq!(v["LINE_NETNS"], "('ims0' 'ims1')");
    assert_eq!(v["LINE_CONTROL_PORT"], "('7100' '7101')");
    assert_eq!(v["LINE_VPCD_PORT"], "('15963' '15964')");
    assert_eq!(v["LINE_STRONGSWAN_IF_ID"], "('42' '43')");
    assert_eq!(v["LINE_STRONGSWAN_TUN_IFACE"], "('ipsec-0' 'ipsec-1')");
    assert_eq!(v["CS_EXCLUDED_PORTS"], "('/dev/ttyUSB9')");
}

/// A consumer indexes `LINE_*[i]` in a `seq 0 $((LINE_COUNT - 1))` loop, so
/// every array must have exactly `LINE_COUNT` elements — a printer that
/// emitted a short array for one key would silently give that line an empty
/// netns/iface. This is the invariant the old `healthcheck.sh` depended on
/// and never checked.
#[test]
fn every_discover_line_array_has_exactly_line_count_elements() {
    let resolution = LineResolution {
        circuit_switched_excluded_ports: vec![],
        lines: vec![entry(0, "a"), entry(1, "b"), entry(2, "c")],
        failed: vec![],
    };

    let rendered = discover::render_discover_shell_env(&resolution);
    let v = vars(&rendered);
    assert_eq!(v["LINE_COUNT"], "3");

    for (key, value) in &v {
        if !key.starts_with("LINE_") || key == "LINE_COUNT" {
            continue;
        }
        let inner = value
            .strip_prefix('(')
            .and_then(|s| s.strip_suffix(')'))
            .unwrap_or_else(|| panic!("{key} is not a shell array: {value}"));
        assert_eq!(
            inner.split_whitespace().count(),
            3,
            "{key} has the wrong element count: {value}"
        );
    }
}

#[test]
fn discover_shell_env_with_no_lines_still_emits_every_key_as_an_empty_array() {
    let rendered = discover::render_discover_shell_env(&LineResolution::default());
    let v = vars(&rendered);

    assert_eq!(v["LINE_COUNT"], "0");
    // Zero lines is a reported condition, not a failure — the consumer's
    // `LINE_COUNT -eq 0` early-exit only works if the keys are still defined.
    for key in [
        "LINE_CARD_ID",
        "LINE_NETNS",
        "LINE_STRONGSWAN_TUN_IFACE",
        "LINE_PCSCF_SOURCE_PATH",
    ] {
        assert_eq!(v[key], "()", "{key} should be an empty array");
    }
}

/// Every value goes through `shell_quote`, so a path or id containing a quote
/// or a space cannot break out of its word and inject shell.
#[test]
fn shell_env_values_are_quoted_against_injection() {
    let mut e = entry(0, "ec20-'; rm -rf /; '");
    e.modem_port = "/dev/tty USB0".to_string();
    let resolution = LineResolution {
        circuit_switched_excluded_ports: vec![],
        lines: vec![e],
        failed: vec![],
    };

    let v = vars(&discover::render_discover_shell_env(&resolution));

    assert_eq!(v["LINE_CARD_ID"], r#"('ec20-'\''; rm -rf /; '\''')"#);
    assert_eq!(v["LINE_MODEM_PORT"], "('/dev/tty USB0')");
}

/// Loads a config through the real parser rather than hand-building an
/// `AppConfig`, so this also pins the *defaults* the printer emits — the
/// values a deployment that configures nothing actually gets.
fn load_minimal_config() -> AppConfig {
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(
        br#"
[sip]
server = "pbx.example.com"
username = "bridge"
password = "s3cret"
"#,
    )
    .unwrap();
    gsm_sip_bridge::config::load_config(f.path()).unwrap()
}

#[test]
fn vowifi_shell_env_emits_the_globals_a_consumer_reads_by_name() {
    let config = load_minimal_config();

    let v = vars(&config_cmd::render_vowifi_shell_env(&config));

    // METRICS_PORT and TUNNEL_ENGINE are read by name, not position.
    assert_eq!(v["METRICS_PORT"], "'9091'");
    assert_eq!(v["TUNNEL_ENGINE"], "'strongswan'");
    assert_eq!(v["APN"], "'ims'");
    assert_eq!(v["VPCD_PORT"], "'15963'");
    // An unset optional renders as an empty quoted string, never as a bare
    // word that would leave the variable undefined after `eval`.
    assert_eq!(v["EPDG_IP"], "''");
    assert_eq!(v["SRC_ADDR"], "''");
}

#[test]
fn volte_shell_env_arrays_line_up_with_the_line_count() {
    let base = VolteConfig::default();
    let lines = gsm_sip_bridge::volte::discovery::resolve_volte_lines(&[], &base).lines;
    let v = vars(&volte::render_volte_discover_lines_shell_env(&lines));

    assert_eq!(v["VOLTE_LINE_COUNT"], "0");
    assert_eq!(v["VOLTE_LINE_CARD_ID"], "()");
    assert_eq!(v["VOLTE_LINE_NETNS"], "()");
    assert_eq!(v["VOLTE_LINE_VETH_CARRIER_ADDR"], "()");
}
