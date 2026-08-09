//! The shared modem inventory scan: walk USB, recognize known Quectel
//! modules, find each one's AT-capable serial interface, read its SIM, and
//! hand the result to whichever subsystem claims it.
//!
//! This file is the facade and the scan itself; the layers underneath it are
//! split by concern:
//!
//! | Concern | Module |
//! |---|---|
//! | Which USB VID/PIDs we recognize | [`catalog`] |
//! | Walking sysfs for tty/ALSA/net devices | [`sysfs`] |
//! | Blocklist, quarantine, the abandonable bounded worker | [`policy`] |
//! | Finding the interface that answers `AT` | [`probe`] |
//! | Reading SIM identity, and the one optional repair | [`sim`] |
//! | Which modems another subsystem already owns | [`claims`] |
//!
//! The names other modules have always used — `discovery::ProbedModem`,
//! `discovery::SimStatus`, `discovery::DiscoveryPolicy` and so on — are
//! re-exported here so no caller moved when this became a directory.

mod catalog;
mod claims;
mod policy;
mod probe;
mod sim;
mod sysfs;
#[cfg(test)]
mod test_support;

use crate::error::BridgeResult;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use catalog::match_known_device;
use claims::{
    active_volte_card_ids, active_volte_line_ports, active_vowifi_card_ids,
    excluded_ports_from_lines_file,
};
use sysfs::{find_alsa_card, find_net_iface, read_sysfs_attr};

pub use claims::{
    lines_file_path, volte_claimed_card_ids, volte_claimed_ports, DEFAULT_LINES_FILE,
    LINES_FILE_ENV,
};
pub use policy::DiscoveryPolicy;
pub use sim::{SimRecovery, SimStatus};

#[derive(Debug, Clone)]
pub struct DiscoveredModule {
    pub id: String,
    pub serial_port: PathBuf,
    pub audio_device: String,
    pub usb_serial: String,
}

pub fn derive_module_id(identifier: &str) -> String {
    let clean: String = identifier.chars().filter(|c| c.is_alphanumeric()).collect();
    let suffix = if clean.len() >= 6 {
        &clean[clean.len() - 6..]
    } else {
        &clean
    };
    format!("ec20-{}", suffix.to_ascii_uppercase())
}

/// Every USB-recognized modem, probed for its AT-capable interface and SIM
/// identity, before any circuit-switched/VoWiFi role assignment
/// (specs/013-multi-card-vowifi's shared inventory scan, research.md item
/// 1). `scan_modules` (below) narrows this down to the audio-capable subset
/// for the circuit-switched pool — unchanged behavior from before this
/// feature. `vowifi::discovery` narrows it down the other way for VoWiFi
/// lines.
#[derive(Debug, Clone)]
pub struct ProbedModem {
    pub card_id: String,
    pub model: &'static str,
    pub usb_serial: String,
    pub has_audio_capability: bool,
    pub audio_device: Option<String>,
    /// Host network interface this modem exposes for its data path (e.g. a
    /// QMI/ECM `wwan*`/`enx*` device), if one is enumerated. Used by the
    /// host-side LTE bridge to bind each line's IMS PDN to its own modem's
    /// interface (specs/018-volte-multi-modem); irrelevant to VoWiFi, which
    /// carries its data over the ePDG tunnel, not the modem's netdev.
    pub net_device: Option<String>,
    pub at_port: Option<PathBuf>,
    /// `None` only when `at_port` is `None` too — there was nothing to ask.
    pub sim_status: Option<SimStatus>,
}

/// The shared inventory scan: walks the USB bus, recognizes every known
/// modem (audio-capable or not, FR-003), probes each one's serial
/// interfaces for a live AT response instead of assuming a fixed interface
/// number (FR-002), and reads SIM identity/readiness for any modem that
/// answers (FR-004/FR-006). Both `scan_modules` (circuit-switched) and
/// `vowifi::discovery`'s role assignment are built on top of this.
///
/// Always a clean, unbiased probe of every recognized device — deliberately
/// does NOT consult "which modems does an existing line-resolution file
/// already claim" (see `scan_modules`'s different treatment of that
/// question): this is also what `gsm-sip-bridge discover` itself calls, and
/// a `docker restart` (same container, same `/tmp`) can leave a stale
/// resolution file from the *previous* run on disk — `discover` re-probing
/// everything fresh regardless of that stale content is correct; treating
/// it as "already claimed" would make discovery refuse to ever re-find its
/// own line after a restart.
pub fn scan_all() -> BridgeResult<Vec<ProbedModem>> {
    scan_all_preferring(&[])
}

