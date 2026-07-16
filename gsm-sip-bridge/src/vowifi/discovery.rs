//! Multi-card VoWiFi (specs/013-multi-card-vowifi): role assignment between
//! the circuit-switched and VoWiFi subsystems, line-table resolution, and
//! per-line resource derivation. Built on top of the shared inventory scan
//! in `modules::discovery` (`scan_all`/`ProbedModem`) — this module owns
//! everything specific to *VoWiFi's* use of that scan; `modules::discovery`
//! itself stays free of any dependency on `vowifi` (see its
//! `DEFAULT_LINES_FILE` doc comment).
//!
//! The `gsm-sip-bridge discover` subcommand (`main.rs`) is the single place
//! this module's functions are actually driven from — see
//! `specs/013-multi-card-vowifi/contracts/discover-cli-contract.md`.

use crate::config::{VowifiConfig, VowifiLineOverride};
use crate::modules::discovery::{ProbedModem, SimStatus};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Partition of successfully AT-probed modems into the two subsystems
/// (FR-007/008/009). Built from `modules::discovery::scan_all`'s output —
/// modems with no working AT port at all serve neither and are dropped here.
#[derive(Debug, Clone, Default)]
pub struct RoleAssignment {
    pub circuit_switched: Vec<ProbedModem>,
    /// VoWiFi *candidates* — still subject to `resolve_lines`'s SIM-
    /// readiness filter and `max_lines` bound before becoming actual lines.
    pub vowifi: Vec<ProbedModem>,
}

impl RoleAssignment {
    /// Default rule (FR-008): audio-capable → circuit-switched, audio-less
    /// → VoWiFi. An explicit `[[vowifi.line]]` override (FR-009) always
    /// wins, regardless of audio capability. A modem with no AT port at all
    /// (never probed successfully) serves neither.
    pub fn from_probed(modems: &[ProbedModem], overrides: &[VowifiLineOverride]) -> Self {
        let mut circuit_switched = Vec::new();
        let mut vowifi = Vec::new();
        for modem in modems {
            if modem.at_port.is_none() {
                continue;
            }
            if is_overridden_to_vowifi(modem, overrides) || !modem.has_audio_capability {
                vowifi.push(modem.clone());
            } else {
                circuit_switched.push(modem.clone());
            }
        }
        Self {
            circuit_switched,
            vowifi,
        }
    }
}

/// The override list `RoleAssignment::from_probed` should actually use:
/// `config.line_overrides` as-is, unless it's empty AND `config.modem_port`
/// names a device — in which case that's an existing pre-multi-card config
/// (`[vowifi].modem_port` set, no `[[vowifi.line]]` array at all), and
/// acceptance scenario 5/FR-020 requires that named port keep being used
/// exactly as before, undisturbed by auto-discovery: synthesize a single
/// implicit override pinning it, the same mechanism `[[vowifi.line]]` uses.
/// Once any `[[vowifi.line]]` entry exists, `modem_port` is not consulted
/// here at all — the array is the one source of truth from that point.
pub fn effective_line_overrides(config: &VowifiConfig) -> Vec<VowifiLineOverride> {
    if config.line_overrides.is_empty() && !config.modem_port.is_empty() {
        vec![VowifiLineOverride {
            modem_port: Some(config.modem_port.clone()),
            ..Default::default()
        }]
    } else {
        config.line_overrides.clone()
    }
}

fn is_overridden_to_vowifi(modem: &ProbedModem, overrides: &[VowifiLineOverride]) -> bool {
    overrides.iter().any(|o| {
        o.modem_serial
            .as_deref()
            .is_some_and(|s| s == modem.usb_serial)
            || o.modem_port.as_deref().is_some_and(|p| {
                modem
                    .at_port
                    .as_deref()
                    .is_some_and(|port| port == Path::new(p))
            })
    })
}

