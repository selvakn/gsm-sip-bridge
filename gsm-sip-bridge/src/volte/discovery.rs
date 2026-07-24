//! Multi-modem VoLTE line resolution (specs/018-volte-multi-modem): which
//! discovered modems become host-side LTE bridge lines, and each line's
//! per-line-derived loopback ports and PDN/P-CSCF settings. The LTE
//! counterpart to [`crate::vowifi::discovery`], but far smaller.
//!
//! The LTE path has **no network namespace** (specs/015 research R4), so a
//! line needs none of VoWiFi's netns/veth/XFRM/vpcd isolation — its two
//! halves are threads in one process joined over loopback. The only thing
//! that must differ per line is therefore its **loopback port trio** (the
//! carrier half's status listener, the carrier↔telephony leg, and the
//! control channel), which this module derives as a function of the line
//! index, exactly the way [`crate::vowifi::discovery`] derives veth
//! addresses and vpcd ports.
//!
//! Resolution is a pure function over [`ProbedModem`]s (from the shared
//! [`crate::modules::discovery`] scan) and the `[volte]` base config, so the
//! whole role/port/settings derivation is unit-testable without a modem.

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
    pub iface: String,
    /// File this line's `attach` recorded its displaced context id in, so
    /// cleanup can `volte-pdn down --restore-cid` it.
    pub restore_cid_path: String,
    pub status_port: u16,
    pub control_port: u16,
    pub sip_leg_port: u16,
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
    fn manifest_round_trips_through_json() {
        let manifest = VolteLineManifest {
            lines: vec![VolteLineManifestEntry {
                index: 0,
                card_id: "ec20-AAAAAA".to_string(),
                modem_port: "/dev/ttyUSB0".to_string(),
                cid: 3,
                iface: "wwan0".to_string(),
                restore_cid_path: "/run/volte-restore-cid-0".to_string(),
                status_port: LOOPBACK_STATUS_PORT,
                control_port: LOOPBACK_CONTROL_PORT,
                sip_leg_port: LOOPBACK_SIP_PORT,
            }],
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: VolteLineManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, manifest);
    }
}