/// Like `scan_all`, but when a device exposes *several* AT-capable serial
/// interfaces (real hardware: an EC200 was found live-testing to answer AT
/// on more than one `ttyUSB*`, e.g. both a primary and a diagnostic port),
/// and one of `preferred_ports` is among that device's candidates, that one
/// is used instead of whichever candidate the plain first-match probe would
/// otherwise settle on. Without this, an operator's `[vowifi].modem_port`/
/// `[[vowifi.line]]` override naming a *working but non-first* AT port on a
/// multi-port modem would silently fail to match `ProbedModem.at_port`
/// (found live-testing) — defeating "that port is used as-is"
/// (FR-009/FR-020, acceptance scenario 5). `main.rs`'s `discover` handler
/// passes `vowifi::discovery::effective_line_overrides`' configured ports
/// here; a plain `scan_all()` (no hints) behaves exactly as before.
pub fn scan_all_preferring(preferred_ports: &[PathBuf]) -> BridgeResult<Vec<ProbedModem>> {
    // A one-shot scan with no operator config: the bounded probe and quarantine
    // still protect it (default timeout), but there is no persistent state to
    // carry, so a transient unfiltered policy is right.
    scan_all_inner(
        preferred_ports,
        &HashSet::new(),
        SimRecovery::Disabled,
        &mut DiscoveryPolicy::unfiltered(),
    )
}

/// [`scan_all_preferring`] for the one-shot startup scan behind
/// `gsm-sip-bridge discover`, which may additionally attempt SIM recovery —
/// see [`SimRecovery`] for why every other caller must not.
pub fn scan_all_preferring_with_sim_recovery(
    preferred_ports: &[PathBuf],
    sim_recovery: SimRecovery,
    policy: &mut DiscoveryPolicy,
) -> BridgeResult<Vec<ProbedModem>> {
    scan_all_inner(preferred_ports, &HashSet::new(), sim_recovery, policy)
}

/// Like [`scan_all_preferring`], but honoring an operator [`DiscoveryPolicy`]
/// (its `[discovery].excluded_ports` blocklist and probe timeout). The VoLTE
/// startup discovery path uses this so a known-bad port is skipped on the
/// container-start scan that resolves the line table — not just on the
/// circuit-switched rescans (specs/030-bad-port-isolation FR-007/FR-010,
/// SC-003).
pub fn scan_all_preferring_with_policy(
    preferred_ports: &[PathBuf],
    policy: &mut DiscoveryPolicy,
) -> BridgeResult<Vec<ProbedModem>> {
    scan_all_inner(
        preferred_ports,
        &HashSet::new(),
        SimRecovery::Disabled,
        policy,
    )
}