fn override_for<'a>(
    modem: &ProbedModem,
    overrides: &'a [VowifiLineOverride],
) -> Option<&'a VowifiLineOverride> {
    overrides.iter().find(|o| {
        o.modem_serial
            .as_deref()
            .is_some_and(|s| s == modem.usb_serial)
            || o.modem_port.as_deref().is_some_and(|p| {
                modem
                    .at_port
                    .as_deref()
                    .is_some_and(|port| port == Path::new(p))
            })
    })
}

/// A modem that can't become a line, and why (FR-006/FR-016).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FailedLine {
    pub card_id: String,
    pub reason: String,
}

/// One resolved VoWiFi line — the "Line Table" key entity
/// (specs/013-multi-card-vowifi data-model.md). `config` is a fully
/// per-line-derived `VowifiConfig`: every isolated resource (netns, XFRM
/// if_id/iface, veth iface/addrs, vpcd_port, pcscf_source_path) has already
/// been computed as a function of `index` (research.md item 5) — downstream
/// code (`ims::agent`, `vowifi::run`) takes `&config` exactly as it does
/// today and needs no awareness that it's one of several lines.
#[derive(Debug, Clone)]
pub struct ResolvedLine {
    pub index: u32,
    pub card_id: String,
    pub modem_port: PathBuf,
    pub mcc: String,
    pub mnc: String,
    pub imsi_override: Option<String>,
    pub config: VowifiConfig,
}

#[derive(Debug, Clone, Default)]
pub struct LineTableResult {
    pub lines: Vec<ResolvedLine>,
    pub failed: Vec<FailedLine>,
}

/// Resolves `assignment.vowifi` into an ordered, bounded `LineTable`
/// (FR-012/FR-016): only SIM-ready candidates become lines, stable card-id
/// order (independent of USB enumeration jitter), capped at
/// `base.max_lines` with the excess reported as failed rather than dropped.
pub fn resolve_lines(assignment: &RoleAssignment, base: &VowifiConfig) -> LineTableResult {
    let mut failed = Vec::new();
    let mut ready: Vec<&ProbedModem> = Vec::new();

    for modem in &assignment.vowifi {
        match &modem.sim_status {
            Some(SimStatus::Ready { .. }) => ready.push(modem),
            Some(SimStatus::Absent) => failed.push(FailedLine {
                card_id: modem.card_id.clone(),
                reason: "sim_absent".to_string(),
            }),
            Some(SimStatus::Locked) => failed.push(FailedLine {
                card_id: modem.card_id.clone(),
                reason: "sim_locked".to_string(),
            }),
            Some(SimStatus::Unreadable(msg)) => failed.push(FailedLine {
                card_id: modem.card_id.clone(),
                reason: format!("sim_unreadable: {msg}"),
            }),
            None => failed.push(FailedLine {
                card_id: modem.card_id.clone(),
                reason: "no_at_port".to_string(),
            }),
        }
    }

    ready.sort_by(|a, b| a.card_id.cmp(&b.card_id));

    let max_lines = base.max_lines as usize;
    let (kept, overflow) = if ready.len() > max_lines {
        ready.split_at(max_lines)
    } else {
        (&ready[..], &[][..])
    };
    for modem in overflow {
        failed.push(FailedLine {
            card_id: modem.card_id.clone(),
            reason: "max_lines_exceeded".to_string(),
        });
    }

    let lines = kept
        .iter()
        .enumerate()
        .map(|(i, modem)| resolve_one_line(i as u32, modem, base))
        .collect();

    LineTableResult { lines, failed }
}

/// One /30 veth block per line — stepping the whole dotted-quad by 4
/// addresses (rather than assuming which octet has room) keeps the
/// derivation correct regardless of the operator's chosen base subnet.
fn shift_ipv4(addr: &str, delta: u32) -> Option<String> {
    let ip: std::net::Ipv4Addr = addr.parse().ok()?;
    let shifted = u32::from(ip).checked_add(delta)?;
    Some(std::net::Ipv4Addr::from(shifted).to_string())
}

