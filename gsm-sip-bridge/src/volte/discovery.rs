//! Multi-modem VoLTE line resolution (specs/018-volte-multi-modem,
//! specs/020-volte-line-netns): which discovered modems become host-side LTE
//! bridge lines, and each line's per-line-derived ports, namespace, veth
//! identifiers and PDN/P-CSCF settings. The LTE counterpart to
//! [`crate::vowifi::discovery`], and now shaped much more like it: every line
//! gets its own network namespace and veth pair, derived the same way
//! [`crate::vowifi::discovery`] derives its own — on a distinct (`volte`-
//! prefixed) base so the two subsystems' identifiers can never collide
//! (specs/020-volte-line-netns FR-004a) — alongside the loopback port trio
//! (the carrier half's status listener, the carrier↔telephony leg, and the
//! control channel) that predates it and still applies for the single-line
//! diagnostic path, which has no namespace (research.md R7).
//!
//! Resolution is a pure function over [`ProbedModem`]s (from the shared
//! [`crate::modules::discovery`] scan) and the `[volte]` base config, so the
//! whole role/port/namespace/settings derivation is unit-testable without a
//! modem.

use crate::config::{VolteConfig, VolteLineOverride};
use crate::modules::discovery::{ProbedModem, SimStatus};
use crate::volte::bridge::{LOOPBACK_CONTROL_PORT, LOOPBACK_SIP_PORT, LOOPBACK_STATUS_PORT};
use crate::vowifi::discovery::FailedLine;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Port spacing between consecutive lines' loopback trios. Each line owns a
/// contiguous block of this many ports starting at its base; a stride of 4
/// leaves the three used ports (sip-leg / control / status) plus one spare,
/// so the blocks never overlap and there is headroom to add a fourth port
/// later without re-spacing everything.
pub const LINE_PORT_STRIDE: u16 = 4;

/// Loopback SIP-leg port for line `index` — line 0 keeps today's
/// [`LOOPBACK_SIP_PORT`], later lines step by [`LINE_PORT_STRIDE`].
pub fn sip_leg_port(index: u32) -> u16 {
    LOOPBACK_SIP_PORT + (index as u16) * LINE_PORT_STRIDE
}

/// Loopback control port for line `index` — line 0 keeps today's
/// [`LOOPBACK_CONTROL_PORT`].
pub fn control_port(index: u32) -> u16 {
    LOOPBACK_CONTROL_PORT + (index as u16) * LINE_PORT_STRIDE
}

/// Loopback registration-status port for line `index` — line 0 is
/// [`LOOPBACK_STATUS_PORT`].
pub fn status_port(index: u32) -> u16 {
    LOOPBACK_STATUS_PORT + (index as u16) * LINE_PORT_STRIDE
}

/// One resolved host-side LTE line — the LTE analogue of
/// [`crate::vowifi::discovery::ResolvedLine`]. Everything the bridge needs to
/// attach this modem's PDN, register over it, and join its two loopback
/// halves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedVolteLine {
    pub index: u32,
    pub card_id: String,
    pub modem_port: PathBuf,
    pub cid: u8,
    pub apn: String,
    /// Explicitly-configured P-CSCF address for this line (`None` = fall back
    /// to the ePDG capture at runtime, as the single-line path already does).
    pub pcscf: Option<String>,
    /// Host data interface bound to this line's IMS PDN (`""` = manage the
    /// PDN only, skip host interface configuration — the base behaviour).
    pub iface: String,
    pub msisdn: Option<String>,
    pub sip_leg_port: u16,
    pub control_port: u16,
    pub status_port: u16,
    /// This line's network namespace, derived from `[volte].netns`
    /// (specs/020-volte-line-netns): index 0 keeps the unindexed base
    /// (back-compat identity), later lines append their index — exactly the
    /// shape `vowifi::discovery::resolve_one_line` already derives `netns`
    /// in, on a distinct (`volte`-prefixed) base so the two subsystems'
    /// namespaces can never collide (FR-004a).
    pub netns: String,
    /// Veth end inside `netns` — the carrier agent's side.
    pub veth_carrier_iface: String,
    /// Veth end in the default namespace — the shared telephony half's side.
    pub veth_telephony_iface: String,
    /// `/30` address for the carrier-agent side of the veth link.
    pub veth_carrier_addr: String,
    /// `/30` address for the telephony-half side of the veth link.
    pub veth_telephony_addr: String,
}