/// Shared implementation. `skip_card_ids` are devices whose serial ports
/// must not be opened at all this call — not merely omitted from the
/// result afterward — because something else already has them open. Only
/// `scan_modules` passes a non-empty set (see `active_vowifi_card_ids`'s
/// doc comment for why `scan_all`/`scan_all_preferring` never do): its
/// *ongoing* rescans run for the container's entire lifetime, concurrently
/// with already-running `vowifi-usim-bridge`/agent processes, and probing
/// (opening + sending `AT`) a port those processes are mid-transaction on
/// was observed live to intermittently disrupt them (`AT+CPIN?: no status
/// in response` on the *already-registered* line's own port) — the
/// "modem claimed by both subsystems" hazard the spec's edge cases warn
/// about, just manifesting after startup instead of at it.
///
/// `sim_recovery` gates the one effect in here that writes to a modem
/// rather than reading from it — see [`SimRecovery`].
fn scan_all_inner(
    preferred_ports: &[PathBuf],
    skip_card_ids: &HashSet<String>,
    sim_recovery: SimRecovery,
    policy: &mut DiscoveryPolicy,
) -> BridgeResult<Vec<ProbedModem>> {
    let mut modems = Vec::new();

    let usb_devices = Path::new("/sys/bus/usb/devices");
    let entries = match fs::read_dir(usb_devices) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read /sys/bus/usb/devices");
            return Ok(modems);
        }
    };

    for entry in entries.flatten() {
        let dev_path = entry.path();
        let Some(device) = match_known_device(&dev_path) else {
            continue;
        };
        let usb_name = entry.file_name().to_string_lossy().to_string();

        let serial = read_sysfs_attr(&dev_path, "serial").unwrap_or_default();
        let identifier = if serial.is_empty() {
            usb_name.clone()
        } else {
            serial.clone()
        };
        let card_id = derive_module_id(&identifier);

        if skip_card_ids.contains(&card_id) {
            tracing::debug!(
                module_id = %card_id,
                model = device.model,
                usb_path = %usb_name,
                "modem already claimed by an active VoWiFi line; not re-probing its serial ports"
            );
            modems.push(ProbedModem {
                card_id,
                model: device.model,
                usb_serial: serial,
                has_audio_capability: device.has_audio_capability,
                audio_device: find_alsa_card(&dev_path),
                net_device: find_net_iface(&dev_path),
                at_port: None,
                sim_status: None,
            });
            continue;
        }

        let audio_device = find_alsa_card(&dev_path);
        let net_device = find_net_iface(&dev_path);
        let at_candidate = probe::probe_at_port(&dev_path, preferred_ports, policy);
        let sim_status = at_candidate
            .as_ref()
            .map(|c| sim::probe_sim_status_at(&c.device_path, &c.iface_path, sim_recovery, policy));
        let at_port = at_candidate.map(|c| c.device_path);

        match (&at_port, &sim_status) {
            (Some(port), Some(SimStatus::Ready { imsi })) => {
                tracing::info!(
                    module_id = %card_id,
                    model = device.model,
                    usb_path = %usb_name,
                    serial_port = %port.display(),
                    imsi = %imsi,
                    has_audio_capability = device.has_audio_capability,
                    "discovered modem"
                );
            }
            (Some(port), Some(reason)) => {
                tracing::warn!(
                    module_id = %card_id,
                    model = device.model,
                    usb_path = %usb_name,
                    serial_port = %port.display(),
                    reason = ?reason,
                    "modem's SIM is not usable; excluded from line/card tables"
                );
            }
            _ => {
                tracing::warn!(
                    module_id = %card_id,
                    model = device.model,
                    usb_path = %usb_name,
                    "no AT-capable interface found among this modem's serial ports"
                );
            }
        }

        modems.push(ProbedModem {
            card_id,
            model: device.model,
            usb_serial: serial,
            has_audio_capability: device.has_audio_capability,
            audio_device,
            net_device,
            at_port,
            sim_status,
        });
    }

    Ok(modems)
}

/// The circuit-switched pool's view of `scan_all`: audio-capable modems
/// only (today's exact behavior, FR-021 — VoWiFi-only models were always
/// excluded here), minus any modem a resolved VoWiFi line table has already
/// claimed (FR-007, read from the line-resolution file
/// `vowifi::discovery::DEFAULT_LINES_FILE` writes — see
/// `excluded_ports_from_lines_file`). A missing/unparsable resolution file
/// excludes nothing, so a fleet that never runs `discover` (VoWiFi
/// permanently disabled) behaves exactly as before this feature.
pub fn scan_modules() -> BridgeResult<Vec<DiscoveredModule>> {
    scan_modules_excluding(&[])
}

/// [`scan_modules`] with an explicit extra exclusion set, so the caller can
/// state which ports another subsystem owns rather than this module guessing.
pub fn scan_modules_excluding(also_excluded: &[PathBuf]) -> BridgeResult<Vec<DiscoveredModule>> {
    scan_modules_excluding_cards(also_excluded, &[], &mut DiscoveryPolicy::unfiltered())
}