fn resolve_one_line(index: u32, modem: &ProbedModem, base: &VowifiConfig) -> ResolvedLine {
    let modem_port = modem
        .at_port
        .clone()
        .expect("a Ready line always has a working AT port");
    let over = override_for(modem, &base.line_overrides);
    let mcc = over
        .and_then(|o| o.mcc.clone())
        .unwrap_or_else(|| base.mcc.clone());
    let mnc = over
        .and_then(|o| o.mnc.clone())
        .unwrap_or_else(|| base.mnc.clone());
    let imsi_override = over
        .and_then(|o| o.imsi_override.clone())
        .or_else(|| base.imsi_override.clone());

    let mut config = base.clone();
    config.modem_port = modem_port.to_string_lossy().to_string();
    config.mcc = mcc.clone();
    config.mnc = mnc.clone();
    config.imsi_override = imsi_override.clone();
    // Not meaningful on a per-line derived config — overrides have already
    // been applied above.
    config.line_overrides = Vec::new();

    // index == 0 keeps every field at its unindexed default, by construction
    // (FR-020) — nothing below runs for the single-line case.
    if index > 0 {
        config.netns = format!("{}{}", base.netns, index);
        config.strongswan_tun_iface = format!("{}-{}", base.strongswan_tun_iface, index);
        config.strongswan_if_id = base.strongswan_if_id.saturating_add(index);
        config.veth_sip_iface = format!("{}{}", base.veth_sip_iface, index);
        config.veth_ims_iface = format!("{}{}", base.veth_ims_iface, index);
        let step = 4u32 * index;
        if let Some(local) = shift_ipv4(&base.veth_local_addr, step) {
            config.veth_local_addr = local;
        }
        if let Some(peer) = shift_ipv4(&base.veth_peer_addr, step) {
            config.veth_peer_addr = peer;
        }
        config.vpcd_port = base.vpcd_port.saturating_add(index as u16);
        config.pcscf_source_path = format!("{}-{}", base.pcscf_source_path, index);
    }

    ResolvedLine {
        index,
        card_id: modem.card_id.clone(),
        modem_port,
        mcc,
        mnc,
        imsi_override,
        config,
    }
}

/// The serialized artifact `gsm-sip-bridge discover` writes so the
/// circuit-switched daemon and `docker/entrypoint.sh` agree on the same
/// role assignment/line table without each re-scanning independently
/// (research.md item 3, `contracts/discover-cli-contract.md`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LineResolution {
    #[serde(default)]
    pub circuit_switched_excluded_ports: Vec<String>,
    #[serde(default)]
    pub lines: Vec<LineResolutionEntry>,
    #[serde(default)]
    pub failed: Vec<FailedLine>,
}

/// Everything a consumer needs for one line: the flat fields
/// `docker/entrypoint.sh`'s `--shell-env` output reads directly, plus the
/// complete derived `VowifiConfig` so `vowifi-ims-agent --line N` (`main.rs`)
/// can load it verbatim with no re-derivation (and, critically, no second
/// USB/AT scan — see this module's top-level doc comment and research.md
/// item 3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineResolutionEntry {
    pub index: u32,
    pub card_id: String,
    pub modem_port: String,
    pub netns: String,
    pub control_port: u16,
    pub veth_local_addr: String,
    pub veth_peer_addr: String,
    pub vpcd_port: u16,
    pub strongswan_if_id: u32,
    pub strongswan_tun_iface: String,
    pub pcscf_source_path: String,
    pub mcc: String,
    pub mnc: String,
    pub config: VowifiConfig,
}

impl From<&ResolvedLine> for LineResolutionEntry {
    fn from(line: &ResolvedLine) -> Self {
        Self {
            index: line.index,
            card_id: line.card_id.clone(),
            modem_port: line.modem_port.to_string_lossy().to_string(),
            netns: line.config.netns.clone(),
            control_port: line.config.control_port,
            veth_local_addr: line.config.veth_local_addr.clone(),
            veth_peer_addr: line.config.veth_peer_addr.clone(),
            vpcd_port: line.config.vpcd_port,
            strongswan_if_id: line.config.strongswan_if_id,
            strongswan_tun_iface: line.config.strongswan_tun_iface.clone(),
            pcscf_source_path: line.config.pcscf_source_path.clone(),
            mcc: line.mcc.clone(),
            mnc: line.mnc.clone(),
            config: line.config.clone(),
        }
    }
}

