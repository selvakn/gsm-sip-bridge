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
use crate::line::resources::{self, shift_ipv4};
use crate::modules::discovery::ProbedModem;
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
/// `config.line_overrides`, the one source of truth for pinning a modem to
/// VoWiFi (`[[vowifi.line]]` — there is no top-level `modem_port` fallback
/// to synthesize an implicit override from anymore).
pub fn effective_line_overrides(config: &VowifiConfig) -> Vec<VowifiLineOverride> {
    config.line_overrides.clone()
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
///
/// Re-exported from [`crate::line`], where it lives now: VoLTE was importing
/// this type *from the VoWiFi module*, which made the LTE path depend on the
/// Wi-Fi path for something that belongs to neither.
pub use crate::line::FailedLine;

/// One resolved VoWiFi line — the "Line Table" key entity
/// (specs/013-multi-card-vowifi data-model.md). `config` is a fully
/// per-line-derived `VowifiConfig`: every isolated resource (netns, XFRM
/// if_id/iface, veth iface/addrs, vpcd_port) has already been computed as a
/// function of `index` (research.md item 5), uniformly for every line
/// including the first — downstream code (`ims::agent`, `vowifi::run`) takes
/// `&config` exactly as it does today and needs no awareness that it's one
/// of several lines — `pcscf_source_path` included, since two simultaneously
/// established lines otherwise overwrite each other's P-CSCF in one file.
#[derive(Debug, Clone)]
pub struct ResolvedLine {
    pub index: u32,
    pub card_id: String,
    pub modem_port: PathBuf,
    pub mcc: String,
    pub mnc: String,
    pub imsi_override: Option<String>,
    /// This line's SIM comes from a physical PC/SC reader rather than a
    /// modem (specs/023-omnikey-pcsc-vowifi) — `modem_port` is an empty
    /// path, and orchestration skips every modem-only step (existence
    /// check, `modem-ims` reconcile, `vowifi-usim-bridge` spawn) for it.
    pub pcsc_reader: bool,
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
    // Role assignment has already established each candidate's AT port, so
    // a `Ready` SIM without one cannot occur here — hence
    // `AlreadyEstablished` rather than VoLTE's `Required`.
    let candidates = crate::line::select(
        &assignment.vowifi,
        base.max_lines,
        crate::line::AtPortRequirement::AlreadyEstablished,
    );
    let mut failed = candidates.failed;
    let max_lines = base.max_lines as usize;

    let mut lines: Vec<ResolvedLine> = candidates
        .kept
        .iter()
        .enumerate()
        .map(|(i, modem)| resolve_one_line(i as u32, modem, base))
        .collect();

    // Card-reader-backed lines (specs/023-omnikey-pcsc-vowifi) are independent
    // of modem scanning entirely — sourced straight from `base.line_overrides`
    // — but continue the same index counter and share the same `max_lines`
    // bound as modem lines combined (spec FR-006): an entry pushed past the
    // cap is reported as failed identically to an excess modem line, using a
    // synthetic `pcscN` card id (N = its position among pcsc overrides) since
    // there is no USB modem identity to report instead.
    let pcsc_overrides: Vec<&VowifiLineOverride> = base
        .line_overrides
        .iter()
        .filter(|o| o.pcsc_reader)
        .collect();
    let remaining_capacity = max_lines.saturating_sub(lines.len());
    for (i, over) in pcsc_overrides.iter().enumerate() {
        let card_id = format!("pcsc{i}");
        if i < remaining_capacity {
            let index = lines.len() as u32;
            lines.push(resolve_one_pcsc_line(index, card_id, over, base));
        } else {
            failed.push(FailedLine::new(
                card_id,
                crate::line::Rejection::MaxLinesExceeded.reason(),
            ));
        }
    }

    LineTableResult { lines, failed }
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
    let imei_override = over
        .and_then(|o| o.imei_override.clone())
        .or_else(|| base.imei_override.clone());

    let mut config = base.clone();
    config.modem_port = modem_port.to_string_lossy().to_string();
    config.mcc = mcc.clone();
    config.mnc = mnc.clone();
    config.imsi_override = imsi_override.clone();
    config.imei_override = imei_override.clone();
    // Not meaningful on a per-line derived config — overrides have already
    // been applied above.
    config.line_overrides = Vec::new();

    // Pure per-line infrastructure — always mechanically derived from the
    // line's index, uniformly for every line including the first. No config
    // knob backs any of this (see `VowifiConfig`'s field docs).
    config.netns = resources::indexed(&base.netns, index);
    config.strongswan_tun_iface = format!("{}-{}", base.strongswan_tun_iface, index);
    config.strongswan_if_id = base.strongswan_if_id.saturating_add(index);
    config.veth_sip_iface = resources::indexed(&base.veth_sip_iface, index);
    config.veth_ims_iface = resources::indexed(&base.veth_ims_iface, index);
    let step = 4u32 * index;
    if let Some(local) = shift_ipv4(&base.veth_local_addr, step) {
        config.veth_local_addr = local;
    }
    if let Some(peer) = shift_ipv4(&base.veth_peer_addr, step) {
        config.veth_peer_addr = peer;
    }
    config.vpcd_port = base.vpcd_port.saturating_add(index as u16);
    // Per-line, like every other resource here. It was left shared, and with
    // only one line ever actually establishing at a time that was invisible.
    // The moment two lines came up together they raced over one file: each
    // line's supervisor writes its own tunnel-assigned P-CSCF to this path and
    // each line's Agent A reads it back, so the loser registered against the
    // *other* carrier's proxy — unreachable from its own netns — and crash-
    // looped. Observed live 2026-07-29 holding an address belonging to neither
    // line (a stale one from an earlier tunnel), which is the same race one
    // step further along.
    config.pcscf_source_path =
        crate::line::resources::indexed(&format!("{}-", base.pcscf_source_path), index);

    ResolvedLine {
        index,
        card_id: modem.card_id.clone(),
        modem_port,
        mcc,
        mnc,
        imsi_override,
        pcsc_reader: false,
        config,
    }
}

/// Deterministically derives a syntactically valid IMEI (TS 23.003 Annex A
/// Luhn check digit) for a `pcsc_reader` line's `+sip.instance` Contact
/// parameter — there's no modem to read a real one from via `AT+CGSN`.
/// Seeded from the line's own IMSI (not randomness) so it stays stable
/// across restarts, since a real network expects the same device identity
/// on every registration attempt from one subscriber. This is not a real,
/// IMEI-database-registered device identity — only a well-formed one.
fn generate_imei(imsi: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    imsi.hash(&mut hasher);
    "specs/023-omnikey-pcsc-vowifi generated imei".hash(&mut hasher);
    let body = format!("{:014}", hasher.finish() % 100_000_000_000_000);
    format!("{body}{}", luhn_check_digit(&body))
}

/// The Luhn check digit (TS 23.003 Annex A) that would append to `body` to
/// make a full 15-digit IMEI.
fn luhn_check_digit(body: &str) -> u8 {
    let sum: u32 = body
        .chars()
        .rev()
        .enumerate()
        .map(|(i, c)| {
            let d = c.to_digit(10).expect("body must be all ASCII digits");
            if i % 2 == 0 {
                let doubled = d * 2;
                if doubled > 9 {
                    doubled - 9
                } else {
                    doubled
                }
            } else {
                d
            }
        })
        .sum();
    ((10 - (sum % 10)) % 10) as u8
}

/// The card-reader-backed counterpart to `resolve_one_line`: same per-index
/// infrastructure derivation (netns, veth, strongswan if_id/iface, vpcd
/// port), but the network identity comes straight from the override's
/// mandatory `mcc`/`mnc`/`imsi_override` (config validation guarantees these
/// are `Some`, per `parse_vowifi_line_overrides`) rather than from a probed
/// modem — there is no modem to read them from.
fn resolve_one_pcsc_line(
    index: u32,
    card_id: String,
    over: &VowifiLineOverride,
    base: &VowifiConfig,
) -> ResolvedLine {
    let mcc = over.mcc.clone().unwrap_or_default();
    let mnc = over.mnc.clone().unwrap_or_default();
    let imsi_override = over.imsi_override.clone();
    // No modem means no AT+CGSN to read a real IMEI from. Autogenerate a
    // syntactically valid, stable one (seeded from this line's own IMSI)
    // unless the operator pinned one explicitly — real IMS-AKA registration
    // needs *some* device identity in the Contact header's +sip.instance
    // (specs/023-omnikey-pcsc-vowifi's original scope only covered the ePDG
    // tunnel; IMS registration surfaced this gap during live testing).
    let imei_override = Some(
        over.imei_override
            .clone()
            .unwrap_or_else(|| generate_imei(imsi_override.as_deref().unwrap_or(""))),
    );

    let mut config = base.clone();
    config.modem_port = String::new();
    config.mcc = mcc.clone();
    config.mnc = mnc.clone();
    config.imsi_override = imsi_override.clone();
    config.imei_override = imei_override.clone();
    config.pcsc_reader = true;
    config.line_overrides = Vec::new();

    config.netns = resources::indexed(&base.netns, index);
    config.strongswan_tun_iface = format!("{}-{}", base.strongswan_tun_iface, index);
    config.strongswan_if_id = base.strongswan_if_id.saturating_add(index);
    config.veth_sip_iface = resources::indexed(&base.veth_sip_iface, index);
    config.veth_ims_iface = resources::indexed(&base.veth_ims_iface, index);
    let step = 4u32 * index;
    if let Some(local) = shift_ipv4(&base.veth_local_addr, step) {
        config.veth_local_addr = local;
    }
    if let Some(peer) = shift_ipv4(&base.veth_peer_addr, step) {
        config.veth_peer_addr = peer;
    }
    config.vpcd_port = base.vpcd_port.saturating_add(index as u16);

    ResolvedLine {
        index,
        card_id,
        modem_port: PathBuf::new(),
        mcc,
        mnc,
        imsi_override,
        pcsc_reader: true,
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
/// the `--shell-env` output reads directly, plus the
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
    #[serde(default)]
    pub pcsc_reader: bool,
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
            pcsc_reader: line.pcsc_reader,
            config: line.config.clone(),
        }
    }
}