/// Like [`scan_modules_excluding`], but the caller can additionally name card
/// ids to skip probing entirely (not merely filter out afterward) — the
/// robust form of "a modem belongs to exactly one subsystem" (FR-034) for a
/// modem that answers `AT` on several ports. Used to keep the host-side LTE
/// bridge's serial-pinned modems out of the circuit-switched pool.
pub fn scan_modules_excluding_cards(
    also_excluded: &[PathBuf],
    also_skip_cards: &[String],
    policy: &mut DiscoveryPolicy,
) -> BridgeResult<Vec<DiscoveredModule>> {
    let mut excluded = excluded_ports_from_lines_file();
    excluded.extend(active_volte_line_ports());
    excluded.extend(also_excluded.iter().cloned());
    // Skips re-probing any modem an active VoWiFi line, an *auto-discovered*
    // VoLTE line (specs/020-volte-line-netns — read from the manifest
    // `volte-discover-lines` writes, the same way `active_vowifi_card_ids`
    // reads VoWiFi's own line-resolution file), or a serial-pinned VoLTE line
    // already owns — not just filtering it out afterward (see
    // `scan_all_inner`'s doc comment).
    //
    // This closes a real contention hazard found live: a serial-pinned
    // `[[volte.line]]` override alone is not enough when a modem answers `AT`
    // on several `ttyUSB` interfaces (the very case `volte_claimed_card_ids`'s
    // own doc comment already warns about) — the circuit-switched daemon's
    // periodic re-scan can settle on a *different* port than the one pinned,
    // so a port-string exclusion misses it and both subsystems' AT traffic
    // interleaves on the same physical SIM (observed as intermittent
    // `AT+CIMI`/`AT+CPIN` failures on the VoLTE side). Reading the *resolved*
    // card id back from the manifest — the identity the modem actually probed
    // as, not the identity a config override guessed — closes that gap the
    // same way `active_vowifi_card_ids` already closes it for VoWiFi.
    let mut skip = active_vowifi_card_ids();
    skip.extend(active_volte_card_ids());
    skip.extend(also_skip_cards.iter().cloned());
    // `SimRecovery::Disabled`: this is the ongoing-rescan path, which must
    // stay a read-only probe — see `SimRecovery`.
    let modems = scan_all_inner(&[], &skip, SimRecovery::Disabled, policy)?;
    Ok(modems
        .into_iter()
        .filter(|m| m.has_audio_capability)
        .filter_map(|m| {
            let serial_port = m.at_port?;
            if excluded.contains(&serial_port) {
                tracing::info!(
                    module_id = %m.card_id,
                    serial_port = %serial_port.display(),
                    "modem claimed by another subsystem; excluded from the circuit-switched pool"
                );
                return None;
            }
            Some(DiscoveredModule {
                id: m.card_id,
                serial_port,
                audio_device: m.audio_device.unwrap_or_default(),
                usb_serial: m.usb_serial,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    /// The ongoing-rescan path must never power-cycle a radio: it runs for
    /// the container's whole lifetime next to modems that may be registered
    /// or mid-call, and `skip_card_ids` only shields *VoWiFi* lines, not
    /// circuit-switched ones. Encoded as a call-site assertion because the
    /// effect itself (`AT+CFUN`) needs real hardware to observe — what is
    /// checkable here is that the rescan entry point still asks for
    /// `Disabled`, which is what keeps `probe_sim_status_at` read-only.
    ///
    /// Scans this file specifically: every `scan_all_inner` call site lives
    /// here in the facade, and `scan_all_inner` itself is private to it.
    #[test]
    fn only_the_one_shot_discover_scan_opts_into_sim_recovery() {
        // Flattened so the check survives rustfmt reflowing argument lists.
        let src: String = include_str!("mod.rs")
            .lines()
            .map(str::trim)
            .collect::<Vec<_>>()
            .join(" ");
        // Every `scan_all_inner(...)` call in this module — the rescan path
        // and the `scan_all_preferring` family — must pass `Disabled`; only
        // the caller-supplied parameter may ever carry the opt-in.
        for call in src.split("scan_all_inner(").skip(1) {
            let args = call.split(')').next().unwrap_or_default();
            assert!(
                !args.contains("CfunCycleOnUnreadable"),
                "a scan_all_inner call site in this module hard-codes SIM \
                 recovery; it must stay Disabled here: {args}"
            );
        }

        let callers: Vec<&str> = include_str!("../../commands/discover.rs")
            .lines()
            .filter(|l| l.contains("SimRecovery::"))
            .collect();
        assert_eq!(
            callers.len(),
            1,
            "the one-shot discover scan is the single intended opt-in, got {callers:?}"
        );
        assert!(callers[0].contains("CfunCycleOnUnreadable"));
    }
}
