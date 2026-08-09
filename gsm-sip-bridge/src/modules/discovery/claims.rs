//! Which modems another subsystem has already claimed, read from config and
//! from the two on-disk line manifests.
//!
//! Split out of `discovery::mod` because none of this touches USB, sysfs, or
//! AT at all — it is config and JSON. A card belongs to exactly one subsystem
//! (FR-034); this module is how the circuit-switched scan finds out which ones
//! are already spoken for.

use super::derive_module_id;
use serde::de::DeserializeOwned;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Default path for the VoWiFi line-resolution artifact
/// (specs/013-multi-card-vowifi, `contracts/discover-cli-contract.md`).
/// Re-exported here (not defined in `vowifi::discovery`) so this module — the
/// lower-level shared scan both subsystems build on — has no dependency on
/// the `vowifi` module; `vowifi::discovery`'s writer imports these names
/// instead, the natural direction (a specific feature depending on shared
/// infrastructure, not the reverse).
pub use crate::line::manifest::{
    VOWIFI_LINES_DEFAULT_PATH as DEFAULT_LINES_FILE, VOWIFI_LINES_ENV as LINES_FILE_ENV,
};

/// Resolves the line-resolution file path every reader/writer of it
/// (`main.rs`'s `discover`/`--line` handling, `vowifi::mod`'s Agent B
/// listener setup, `vowifi-status`) should use: `LINES_FILE_ENV` if set,
/// else `DEFAULT_LINES_FILE`.
pub fn lines_file_path() -> PathBuf {
    crate::line::manifest::vowifi_lines_path()
}

/// Reads a partial view of one of the line manifests. Absent or unparsable
/// both degrade to `T::default()` — a missing manifest must claim nothing, so
/// a fleet that never runs `discover` behaves exactly as it did before these
/// features existed. `what` names the file in the parse warning.
///
/// Generic because the VoWiFi and VoLTE readers were line-for-line identical
/// apart from that name and the path source.
fn read_manifest_excerpt<T: DeserializeOwned + Default>(path: &Path, what: &str) -> T {
    let Ok(contents) = fs::read_to_string(path) else {
        return T::default();
    };
    serde_json::from_str(&contents).unwrap_or_else(|e| {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "failed to parse {what}; treating it as absent"
        );
        T::default()
    })
}

#[derive(serde::Deserialize, Default)]
struct LinesFileExcerpt {
    #[serde(default)]
    circuit_switched_excluded_ports: Vec<String>,
    #[serde(default)]
    lines: Vec<LineCardIdExcerpt>,
}

#[derive(serde::Deserialize, Default)]
struct LineCardIdExcerpt {
    #[serde(default)]
    card_id: String,
}

fn read_lines_file_excerpt() -> LinesFileExcerpt {
    read_manifest_excerpt(&lines_file_path(), "VoWiFi line-resolution file")
}

