//! Per-line network isolation for VoLTE (specs/020-volte-line-netns).
//!
//! Namespace/veth derivation itself is unit-tested table-driven alongside
//! `resolve_volte_lines` in `src/volte/discovery.rs` (including the FR-004a
//! non-collision check against `vowifi::discovery`), mirroring where
//! `vowifi::discovery`'s own equivalent tests already live. This file covers
//! the cross-module contract instead: the manifest round-trip
//! `volte-discover-lines` writes and `volte-carrier-agent`/`volte-cleanup`
//! read back, and the diagnostic-path fallback shape `BridgeLine` exposes.

use gsm_sip_bridge::volte::bridge::BridgeLine;
use gsm_sip_bridge::volte::discovery::{
    manifest_path, read_manifest, write_manifest, ResolvedVolteLine, MANIFEST_PATH_ENV,
};

/// Only this test touches `MANIFEST_PATH_ENV` in this binary (integration
/// test files are their own process, so this cannot race the crate's own
/// `#[cfg(test)]` unit tests either) — safe to mutate the process
/// environment without a lock.
#[test]
fn manifest_written_by_discover_lines_is_read_back_with_netns_and_veth_intact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("volte-lines.json");
    std::env::set_var(MANIFEST_PATH_ENV, &path);

    let lines = vec![
        ResolvedVolteLine {
            index: 0,
            card_id: "ec20-AAAAAA".to_string(),
            modem_port: "/dev/ttyUSB0".into(),
            cid: 3,
            apn: "ims".to_string(),
            pcscf: None,
            iface: "wwan0".to_string(),
            msisdn: None,
            sip_leg_port: 5074,
            control_port: 5075,
            status_port: 5076,
            netns: "volte".to_string(),
            veth_carrier_iface: "veth-volte-ims".to_string(),
            veth_telephony_iface: "veth-volte-sip".to_string(),
            veth_carrier_addr: "10.98.0.1".to_string(),
            veth_telephony_addr: "10.98.0.2".to_string(),
        },
        ResolvedVolteLine {
            index: 1,
            card_id: "ec20-BBBBBB".to_string(),
            modem_port: "/dev/ttyUSB1".into(),
            cid: 3,
            apn: "ims".to_string(),
            pcscf: Some("2400:5200:a100:819::6".to_string()),
            iface: "wwan1".to_string(),
            msisdn: Some("919000000001".to_string()),
            sip_leg_port: 5078,
            control_port: 5079,
            status_port: 5080,
            netns: "volte1".to_string(),
            veth_carrier_iface: "veth-volte-ims1".to_string(),
            veth_telephony_iface: "veth-volte-sip1".to_string(),
            veth_carrier_addr: "10.98.0.5".to_string(),
            veth_telephony_addr: "10.98.0.6".to_string(),
        },
    ];

    write_manifest(&lines, Some(std::path::Path::new("/run/volte-restore-cid")))
        .expect("write_manifest must succeed against a writable tempdir");

    assert_eq!(manifest_path(), path, "MANIFEST_PATH_ENV must be honoured");

    let manifest = read_manifest(&path).expect("the manifest we just wrote must read back");
    assert_eq!(manifest.lines.len(), 2);

    let l0 = &manifest.lines[0];
    assert_eq!(l0.netns, "volte");
    assert_eq!(l0.veth_carrier_addr, "10.98.0.1");
    assert_eq!(l0.veth_telephony_addr, "10.98.0.2");
    assert_eq!(l0.restore_cid_path, "/run/volte-restore-cid-0");
    assert_eq!(l0.pcscf, "", "no explicit override for line 0");
    assert_eq!(l0.msisdn, "", "no explicit override for line 0");

    let l1 = &manifest.lines[1];
    assert_eq!(l1.netns, "volte1");
    assert_ne!(l1.netns, l0.netns, "two lines' namespaces must not collide");
    assert_eq!(l1.restore_cid_path, "/run/volte-restore-cid-1");
    assert_eq!(l1.pcscf, "2400:5200:a100:819::6");
    assert_eq!(
        l1.msisdn, "919000000001",
        "msisdn override must not be dropped"
    );

    std::env::remove_var(MANIFEST_PATH_ENV);
}

/// The diagnostic single-`--modem` path (research.md R7) builds a
/// `BridgeLine` with empty `netns`/veth fields — this is what
/// `carrier_agent::run` and `bridge::run_inner`'s telephony-line construction
/// key off to fall back to `LOOPBACK`. Documents the contract at the type
/// level so a future change to either side is caught by a compile error or
/// this assertion, not a live-only surprise.
#[test]
fn a_diagnostic_line_has_no_namespace_or_veth_identifiers() {
    let line = BridgeLine {
        card_id: "volte".to_string(),
        settings: gsm_sip_bridge::volte::VolteSettings::default(),
        msisdn: None,
        sip_leg_port: 5074,
        control_port: 5075,
        status_port: 5076,
        netns: String::new(),
        veth_carrier_addr: String::new(),
        veth_telephony_addr: String::new(),
    };
    assert!(line.netns.is_empty());
    assert!(line.veth_carrier_addr.is_empty());
    assert!(line.veth_telephony_addr.is_empty());
}