impl LineResolution {
    pub fn from_result(circuit_switched: &[ProbedModem], result: &LineTableResult) -> Self {
        Self {
            circuit_switched_excluded_ports: Vec::new(),
            lines: result.lines.iter().map(LineResolutionEntry::from).collect(),
            failed: result.failed.clone(),
        }
        .with_cs_exclusions(circuit_switched, result)
    }

    /// `circuit_switched_excluded_ports` isn't "the CS pool" — it's every
    /// VoWiFi line's modem port, so `modules::discovery::scan_modules` can
    /// exclude them (FR-007) without needing to know anything about roles
    /// itself. `circuit_switched` is accepted only to keep the constructor
    /// symmetric with `RoleAssignment`; it plays no part in the exclusion
    /// set.
    fn with_cs_exclusions(
        mut self,
        _circuit_switched: &[ProbedModem],
        result: &LineTableResult,
    ) -> Self {
        self.circuit_switched_excluded_ports = result
            .lines
            .iter()
            .map(|l| l.modem_port.to_string_lossy().to_string())
            .collect();
        self
    }

    /// Looks up one line by its 0-based index (`vowifi-ims-agent --line N`,
    /// `vowifi-status`).
    pub fn line(&self, index: u32) -> Option<&LineResolutionEntry> {
        self.lines.iter().find(|l| l.index == index)
    }
}