impl LineResolution {
    pub fn from_result(vowifi: &[ProbedModem], result: &LineTableResult) -> Self {
        Self {
            circuit_switched_excluded_ports: Vec::new(),
            lines: result.lines.iter().map(LineResolutionEntry::from).collect(),
            failed: result.failed.clone(),
        }
        .with_cs_exclusions(vowifi)
    }

    /// `circuit_switched_excluded_ports` isn't "the CS pool" — it's every
    /// modem the role assignment gave to VoWiFi (`assignment.vowifi`), so
    /// `modules::discovery::scan_modules` can exclude them (FR-007) without
    /// needing to know anything about roles itself.
    ///
    /// Deliberately built from the *role-assigned* candidates, not
    /// `result.lines` (only the ones that successfully became lines): a
    /// modem an operator explicitly overrode to VoWiFi (FR-009) still
    /// belongs to VoWiFi even when its SIM is transiently unreadable *this*
    /// `discover` run — excluding only successes let the circuit-switched
    /// pool claim it instead, which then held its port open indefinitely,
    /// turning one transient read failure into a permanent one (found
    /// live-testing a genuine 2-line deployment where this raced for the
    /// first time).
    fn with_cs_exclusions(mut self, vowifi: &[ProbedModem]) -> Self {
        self.circuit_switched_excluded_ports = vowifi
            .iter()
            .filter_map(|m| m.at_port.as_ref())
            .map(|p| p.to_string_lossy().to_string())
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
    use crate::modules::discovery::SimStatus;
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
            net_device: None,
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
            net_device: None,
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
    fn effective_overrides_reflects_line_overrides_only() {
        // mcc/mnc/modem_port moved to [[vowifi.line]] only — there is no
        // top-level `modem_port` to synthesize an implicit override from
        // anymore, so `effective_line_overrides` is just a passthrough.
        let config = VowifiConfig::default();
        assert!(effective_line_overrides(&config).is_empty());
    }

    #[test]
    fn explicit_line_array_pins_that_exact_modem_to_vowifi_even_with_audio() {
        // An explicit `[[vowifi.line]]` entry naming a modem keeps using
        // exactly that port, even if (unusually) it happens to be on an
        // audio-capable modem that would otherwise default to the
        // circuit-switched pool.
        let config = VowifiConfig {
            line_overrides: vec![VowifiLineOverride {
                modem_port: Some("/dev/ttyUSB6".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
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
        let base = VowifiConfig {
            max_lines: 2,
            ..Default::default()
        };
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
    fn resolve_lines_single_line_still_goes_through_index_derivation() {
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
        // Index 0 goes through the exact same derivation as every other
        // line — no more special-cased "unindexed defaults" branch. String
        // suffixes always apply (netns "ims" -> "ims0"); the numeric
        // shifts/offsets happen to reduce to the base value at index 0.
        assert_eq!(line.config.netns, format!("{}0", base.netns));
        assert_eq!(
            line.config.strongswan_tun_iface,
            format!("{}-0", base.strongswan_tun_iface)
        );
        assert_eq!(line.config.strongswan_if_id, base.strongswan_if_id);
        assert_eq!(line.config.veth_local_addr, base.veth_local_addr);
        assert_eq!(line.config.veth_peer_addr, base.veth_peer_addr);
        assert_eq!(line.config.vpcd_port, base.vpcd_port);
        // Suffixed even for a single line, like every other derived resource
        // here — so line 0 keeps the same path whether or not a second line
        // later exists.
        assert_eq!(
            line.config.pcscf_source_path,
            format!("{}-0", base.pcscf_source_path)
        );
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
        // Regression test for a live two-line failure: one shared file meant
        // each line's supervisor overwrote the other's tunnel-assigned P-CSCF,
        // so a line could register against the wrong carrier's proxy —
        // unreachable from its own netns — and crash-loop.
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
        let base = VowifiConfig {
            max_lines: 8,
            ..Default::default()
        };
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
        let base = VowifiConfig {
            line_overrides: vec![VowifiLineOverride {
                modem_serial: Some("ec20-AAAAAA".to_string()),
                mcc: Some("404".to_string()),
                mnc: Some("094".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
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
    fn line_override_fixes_imsi_and_imei_for_one_line() {
        let modems = vec![ready_modem("ec20-AAAAAA", "/dev/ttyUSB0", false, "1")];
        let base = VowifiConfig {
            line_overrides: vec![VowifiLineOverride {
                modem_serial: Some("ec20-AAAAAA".to_string()),
                imsi_override: Some("404400975938075".to_string()),
                imei_override: Some("864650053414154".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let assignment = RoleAssignment {
            circuit_switched: vec![],
            vowifi: modems,
        };
        let result = resolve_lines(&assignment, &base);
        assert_eq!(
            result.lines[0].config.imsi_override.as_deref(),
            Some("404400975938075")
        );
        assert_eq!(
            result.lines[0].config.imei_override.as_deref(),
            Some("864650053414154")
        );
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
        let resolution = LineResolution::from_result(&assignment.vowifi, &result);

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

    #[test]
    fn a_modem_overridden_to_vowifi_stays_excluded_even_when_its_sim_read_fails() {
        // An operator's [[vowifi.line]] override declares intent regardless
        // of audio capability (FR-009) — a modem that fails its SIM read
        // *this* discover run still belongs to VoWiFi, not the
        // circuit-switched pool. Excluding only successfully-resolved lines
        // let the circuit-switched daemon claim it instead, holding its port
        // open indefinitely and turning one transient read failure into a
        // permanent one (found live-testing a genuine 2-line deployment).
        let modems = vec![unusable_modem(
            "ec20-CCCCCC",
            Some(SimStatus::Unreadable("13".to_string())),
        )];
        let overrides = vec![VowifiLineOverride {
            modem_serial: Some("ec20-CCCCCC".to_string()),
            ..Default::default()
        }];
        let assignment = RoleAssignment::from_probed(&modems, &overrides);
        assert!(assignment.circuit_switched.is_empty());
        assert_eq!(
            assignment.vowifi.len(),
            1,
            "override still assigns the role"
        );

        let base = VowifiConfig::default();
        let result = resolve_lines(&assignment, &base);
        assert!(
            result.lines.is_empty(),
            "the SIM read failure blocks the line"
        );

        let resolution = LineResolution::from_result(&assignment.vowifi, &result);
        assert_eq!(
            resolution.circuit_switched_excluded_ports,
            vec!["/dev/ttyUSB9".to_string()],
            "excluded from the circuit-switched pool despite never becoming a line"
        );
    }

    fn pcsc_override(imsi: &str, mcc: &str, mnc: &str) -> VowifiLineOverride {
        VowifiLineOverride {
            pcsc_reader: true,
            imsi_override: Some(imsi.to_string()),
            mcc: Some(mcc.to_string()),
            mnc: Some(mnc.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_lines_pcsc_only_produces_one_line_with_no_modem() {
        // specs/023-omnikey-pcsc-vowifi US1 (T006): a pcsc_reader override
        // with no ProbedModems at all still produces exactly one line.
        let base = VowifiConfig {
            line_overrides: vec![pcsc_override("404940123456789", "404", "043")],
            ..Default::default()
        };
        let assignment = RoleAssignment {
            circuit_switched: vec![],
            vowifi: vec![],
        };
        let result = resolve_lines(&assignment, &base);
        assert!(result.failed.is_empty());
        assert_eq!(result.lines.len(), 1);
        let line = &result.lines[0];
        assert!(line.pcsc_reader);
        assert_eq!(line.modem_port, PathBuf::new());
        assert_eq!(line.imsi_override.as_deref(), Some("404940123456789"));
        assert_eq!(line.mcc, "404");
        assert_eq!(line.mnc, "043");
        assert_eq!(line.index, 0);
        assert_eq!(line.card_id, "pcsc0");
    }

    #[test]
    fn resolve_lines_mixed_modem_and_pcsc_get_distinct_resources() {
        // specs/023-omnikey-pcsc-vowifi US2 (T017): one modem line + one
        // pcsc_reader line coexist with distinct index/netns/veth, and the
        // modem line's shape is unchanged.
        let modems = vec![ready_modem(
            "ec20-AAAAAA",
            "/dev/ttyUSB0",
            false,
            "404011111111111",
        )];
        let base = VowifiConfig {
            line_overrides: vec![pcsc_override("404940123456789", "404", "043")],
            ..Default::default()
        };
        let assignment = RoleAssignment {
            circuit_switched: vec![],
            vowifi: modems,
        };
        let result = resolve_lines(&assignment, &base);
        assert_eq!(result.lines.len(), 2);
        let (modem_line, pcsc_line) = (&result.lines[0], &result.lines[1]);

        assert!(!modem_line.pcsc_reader);
        assert_eq!(modem_line.modem_port, PathBuf::from("/dev/ttyUSB0"));
        assert_eq!(modem_line.index, 0);

        assert!(pcsc_line.pcsc_reader);
        assert_eq!(pcsc_line.modem_port, PathBuf::new());
        assert_eq!(pcsc_line.index, 1);
        assert_eq!(pcsc_line.card_id, "pcsc0");

        assert_ne!(modem_line.config.netns, pcsc_line.config.netns);
        assert_ne!(
            modem_line.config.veth_local_addr,
            pcsc_line.config.veth_local_addr
        );
        assert_ne!(
            modem_line.config.strongswan_if_id,
            pcsc_line.config.strongswan_if_id
        );
    }

    #[test]
    fn resolve_lines_pcsc_overflow_reported_like_excess_modem_line() {
        // specs/023-omnikey-pcsc-vowifi US2 (T018): pcsc lines share
        // max_lines with modem lines, not an unbounded separate pool.
        let base = VowifiConfig {
            max_lines: 1,
            line_overrides: vec![pcsc_override("404940123456789", "404", "043")],
            ..Default::default()
        };
        let modems = vec![ready_modem(
            "ec20-AAAAAA",
            "/dev/ttyUSB0",
            false,
            "404011111111111",
        )];
        let assignment = RoleAssignment {
            circuit_switched: vec![],
            vowifi: modems,
        };
        let result = resolve_lines(&assignment, &base);
        assert_eq!(result.lines.len(), 1);
        assert!(!result.lines[0].pcsc_reader, "the modem line wins the slot");
        assert_eq!(result.failed.len(), 1);
        assert_eq!(result.failed[0].card_id, "pcsc0");
        assert_eq!(result.failed[0].reason, "max_lines_exceeded");
    }

    #[test]
    fn resolve_lines_modem_only_scenario_unchanged_by_pcsc_feature() {
        // specs/023-omnikey-pcsc-vowifi US2 (T019): a modem-only deployment
        // (no pcsc_reader overrides at all) is byte-identical to before this
        // feature — no pcsc lines appended, no failed entries introduced.
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
        assert!(result.failed.is_empty());
        assert!(result.lines.iter().all(|l| !l.pcsc_reader));
    }

    #[test]
    fn luhn_check_digit_matches_known_valid_imei() {
        // 490154203237518 is a commonly-cited Luhn-valid IMEI; the check
        // digit (8) is verified against the 14-digit body.
        assert_eq!(luhn_check_digit("49015420323751"), 8);
    }

    #[test]
    fn generate_imei_is_luhn_valid_and_15_digits() {
        let imei = generate_imei("404438083996440");
        assert_eq!(imei.len(), 15);
        assert!(imei.chars().all(|c| c.is_ascii_digit()));
        let (body, check) = imei.split_at(14);
        assert_eq!(check, luhn_check_digit(body).to_string());
    }

    #[test]
    fn generate_imei_is_stable_for_the_same_imsi() {
        assert_eq!(
            generate_imei("404438083996440"),
            generate_imei("404438083996440")
        );
    }

    #[test]
    fn generate_imei_differs_across_imsis() {
        assert_ne!(
            generate_imei("404438083996440"),
            generate_imei("404940123456789")
        );
    }

    #[test]
    fn resolve_one_pcsc_line_autogenerates_imei_when_unset() {
        // 2026-07-28 gap: IMS-AKA registration needs a device identity
        // (+sip.instance) that spec 023's original scope never populated for
        // a pcsc_reader line, leaving it permanently None.
        let over = pcsc_override("404438083996440", "404", "043");
        let base = VowifiConfig::default();
        let line = resolve_one_pcsc_line(0, "pcsc0".to_string(), &over, &base);
        let imei = line
            .config
            .imei_override
            .expect("imei must be auto-generated");
        assert_eq!(imei.len(), 15);
        assert!(line.config.pcsc_reader);
    }

    #[test]
    fn resolve_one_pcsc_line_respects_explicit_imei_override() {
        let mut over = pcsc_override("404438083996440", "404", "043");
        over.imei_override = Some("123456789012345".to_string());
        let base = VowifiConfig::default();
        let line = resolve_one_pcsc_line(0, "pcsc0".to_string(), &over, &base);
        assert_eq!(
            line.config.imei_override.as_deref(),
            Some("123456789012345")
        );
    }
}