pub(super) fn excluded_ports_from_lines_file() -> HashSet<PathBuf> {
    read_lines_file_excerpt()
        .circuit_switched_excluded_ports
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

/// Card ids of every currently-resolved VoWiFi line — used only by
/// `scan_modules`'s *ongoing* rescans (FR-007), never by a fresh `discover`
/// run: a `docker restart` (same container, same `/tmp`) can leave a stale
/// resolution file from the previous run on disk, and `discover` itself
/// must still do a clean, unbiased probe of everything at that moment (see
/// `scan_all`/`scan_all_preferring`'s doc comments).
pub(super) fn active_vowifi_card_ids() -> HashSet<String> {
    read_lines_file_excerpt()
        .lines
        .into_iter()
        .map(|l| l.card_id)
        .filter(|s| !s.is_empty())
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct VolteManifestExcerpt {
    #[serde(default)]
    lines: Vec<VolteLineExcerpt>,
}

#[derive(serde::Deserialize, Default)]
struct VolteLineExcerpt {
    #[serde(default)]
    card_id: String,
    #[serde(default)]
    modem_port: String,
}

fn read_volte_manifest_excerpt() -> VolteManifestExcerpt {
    read_manifest_excerpt(
        &crate::line::manifest::volte_lines_path(),
        "VoLTE line manifest",
    )
}

/// Ports every resolved (auto-discovered or serial-pinned) VoLTE line
/// actually settled on — read back from the manifest `volte-discover-lines`
/// writes, so an auto-discovered line (no `[[volte.line]]` override to derive
/// a port from) is excluded too, not only explicitly pinned ones.
pub(super) fn active_volte_line_ports() -> HashSet<PathBuf> {
    read_volte_manifest_excerpt()
        .lines
        .into_iter()
        .map(|l| PathBuf::from(l.modem_port))
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

/// Card ids of every currently-resolved VoLTE line — the VoLTE counterpart to
/// `active_vowifi_card_ids`, closing the same "answers AT on several ports"
/// gap by excluding the modem's whole USB device (by its *actually resolved*
/// card id), not just the one port it happened to be probed on.
pub(super) fn active_volte_card_ids() -> HashSet<String> {
    read_volte_manifest_excerpt()
        .lines
        .into_iter()
        .map(|l| l.card_id)
        .filter(|s| !s.is_empty())
        .collect()
}

/// The port the host-side cellular service owns, if it is enabled.
///
/// A card belongs to exactly one subsystem (FR-034). The hazard of getting
/// this wrong is already documented in this module by name — "modem claimed
/// by both subsystems" — with a live symptom recorded: probing a port another
/// subsystem was mid-transaction on produced `AT+CPIN?: no status in
/// response` on an already-registered line.
///
/// Disabled `[volte]` claims nothing, so a deployment that never turns this
/// on behaves exactly as it did before the feature existed (FR-021, FR-024).
/// That default is what makes this safe to merge.
pub fn volte_claimed_ports(config: &crate::config::VolteConfig) -> Vec<PathBuf> {
    if !config.enabled {
        return Vec::new();
    }
    // Any AT port a `[[volte.line]]` override pins in multi-modem discovery
    // mode (specs/018-volte-multi-modem) — all claimed so the
    // circuit-switched pool never grabs a modem this bridge drives.
    let mut ports = Vec::new();
    for over in &config.line_overrides {
        if let Some(p) = &over.modem_port {
            ports.push(PathBuf::from(p));
        }
    }
    ports
}

/// Card ids the host-side LTE bridge claims by SIM/hardware serial in
/// `[[volte.line]]` overrides. Excluding by **card id** (not just port) is
/// what makes exclusion robust when a modem answers `AT` on several `ttyUSB`
/// interfaces — a port-only exclusion misses the modem when the scan settles
/// on a different one of its ports than the override pinned (observed live on
/// the EC25, specs/018-volte-multi-modem). Only serial-pinned lines can be
/// excluded this way; a pure auto-discovery line's card id is not known until
/// the bridge scans at runtime.
pub fn volte_claimed_card_ids(config: &crate::config::VolteConfig) -> Vec<String> {
    if !config.enabled {
        return Vec::new();
    }
    config
        .line_overrides
        .iter()
        .filter_map(|o| o.modem_serial.as_deref().map(derive_module_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A single test, not two — both set the same process-wide
    // GSM_SIP_BRIDGE_LINES_FILE env var, which `cargo test`'s default
    // parallel-within-binary execution would otherwise race (see
    // test_config.rs's convention of giving each env-var test its own
    // unique variable name; that isn't available here since the variable
    // name itself is the thing under test).
    #[test]
    fn excluded_ports_from_lines_file_behavior() {
        std::env::set_var(LINES_FILE_ENV, "/tmp/does-not-exist-013.json");
        assert!(
            excluded_ports_from_lines_file().is_empty(),
            "missing file excludes nothing"
        );
        assert!(
            active_vowifi_card_ids().is_empty(),
            "missing file has no active lines"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lines.json");
        fs::write(
            &path,
            r#"{"circuit_switched_excluded_ports": ["/dev/ttyUSB6", "/dev/ttyUSB10"], "lines": [{"index": 0, "card_id": "ec20-AAAAAA", "modem_port": "/dev/ttyUSB6"}], "failed": []}"#,
        )
        .unwrap();
        std::env::set_var(LINES_FILE_ENV, &path);
        let excluded = excluded_ports_from_lines_file();
        assert_eq!(excluded.len(), 2);
        assert!(excluded.contains(&PathBuf::from("/dev/ttyUSB6")));
        assert!(excluded.contains(&PathBuf::from("/dev/ttyUSB10")));
        let active = active_vowifi_card_ids();
        assert_eq!(active.len(), 1);
        assert!(active.contains("ec20-AAAAAA"));

        fs::write(&path, "not json").unwrap();
        assert!(
            excluded_ports_from_lines_file().is_empty(),
            "unparsable file excludes nothing, just warns"
        );
        assert!(
            active_vowifi_card_ids().is_empty(),
            "unparsable file has no active lines either"
        );

        std::env::remove_var(LINES_FILE_ENV);
    }

    // ---- exclusive card assignment (specs/017 T060/T061/T066) -------------

    #[test]
    fn a_disabled_cellular_service_claims_no_card() {
        // The feature is opt-in and changes nothing until asked (FR-024) —
        // which is what makes it safe to merge.
        let config = crate::config::VolteConfig {
            enabled: false,
            line_overrides: vec![crate::config::VolteLineOverride {
                modem_port: Some("/dev/ttyUSB6".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(volte_claimed_ports(&config).is_empty());
    }

    #[test]
    fn an_enabled_cellular_service_claims_its_card() {
        let config = crate::config::VolteConfig {
            enabled: true,
            line_overrides: vec![crate::config::VolteLineOverride {
                modem_port: Some("/dev/ttyUSB6".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            volte_claimed_ports(&config),
            vec![PathBuf::from("/dev/ttyUSB6")]
        );
    }

    #[test]
    fn discovery_mode_claims_pinned_override_ports_and_serials() {
        // Pinned [[volte.line]]s: the pinned AT ports are claimed, and a
        // serial-pinned line is excluded from the circuit-switched pool by
        // card id (robust to a modem answering AT on several ports) —
        // specs/018-volte-multi-modem.
        let config = crate::config::VolteConfig {
            enabled: true,
            line_overrides: vec![
                crate::config::VolteLineOverride {
                    modem_port: Some("/dev/ttyUSB6".to_string()),
                    ..Default::default()
                },
                crate::config::VolteLineOverride {
                    modem_serial: Some("0123456789ABCDEF".to_string()),
                    modem_port: Some("/dev/ttyUSB9".to_string()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            volte_claimed_ports(&config),
            vec![PathBuf::from("/dev/ttyUSB6"), PathBuf::from("/dev/ttyUSB9"),]
        );
        // "0123456789ABCDEF" -> last 6 alphanumerics, uppercased.
        assert_eq!(volte_claimed_card_ids(&config), vec!["ec20-ABCDEF"]);
    }

    #[test]
    fn a_disabled_service_claims_no_card_ids_even_with_overrides() {
        let config = crate::config::VolteConfig {
            enabled: false,
            line_overrides: vec![crate::config::VolteLineOverride {
                modem_serial: Some("0123456789ABCDEF".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(volte_claimed_card_ids(&config).is_empty());
    }

    #[test]
    fn an_enabled_service_with_no_overrides_claims_nothing_rather_than_everything() {
        // No [[volte.line]] pins must not be read as a wildcard claiming
        // every modem — full auto-discovery claims nothing up front.
        let config = crate::config::VolteConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(volte_claimed_ports(&config).is_empty());
    }

    #[test]
    fn a_card_claimed_by_the_cellular_service_is_kept_out_of_the_circuit_switched_pool() {
        // The "modem claimed by both subsystems" hazard this module already
        // documents by name. Its live symptom was `AT+CPIN?: no status in
        // response` on an already-registered line, because two subsystems
        // were interleaving AT transactions on one port.
        let config = crate::config::VolteConfig {
            enabled: true,
            line_overrides: vec![crate::config::VolteLineOverride {
                modem_port: Some("/dev/ttyUSB6".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let claimed = volte_claimed_ports(&config);
        assert!(claimed.contains(&PathBuf::from("/dev/ttyUSB6")));

        // The exclusion set the circuit-switched scan applies is the union of
        // the VoWiFi line table and this, so a card can belong to exactly one.
        let mut excluded: HashSet<PathBuf> = excluded_ports_from_lines_file();
        excluded.extend(claimed.iter().cloned());
        assert!(excluded.contains(&PathBuf::from("/dev/ttyUSB6")));
    }
}