/// Reads a `LineResolution` back from disk (used by `main.rs`'s `--line`
/// selector and `vowifi-status`) — a plain, fallible read/parse with no
/// magic env-var defaulting of its own; callers pass the path they got from
/// `crate::modules::discovery::DEFAULT_LINES_FILE`/`LINES_FILE_ENV`.
pub fn read_line_resolution(path: &Path) -> Result<LineResolution, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    serde_json::from_str(&contents).map_err(|e| format!("failed to parse {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ready_modem(card_id: &str, port: &str, audio: bool, imsi: &str) -> ProbedModem {
        ProbedModem {
            card_id: card_id.to_string(),
            model: "EC200",
            usb_serial: card_id.to_string(),
            has_audio_capability: audio,
            audio_device: if audio {
                Some("hw:0,0".to_string())
            } else {
                None
            },
            at_port: Some(PathBuf::from(port)),
            sim_status: Some(SimStatus::Ready {
                imsi: imsi.to_string(),
            }),
        }
    }

    fn unusable_modem(card_id: &str, status: Option<SimStatus>) -> ProbedModem {
        ProbedModem {
            card_id: card_id.to_string(),
            model: "EC200",
            usb_serial: card_id.to_string(),
            has_audio_capability: false,
            audio_device: None,
            at_port: status.as_ref().map(|_| PathBuf::from("/dev/ttyUSB9")),
            sim_status: status,
        }
    }

    #[test]
    fn role_assignment_default_splits_by_audio() {
        let modems = vec![
            ready_modem("ec20-AAAAAA", "/dev/ttyUSB0", true, "404011111111111"),
            ready_modem("ec20-BBBBBB", "/dev/ttyUSB1", false, "404022222222222"),
        ];
        let assignment = RoleAssignment::from_probed(&modems, &[]);
        assert_eq!(assignment.circuit_switched.len(), 1);
        assert_eq!(assignment.circuit_switched[0].card_id, "ec20-AAAAAA");
        assert_eq!(assignment.vowifi.len(), 1);
        assert_eq!(assignment.vowifi[0].card_id, "ec20-BBBBBB");
    }

    #[test]
    fn role_assignment_override_claims_audio_capable_modem() {
        let modems = vec![ready_modem(
            "ec20-AAAAAA",
            "/dev/ttyUSB0",
            true,
            "404011111111111",
        )];
        let overrides = vec![VowifiLineOverride {
            modem_serial: Some("ec20-AAAAAA".to_string()),
            ..Default::default()
        }];
        let assignment = RoleAssignment::from_probed(&modems, &overrides);
        assert!(assignment.circuit_switched.is_empty());
        assert_eq!(assignment.vowifi.len(), 1);
    }

    #[test]
    fn role_assignment_never_double_assigns() {
        let modems = vec![
            ready_modem("ec20-AAAAAA", "/dev/ttyUSB0", true, "404011111111111"),
            ready_modem("ec20-BBBBBB", "/dev/ttyUSB1", false, "404022222222222"),
        ];
        let overrides = vec![VowifiLineOverride {
            modem_port: Some("/dev/ttyUSB0".to_string()),
            ..Default::default()
        }];
        let assignment = RoleAssignment::from_probed(&modems, &overrides);
        let all_ids: Vec<&str> = assignment
            .circuit_switched
            .iter()
            .chain(assignment.vowifi.iter())
            .map(|m| m.card_id.as_str())
            .collect();
        assert_eq!(all_ids.len(), 2);
        assert!(
            !(assignment
                .circuit_switched
                .iter()
                .any(|m| m.card_id == "ec20-AAAAAA")
                && assignment.vowifi.iter().any(|m| m.card_id == "ec20-AAAAAA"))
        );
    }

    #[test]
    fn role_assignment_excludes_modems_with_no_at_port() {
        let modems = vec![unusable_modem("ec20-CCCCCC", None)];
        let assignment = RoleAssignment::from_probed(&modems, &[]);
        assert!(assignment.circuit_switched.is_empty());
        assert!(assignment.vowifi.is_empty());
    }

    #[test]
    fn effective_overrides_empty_when_nothing_configured() {
        let config = VowifiConfig::default();
        assert!(effective_line_overrides(&config).is_empty());
    }

    #[test]
    fn effective_overrides_synthesizes_implicit_override_from_modem_port() {
        let mut config = VowifiConfig::default();
        config.modem_port = "/dev/ttyUSB6".to_string();
        let overrides = effective_line_overrides(&config);
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides[0].modem_port.as_deref(), Some("/dev/ttyUSB6"));
    }

    #[test]
    fn effective_overrides_prefers_explicit_line_array_over_modem_port() {
        let mut config = VowifiConfig::default();
        config.modem_port = "/dev/ttyUSB6".to_string();
        config.line_overrides = vec![VowifiLineOverride {
            modem_port: Some("/dev/ttyUSB10".to_string()),
            ..Default::default()
        }];
        let overrides = effective_line_overrides(&config);
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides[0].modem_port.as_deref(), Some("/dev/ttyUSB10"));
    }

    #[test]
    fn legacy_modem_port_config_pins_that_exact_modem_to_vowifi_even_with_audio() {
        // Acceptance scenario 5 / FR-020: an existing single-SIM config that
        // names a port explicitly keeps using exactly that port, even if
        // (unusually) it happens to be on an audio-capable modem that would
        // otherwise default to the circuit-switched pool.
        let mut config = VowifiConfig::default();
        config.modem_port = "/dev/ttyUSB6".to_string();
        let modems = vec![ready_modem(
            "ec20-AAAAAA",
            "/dev/ttyUSB6",
            true,
            "404011111111111",
        )];
        let overrides = effective_line_overrides(&config);
        let assignment = RoleAssignment::from_probed(&modems, &overrides);
        assert!(assignment.circuit_switched.is_empty());
        assert_eq!(assignment.vowifi.len(), 1);
    }

    #[test]
    fn resolve_lines_orders_by_card_id_not_input_order() {
        let modems = vec![
            ready_modem("ec20-ZZZZZZ", "/dev/ttyUSB1", false, "1"),
            ready_modem("ec20-AAAAAA", "/dev/ttyUSB0", false, "2"),
        ];
        let assignment = RoleAssignment {
            circuit_switched: vec![],
            vowifi: modems,
        };
        let base = VowifiConfig::default();
        let result = resolve_lines(&assignment, &base);
        assert_eq!(result.lines.len(), 2);
        assert_eq!(result.lines[0].card_id, "ec20-AAAAAA");
        assert_eq!(result.lines[1].card_id, "ec20-ZZZZZZ");
        assert!(result.failed.is_empty());
    }

    #[test]
    fn resolve_lines_reports_and_skips_unusable_sims() {
        let assignment = RoleAssignment {
            circuit_switched: vec![],
            vowifi: vec![
                ready_modem("ec20-AAAAAA", "/dev/ttyUSB0", false, "1"),
                unusable_modem("ec20-BBBBBB", Some(SimStatus::Locked)),
                unusable_modem("ec20-CCCCCC", Some(SimStatus::Absent)),
            ],
        };
        let base = VowifiConfig::default();
        let result = resolve_lines(&assignment, &base);
        assert_eq!(result.lines.len(), 1);
        assert_eq!(result.lines[0].card_id, "ec20-AAAAAA");
        assert_eq!(result.failed.len(), 2);
        assert!(result
            .failed
            .iter()
            .any(|f| f.card_id == "ec20-BBBBBB" && f.reason == "sim_locked"));
        assert!(result
            .failed
            .iter()
            .any(|f| f.card_id == "ec20-CCCCCC" && f.reason == "sim_absent"));
    }

    #[test]
    fn resolve_lines_bounds_at_max_lines() {
        let mut base = VowifiConfig::default();
        base.max_lines = 2;
        let modems: Vec<ProbedModem> = (0..4)
            .map(|i| {
                ready_modem(
                    &format!("ec20-{i:06}"),
                    &format!("/dev/ttyUSB{i}"),
                    false,
                    "1",
                )
            })
            .collect();
        let assignment = RoleAssignment {
            circuit_switched: vec![],
            vowifi: modems,
        };
        let result = resolve_lines(&assignment, &base);
        assert_eq!(result.lines.len(), 2);
        assert_eq!(
            result
                .failed
                .iter()
                .filter(|f| f.reason == "max_lines_exceeded")
                .count(),
            2
        );
    }

    #[test]
    fn resolve_lines_single_line_matches_unindexed_defaults() {
        let modems = vec![ready_modem(
            "ec20-AAAAAA",
            "/dev/ttyUSB6",
            false,
            "404938123456789",
        )];
        let assignment = RoleAssignment {
            circuit_switched: vec![],
            vowifi: modems,
        };
        let base = VowifiConfig::default();
        let result = resolve_lines(&assignment, &base);
        assert_eq!(result.lines.len(), 1);
        let line = &result.lines[0];
        assert_eq!(line.index, 0);
        assert_eq!(line.config.netns, base.netns);
        assert_eq!(line.config.strongswan_tun_iface, base.strongswan_tun_iface);
        assert_eq!(line.config.strongswan_if_id, base.strongswan_if_id);
        assert_eq!(line.config.veth_local_addr, base.veth_local_addr);
        assert_eq!(line.config.veth_peer_addr, base.veth_peer_addr);
        assert_eq!(line.config.vpcd_port, base.vpcd_port);
        assert_eq!(line.config.pcscf_source_path, base.pcscf_source_path);
        assert_eq!(line.config.control_port, base.control_port);
        // The one thing that DOES change even for a single line: the modem
        // port comes from discovery, not the (irrelevant) default placeholder.
        assert_eq!(line.config.modem_port, "/dev/ttyUSB6");
    }

    #[test]
    fn resolve_lines_two_lines_derive_distinct_resources() {
        let modems = vec![
            ready_modem("ec20-AAAAAA", "/dev/ttyUSB0", false, "1"),
            ready_modem("ec20-BBBBBB", "/dev/ttyUSB1", false, "2"),
        ];
        let assignment = RoleAssignment {
            circuit_switched: vec![],
            vowifi: modems,
        };
        let base = VowifiConfig::default();
        let result = resolve_lines(&assignment, &base);
        assert_eq!(result.lines.len(), 2);
        let (l0, l1) = (&result.lines[0], &result.lines[1]);
        assert_ne!(l0.config.netns, l1.config.netns);
        assert_ne!(l0.config.strongswan_if_id, l1.config.strongswan_if_id);
        assert_ne!(
            l0.config.strongswan_tun_iface,
            l1.config.strongswan_tun_iface
        );
        assert_ne!(l0.config.veth_local_addr, l1.config.veth_local_addr);
        assert_ne!(l0.config.veth_peer_addr, l1.config.veth_peer_addr);
        assert_ne!(l0.config.vpcd_port, l1.config.vpcd_port);
        assert_ne!(l0.config.pcscf_source_path, l1.config.pcscf_source_path);
        // FR-011: no accidental collisions.
        assert_ne!(l0.config.veth_local_addr, l0.config.veth_peer_addr);
        assert_ne!(l1.config.veth_local_addr, l1.config.veth_peer_addr);
    }

    #[test]
    fn resolve_lines_eight_lines_all_distinct() {
        let modems: Vec<ProbedModem> = (0..8)
            .map(|i| {
                ready_modem(
                    &format!("ec20-{i:06}"),
                    &format!("/dev/ttyUSB{i}"),
                    false,
                    "1",
                )
            })
            .collect();
        let mut base = VowifiConfig::default();
        base.max_lines = 8;
        let assignment = RoleAssignment {
            circuit_switched: vec![],
            vowifi: modems,
        };
        let result = resolve_lines(&assignment, &base);
        assert_eq!(result.lines.len(), 8);
        let mut netns: Vec<&str> = result
            .lines
            .iter()
            .map(|l| l.config.netns.as_str())
            .collect();
        netns.sort();
        netns.dedup();
        assert_eq!(netns.len(), 8);
        let mut vpcd_ports: Vec<u16> = result.lines.iter().map(|l| l.config.vpcd_port).collect();
        vpcd_ports.sort();
        vpcd_ports.dedup();
        assert_eq!(vpcd_ports.len(), 8);
    }

    #[test]
    fn line_override_fixes_mcc_mnc_for_one_line() {
        let modems = vec![ready_modem("ec20-AAAAAA", "/dev/ttyUSB0", false, "1")];
        let mut base = VowifiConfig::default();
        base.line_overrides = vec![VowifiLineOverride {
            modem_serial: Some("ec20-AAAAAA".to_string()),
            mcc: Some("404".to_string()),
            mnc: Some("094".to_string()),
            ..Default::default()
        }];
        let assignment = RoleAssignment {
            circuit_switched: vec![],
            vowifi: modems,
        };
        let result = resolve_lines(&assignment, &base);
        assert_eq!(result.lines[0].mcc, "404");
        assert_eq!(result.lines[0].mnc, "094");
        assert_eq!(result.lines[0].config.mcc, "404");
        assert_eq!(result.lines[0].config.mnc, "094");
    }

    #[test]
    fn line_resolution_round_trips_through_json() {
        let modems = vec![
            ready_modem("ec20-AAAAAA", "/dev/ttyUSB0", false, "1"),
            ready_modem("ec20-BBBBBB", "/dev/ttyUSB1", true, "2"),
        ];
        let assignment = RoleAssignment::from_probed(&modems, &[]);
        let base = VowifiConfig::default();
        let result = resolve_lines(&assignment, &base);
        let resolution = LineResolution::from_result(&assignment.circuit_switched, &result);

        assert_eq!(resolution.lines.len(), 1);
        assert_eq!(
            resolution.circuit_switched_excluded_ports,
            vec!["/dev/ttyUSB0".to_string()]
        );

        let json = serde_json::to_string(&resolution).unwrap();
        let parsed: LineResolution = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.lines.len(), 1);
        assert_eq!(
            parsed.circuit_switched_excluded_ports,
            resolution.circuit_switched_excluded_ports
        );
    }
}