/// The resolved LTE line table plus the modems that could not become lines,
/// each with a reason (mirrors [`crate::vowifi::discovery::LineTableResult`]).
#[derive(Debug, Clone, Default)]
pub struct VolteLineTableResult {
    pub lines: Vec<ResolvedVolteLine>,
    pub failed: Vec<FailedLine>,
}

/// Resolves every VoLTE-usable modem into an ordered, bounded line table.
///
/// Unlike VoWiFi there is no audio-capability split: the bridge carries a
/// call as IMS RTP over the LTE PDN, not the modem's ALSA device, so **every**
/// AT-reachable, SIM-ready modem is a candidate line. Candidates are ordered
/// by card id (stable across USB enumeration jitter), capped at
/// `base.max_lines` with the excess reported as `max_lines_exceeded` rather
/// than silently dropped, and each kept modem's per-line settings are derived
/// from the `[volte]` base with any matching `[[volte.line]]` override applied.
pub fn resolve_volte_lines(modems: &[ProbedModem], base: &VolteConfig) -> VolteLineTableResult {
    let mut failed = Vec::new();
    let mut ready: Vec<&ProbedModem> = Vec::new();

    for modem in modems {
        match (&modem.sim_status, &modem.at_port) {
            (Some(SimStatus::Ready { .. }), Some(_)) => ready.push(modem),
            (Some(SimStatus::Ready { .. }), None) => failed.push(FailedLine {
                card_id: modem.card_id.clone(),
                reason: "no_at_port".to_string(),
            }),
            (Some(SimStatus::Absent), _) => failed.push(FailedLine {
                card_id: modem.card_id.clone(),
                reason: "sim_absent".to_string(),
            }),
            (Some(SimStatus::Locked), _) => failed.push(FailedLine {
                card_id: modem.card_id.clone(),
                reason: "sim_locked".to_string(),
            }),
            (Some(SimStatus::Unreadable(msg)), _) => failed.push(FailedLine {
                card_id: modem.card_id.clone(),
                reason: format!("sim_unreadable: {msg}"),
            }),
            (None, _) => failed.push(FailedLine {
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
        .map(|(i, modem)| resolve_one_volte_line(i as u32, modem, base))
        .collect();

    VolteLineTableResult { lines, failed }
}

fn override_for<'a>(
    modem: &ProbedModem,
    overrides: &'a [VolteLineOverride],
) -> Option<&'a VolteLineOverride> {
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

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Adds `delta` to an IPv4 address, for deriving each line's `/30` veth
/// block from the `[volte]` base — identical mechanism to
/// `vowifi::discovery`'s own (private) `shift_ipv4`, not shared across
/// modules since it is a two-line pure function used exactly once on each
/// side (specs/020-volte-line-netns research.md R4).
fn shift_ipv4(addr: &str, delta: u32) -> Option<String> {
    let ip: std::net::Ipv4Addr = addr.parse().ok()?;
    let shifted = u32::from(ip).checked_add(delta)?;
    Some(std::net::Ipv4Addr::from(shifted).to_string())
}

fn resolve_one_volte_line(
    index: u32,
    modem: &ProbedModem,
    base: &VolteConfig,
) -> ResolvedVolteLine {
    let over = override_for(modem, &base.line_overrides);
    let cid = over.and_then(|o| o.cid).unwrap_or(base.cid);
    let apn = over
        .and_then(|o| o.apn.clone())
        .unwrap_or_else(|| base.apn.clone());
    let pcscf = over
        .and_then(|o| o.pcscf.clone())
        .or_else(|| non_empty(&base.pcscf));
    // Interface precedence: an explicit per-line override wins, then the
    // interface auto-detected from this modem's own USB device (the multi-
    // modem case, where each modem has its own netdev), then the `[volte]`
    // base interface as a last resort (the single-line case).
    let iface = over
        .and_then(|o| o.iface.clone())
        .or_else(|| modem.net_device.clone())
        .or_else(|| non_empty(&base.iface))
        .unwrap_or_default();
    let msisdn = over.and_then(|o| o.msisdn.clone());

    // Namespace/veth derivation (specs/020-volte-line-netns research.md R4):
    // index 0 keeps the unindexed base — still isolated, just not suffixed —
    // exactly the shape `vowifi::discovery::resolve_one_line` derives `netns`
    // in (`discovery.rs:227`). Isolation is unconditional (FR-004b): there is
    // no "no netns" branch for the single-line case.
    let netns = if index == 0 {
        base.netns.clone()
    } else {
        format!("{}{}", base.netns, index)
    };
    let veth_carrier_iface = if index == 0 {
        base.veth_carrier_iface.clone()
    } else {
        format!("{}{}", base.veth_carrier_iface, index)
    };
    let veth_telephony_iface = if index == 0 {
        base.veth_telephony_iface.clone()
    } else {
        format!("{}{}", base.veth_telephony_iface, index)
    };
    let step = 4u32 * index;
    let veth_carrier_addr =
        shift_ipv4(&base.veth_carrier_addr, step).unwrap_or_else(|| base.veth_carrier_addr.clone());
    let veth_telephony_addr = shift_ipv4(&base.veth_telephony_addr, step)
        .unwrap_or_else(|| base.veth_telephony_addr.clone());

    ResolvedVolteLine {
        index,
        card_id: modem.card_id.clone(),
        modem_port: modem
            .at_port
            .clone()
            .expect("a Ready line always has a working AT port"),
        cid,
        apn,
        pcscf,
        iface,
        msisdn,
        sip_leg_port: sip_leg_port(index),
        control_port: control_port(index),
        status_port: status_port(index),
        netns,
        veth_carrier_iface,
        veth_telephony_iface,
        veth_carrier_addr,
        veth_telephony_addr,
    }
}

/// The on-disk manifest the running bridge writes so `docker/entrypoint.sh`'s
/// cleanup can tear down every line's PDN (each modem's own displaced context
/// is restored) and `volte-status` can query every line's loopback ports —
/// the LTE counterpart to VoWiFi's line-resolution file. Written by the
/// bridge, never by `discover`: the LTE lines are resolved in-process at
/// startup, not by a separate command.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolteLineManifest {
    pub lines: Vec<VolteLineManifestEntry>,
}

/// One line as the cleanup/status consumers need it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolteLineManifestEntry {
    pub index: u32,
    pub card_id: String,
    pub modem_port: String,
    pub cid: u8,
    /// This line's requested APN. Found live to matter more than it looks:
    /// an empty APN here makes `AT+CGDCONT` request the network's *default*
    /// bearer instead of the dedicated IMS one — the network still attaches
    /// and assigns an address, but on the wrong APN (observed:
    /// `www.mnc043.mcc404.gprs` instead of the requested `ims....`), and the
    /// P-CSCF/IMS core is then unreachable from that bearer even though the
    /// interface looks fully configured. Must round-trip through the
    /// manifest for `volte-carrier-agent --line N` to request the right one.
    pub apn: String,
    pub iface: String,
    /// File this line's `attach` recorded its displaced context id in, so
    /// cleanup can `volte-pdn down --restore-cid` it.
    pub restore_cid_path: String,
    pub status_port: u16,
    pub control_port: u16,
    pub sip_leg_port: u16,
    /// This line's network namespace (specs/020-volte-line-netns). Read by
    /// `docker/entrypoint.sh`'s cleanup trap so teardown runs *inside* the
    /// namespace before it is deleted (research.md R6), without re-deriving
    /// it.
    pub netns: String,
    /// This line's carrier-agent-side veth address. Empty means "no netns for
    /// this line" (the single-`--modem` diagnostic path, `volte::bridge`'s
    /// in-process arrangement) — the carrier agent then binds `LOOPBACK`
    /// instead, exactly as before this feature.
    pub veth_carrier_addr: String,
    /// This line's telephony-half-side veth address, for the same reason.
    pub veth_telephony_addr: String,
    /// This line's explicitly-configured P-CSCF (from `[[volte.line]]` or
    /// `[volte].pcscf`), empty if none — `volte-carrier-agent --line N`
    /// resolves it the same way `volte-bridge` always has (an explicit
    /// address wins, else the ePDG capture file), just from this field
    /// instead of re-deriving the override (research.md R7).
    pub pcscf: String,
    /// This line's explicitly-configured MSISDN override (from
    /// `[[volte.line]]` or `[volte].msisdn`), empty if none — used as the
    /// IMS public identity instead of the IMSI-derived default. Must
    /// round-trip through the manifest for the same reason `apn` does: the
    /// split carrier-agent/bridge processes only ever see this line via the
    /// manifest, not the original config.
    pub msisdn: String,
}

/// Default path for the running bridge's line manifest.
pub const DEFAULT_MANIFEST_PATH: &str = "/run/volte-lines.json";
/// Env var overriding [`DEFAULT_MANIFEST_PATH`], read by both the writer (the
/// bridge) and the readers (`volte-status`, `docker/entrypoint.sh`).
pub const MANIFEST_PATH_ENV: &str = "GSM_SIP_BRIDGE_VOLTE_LINES_FILE";

/// Resolves the manifest path every reader/writer should use: `MANIFEST_PATH_ENV`
/// if set, else [`DEFAULT_MANIFEST_PATH`].
pub fn manifest_path() -> PathBuf {
    PathBuf::from(
        std::env::var(MANIFEST_PATH_ENV).unwrap_or_else(|_| DEFAULT_MANIFEST_PATH.to_string()),
    )
}

/// Builds the manifest from a resolved line table and writes it to
/// [`manifest_path`] (specs/020-volte-line-netns). Called by
/// `volte-discover-lines` (the production, auto-discovered path) before any
/// per-line namespace/process is started — `volte-carrier-agent --line N`
/// and `volte-bridge` (Agent B) both read it back rather than re-deriving
/// (research.md R7's "discover once" principle, reused from specs/013 item
/// 3). Best-effort: a write failure degrades cleanup/status, not the calls
/// themselves — logged by the caller, not here.
pub fn write_manifest(
    lines: &[ResolvedVolteLine],
    restore_cid_base: Option<&Path>,
) -> Result<(), String> {
    let manifest = VolteLineManifest {
        lines: lines
            .iter()
            .map(|l| VolteLineManifestEntry {
                index: l.index,
                card_id: l.card_id.clone(),
                modem_port: l.modem_port.to_string_lossy().to_string(),
                cid: l.cid,
                apn: l.apn.clone(),
                iface: l.iface.clone(),
                restore_cid_path: restore_cid_base
                    .map(|b| format!("{}-{}", b.display(), l.index))
                    .unwrap_or_default(),
                status_port: l.status_port,
                control_port: l.control_port,
                sip_leg_port: l.sip_leg_port,
                netns: l.netns.clone(),
                veth_carrier_addr: l.veth_carrier_addr.clone(),
                veth_telephony_addr: l.veth_telephony_addr.clone(),
                pcscf: l.pcscf.clone().unwrap_or_default(),
                msisdn: l.msisdn.clone().unwrap_or_default(),
            })
            .collect(),
    };
    let json = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    std::fs::write(manifest_path(), json).map_err(|e| e.to_string())
}

/// Reads a [`VolteLineManifest`] back from disk (used by `volte-status`).
pub fn read_manifest(path: &Path) -> Result<VolteLineManifest, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    serde_json::from_str(&contents).map_err(|e| format!("failed to parse {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modem(card_id: &str, port: &str, sim: Option<SimStatus>) -> ProbedModem {
        ProbedModem {
            card_id: card_id.to_string(),
            model: "EC20",
            usb_serial: card_id.to_string(),
            has_audio_capability: true,
            audio_device: None,
            net_device: None,
            at_port: sim.as_ref().map(|_| PathBuf::from(port)),
            sim_status: sim,
        }
    }

    fn ready(card_id: &str, port: &str) -> ProbedModem {
        modem(card_id, port, Some(SimStatus::Ready { imsi: "1".into() }))
    }

    fn base() -> VolteConfig {
        VolteConfig {
            enabled: true,
            bridge_inbound: true,
            ..VolteConfig::default()
        }
    }

    #[test]
    fn single_line_gets_the_line_zero_port_trio() {
        let modems = vec![ready("ec20-AAAAAA", "/dev/ttyUSB0")];
        let result = resolve_volte_lines(&modems, &base());
        assert_eq!(result.lines.len(), 1);
        let l = &result.lines[0];
        assert_eq!(l.index, 0);
        assert_eq!(l.sip_leg_port, LOOPBACK_SIP_PORT);
        assert_eq!(l.control_port, LOOPBACK_CONTROL_PORT);
        assert_eq!(l.status_port, LOOPBACK_STATUS_PORT);
        assert_eq!(l.cid, base().cid);
        assert_eq!(l.modem_port, PathBuf::from("/dev/ttyUSB0"));
    }

    #[test]
    fn lines_are_ordered_by_card_id_not_input_order() {
        let modems = vec![
            ready("ec20-ZZZZZZ", "/dev/ttyUSB1"),
            ready("ec20-AAAAAA", "/dev/ttyUSB0"),
        ];
        let result = resolve_volte_lines(&modems, &base());
        assert_eq!(result.lines.len(), 2);
        assert_eq!(result.lines[0].card_id, "ec20-AAAAAA");
        assert_eq!(result.lines[1].card_id, "ec20-ZZZZZZ");
    }

    #[test]
    fn multiple_lines_derive_distinct_non_overlapping_port_trios() {
        let modems: Vec<ProbedModem> = (0..4)
            .map(|i| ready(&format!("ec20-{i:06}"), &format!("/dev/ttyUSB{i}")))
            .collect();
        let result = resolve_volte_lines(&modems, &base());
        assert_eq!(result.lines.len(), 4);
        let mut all_ports: Vec<u16> = Vec::new();
        for l in &result.lines {
            all_ports.push(l.sip_leg_port);
            all_ports.push(l.control_port);
            all_ports.push(l.status_port);
        }
        let mut unique = all_ports.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), all_ports.len(), "every port must be unique");
        // Line 1 sits a full stride above line 0.
        assert_eq!(
            result.lines[1].sip_leg_port,
            LOOPBACK_SIP_PORT + LINE_PORT_STRIDE
        );
    }

    #[test]
    fn bounds_at_max_lines_and_reports_the_overflow() {
        let mut b = base();
        b.max_lines = 2;
        let modems: Vec<ProbedModem> = (0..4)
            .map(|i| ready(&format!("ec20-{i:06}"), &format!("/dev/ttyUSB{i}")))
            .collect();
        let result = resolve_volte_lines(&modems, &b);
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
    fn unusable_sims_are_reported_and_skipped() {
        let modems = vec![
            ready("ec20-AAAAAA", "/dev/ttyUSB0"),
            modem("ec20-BBBBBB", "/dev/ttyUSB1", Some(SimStatus::Locked)),
            modem("ec20-CCCCCC", "/dev/ttyUSB2", Some(SimStatus::Absent)),
            modem("ec20-DDDDDD", "/dev/ttyUSB3", None),
        ];
        let result = resolve_volte_lines(&modems, &base());
        assert_eq!(result.lines.len(), 1);
        assert_eq!(result.lines[0].card_id, "ec20-AAAAAA");
        assert!(result
            .failed
            .iter()
            .any(|f| f.card_id == "ec20-BBBBBB" && f.reason == "sim_locked"));
        assert!(result
            .failed
            .iter()
            .any(|f| f.card_id == "ec20-CCCCCC" && f.reason == "sim_absent"));
        assert!(result
            .failed
            .iter()
            .any(|f| f.card_id == "ec20-DDDDDD" && f.reason == "no_at_port"));
    }

    #[test]
    fn override_fixes_cid_pcscf_and_iface_for_the_matched_modem() {
        let mut b = base();
        b.line_overrides = vec![VolteLineOverride {
            modem_serial: Some("ec20-AAAAAA".to_string()),
            cid: Some(5),
            pcscf: Some("2400:5200:a100:819::6".to_string()),
            iface: Some("wwan7".to_string()),
            msisdn: Some("919000000001".to_string()),
            ..Default::default()
        }];
        let modems = vec![ready("ec20-AAAAAA", "/dev/ttyUSB0")];
        let result = resolve_volte_lines(&modems, &b);
        let l = &result.lines[0];
        assert_eq!(l.cid, 5);
        assert_eq!(l.pcscf.as_deref(), Some("2400:5200:a100:819::6"));
        assert_eq!(l.iface, "wwan7");
        assert_eq!(l.msisdn.as_deref(), Some("919000000001"));
    }

    #[test]
    fn iface_auto_detected_from_the_modem_when_not_overridden() {
        let mut m = ready("ec20-AAAAAA", "/dev/ttyUSB0");
        m.net_device = Some("wwan0".to_string());
        let result = resolve_volte_lines(&[m], &base());
        assert_eq!(result.lines[0].iface, "wwan0");
    }

    #[test]
    fn index_zero_keeps_the_unindexed_netns_and_veth_defaults() {
        let modems = vec![ready("ec20-AAAAAA", "/dev/ttyUSB0")];
        let result = resolve_volte_lines(&modems, &base());
        let l = &result.lines[0];
        assert_eq!(l.netns, base().netns);
        assert_eq!(l.veth_carrier_iface, base().veth_carrier_iface);
        assert_eq!(l.veth_telephony_iface, base().veth_telephony_iface);
        assert_eq!(l.veth_carrier_addr, base().veth_carrier_addr);
        assert_eq!(l.veth_telephony_addr, base().veth_telephony_addr);
    }

    #[test]
    fn later_lines_derive_distinct_netns_and_veth_identifiers() {
        let modems: Vec<ProbedModem> = (0..3)
            .map(|i| ready(&format!("ec20-{i:06}"), &format!("/dev/ttyUSB{i}")))
            .collect();
        let result = resolve_volte_lines(&modems, &base());
        assert_eq!(result.lines.len(), 3);

        let mut netns: Vec<&str> = result.lines.iter().map(|l| l.netns.as_str()).collect();
        netns.sort_unstable();
        netns.dedup();
        assert_eq!(netns.len(), 3, "every line's netns must be distinct");

        let mut carrier_addrs: Vec<&str> = result
            .lines
            .iter()
            .map(|l| l.veth_carrier_addr.as_str())
            .collect();
        carrier_addrs.sort_unstable();
        carrier_addrs.dedup();
        assert_eq!(carrier_addrs.len(), 3);

        assert_eq!(result.lines[1].netns, format!("{}1", base().netns));
        assert_eq!(result.lines[2].netns, format!("{}2", base().netns));
        // Each line's own carrier/telephony veth addresses must differ from
        // each other too, not just across lines.
        assert_ne!(
            result.lines[0].veth_carrier_addr,
            result.lines[0].veth_telephony_addr
        );
    }

    /// specs/020-volte-line-netns FR-004a: a VoLTE line's derived namespace
    /// must never equal a VoWiFi line's at the same index — both subsystems
    /// can run in the same container.
    #[test]
    fn derived_netns_never_collides_with_vowifi_at_the_same_index() {
        use crate::config::VowifiConfig;
        use crate::vowifi::discovery::resolve_lines as resolve_vowifi_lines;

        let volte_modems: Vec<ProbedModem> = (0..3)
            .map(|i| ready(&format!("volte-{i:06}"), &format!("/dev/ttyUSB{i}")))
            .collect();
        let volte_lines = resolve_volte_lines(&volte_modems, &base()).lines;

        let vowifi_modem = crate::modules::discovery::ProbedModem {
            card_id: "vowifi-000000".to_string(),
            model: "EC20",
            usb_serial: "vowifi-000000".to_string(),
            has_audio_capability: false,
            audio_device: None,
            net_device: None,
            at_port: Some(PathBuf::from("/dev/ttyUSB9")),
            sim_status: Some(SimStatus::Ready {
                imsi: "1".to_string(),
            }),
        };
        let vowifi_assignment = crate::vowifi::discovery::RoleAssignment {
            circuit_switched: Vec::new(),
            vowifi: vec![vowifi_modem; 3],
        };
        let vowifi_lines = resolve_vowifi_lines(&vowifi_assignment, &VowifiConfig::default()).lines;

        for v in &volte_lines {
            for w in &vowifi_lines {
                if v.index == w.index {
                    assert_ne!(
                        v.netns, w.config.netns,
                        "volte line {} and vowifi line {} share a namespace name",
                        v.index, w.index
                    );
                }
            }
        }
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let manifest = VolteLineManifest {
            lines: vec![VolteLineManifestEntry {
                index: 0,
                card_id: "ec20-AAAAAA".to_string(),
                modem_port: "/dev/ttyUSB0".to_string(),
                cid: 3,
                apn: "ims".to_string(),
                iface: "wwan0".to_string(),
                restore_cid_path: "/run/volte-restore-cid-0".to_string(),
                status_port: LOOPBACK_STATUS_PORT,
                control_port: LOOPBACK_CONTROL_PORT,
                sip_leg_port: LOOPBACK_SIP_PORT,
                netns: "volte".to_string(),
                veth_carrier_addr: "10.98.0.1".to_string(),
                veth_telephony_addr: "10.98.0.2".to_string(),
                pcscf: String::new(),
                msisdn: String::new(),
            }],
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: VolteLineManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, manifest);
    }

    /// Regression test for two bugs found live (specs/020-volte-line-netns,
    /// the second flagged in PR #8 review): the manifest silently dropped
    /// `apn` and `msisdn`.
    ///
    /// Dropping `apn` made `volte-carrier-agent --line N` request an
    /// *empty* APN, so the network assigned its general-purpose bearer
    /// (`www.mnc043.mcc404.gprs`) instead of the requested dedicated IMS one
    /// (`ims.mnc043.mcc404.gprs`) — attach still "succeeded" and the
    /// interface still got configured, but the P-CSCF was unreachable from
    /// that bearer.
    ///
    /// Dropping `msisdn` made both split processes (`volte-carrier-agent`
    /// and `volte-bridge`, which only ever see a line via this manifest)
    /// reconstruct it with `msisdn: None`, so IMS registration fell back to
    /// the IMSI-derived public identity instead of the explicitly
    /// configured one.
    ///
    /// Deliberately a single test: this is the only test in this module
    /// that mutates the process-global `MANIFEST_PATH_ENV`, and Rust runs
    /// unit tests in parallel threads by default — a second such test raced
    /// this one under CI's higher parallelism (Rust runs unit tests in
    /// parallel threads within one process, and `std::env::set_var` is
    /// unsynchronized process-global state), causing an intermittent
    /// "No such file or directory" failure. `write_manifest` must carry a
    /// non-default `apn`/`msisdn` through.
    #[test]
    fn write_manifest_preserves_non_default_apn_and_msisdn() {
        let dir = tempfile_dir_for_test();
        let path = dir.join("volte-lines-apn-msisdn-test.json");
        std::env::set_var(MANIFEST_PATH_ENV, &path);

        let line = ResolvedVolteLine {
            index: 0,
            card_id: "ec20-AAAAAA".to_string(),
            modem_port: PathBuf::from("/dev/ttyUSB0"),
            cid: 3,
            apn: "ims".to_string(),
            pcscf: None,
            iface: "wwan0".to_string(),
            msisdn: Some("919000000001".to_string()),
            sip_leg_port: LOOPBACK_SIP_PORT,
            control_port: LOOPBACK_CONTROL_PORT,
            status_port: LOOPBACK_STATUS_PORT,
            netns: "volte".to_string(),
            veth_carrier_iface: "veth-volte-ims".to_string(),
            veth_telephony_iface: "veth-volte-sip".to_string(),
            veth_carrier_addr: "10.98.0.1".to_string(),
            veth_telephony_addr: "10.98.0.2".to_string(),
        };
        write_manifest(&[line], None).expect("write_manifest must succeed");
        let manifest = read_manifest(&path).expect("must read back");

        assert_eq!(manifest.lines[0].apn, "ims", "apn must not be dropped");
        assert_eq!(
            manifest.lines[0].msisdn, "919000000001",
            "msisdn must not be dropped"
        );
        std::env::remove_var(MANIFEST_PATH_ENV);
    }

    /// Minimal tempdir helper so this one test doesn't need the `tempfile`
    /// dev-dependency this module (a `src/` file) doesn't otherwise pull in.
    fn tempfile_dir_for_test() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gsm-sip-bridge-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
