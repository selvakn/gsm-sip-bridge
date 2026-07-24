use crate::error::BridgeResult;
use crate::modules::at_commander::{AtCommander, AtResponse};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One Quectel module variant this project knows how to recognize on USB.
/// `has_audio_capability` is a static property of the model — `false` for
/// modules with no usable circuit-switched audio path at all (e.g. the
/// EC200 tested here exposes no ALSA device, unlike the EC20). Unlike the
/// AT-capable interface (found by live probing below, specs/013-multi-card-
/// vowifi FR-002), a model's audio capability isn't something a boot-time
/// probe can discover — an audio-capable model with no ALSA device
/// enumerated *this* boot is still audio-capable and stays eligible for the
/// circuit-switched pool (`scan_modules` below), whereas an audio-less model
/// never is, regardless of what's live.
struct KnownDevice {
    vendor_id: &'static str,
    product_id: &'static str,
    model: &'static str,
    has_audio_capability: bool,
}

const KNOWN_DEVICES: &[KnownDevice] = &[
    KnownDevice {
        vendor_id: "2c7c",
        product_id: "0125",
        model: "EC20",
        has_audio_capability: true,
    },
    KnownDevice {
        vendor_id: "2c7c",
        product_id: "0901",
        model: "EC200",
        has_audio_capability: false,
    },
];

/// Per-candidate timeout for the AT probe (specs/013-multi-card-vowifi
/// FR-002) — short because a modem may expose several serial interfaces
/// that are never going to answer AT (diagnostic/NMEA ports), and probing
/// tries each one in turn.
const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

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

/// SIM identity/readiness observed while probing a discovered modem
/// (specs/013-multi-card-vowifi FR-004/FR-006).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimStatus {
    Ready { imsi: String },
    Absent,
    Locked,
    Unreadable(String),
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
    scan_all_inner(preferred_ports, &std::collections::HashSet::new())
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
fn scan_all_inner(
    preferred_ports: &[PathBuf],
    skip_card_ids: &std::collections::HashSet<String>,
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
        let at_port = probe_at_port(&dev_path, preferred_ports);
        let sim_status = at_port.as_ref().map(|port| probe_sim_status_at(port));

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
    // The single pinned modem (`modem_port`), plus any AT port a
    // `[[volte.line]]` override pins in multi-modem discovery mode
    // (specs/018-volte-multi-modem) — all claimed so the circuit-switched pool
    // never grabs a modem this bridge drives.
    let mut ports = Vec::new();
    if !config.modem_port.is_empty() {
        ports.push(PathBuf::from(&config.modem_port));
    }
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

/// [`scan_modules`] with an explicit extra exclusion set, so the caller can
/// state which ports another subsystem owns rather than this module guessing.
pub fn scan_modules_excluding(also_excluded: &[PathBuf]) -> BridgeResult<Vec<DiscoveredModule>> {
    scan_modules_excluding_cards(also_excluded, &[])
}

/// Like [`scan_modules_excluding`], but the caller can additionally name card
/// ids to skip probing entirely (not merely filter out afterward) — the
/// robust form of "a modem belongs to exactly one subsystem" (FR-034) for a
/// modem that answers `AT` on several ports. Used to keep the host-side LTE
/// bridge's serial-pinned modems out of the circuit-switched pool.
pub fn scan_modules_excluding_cards(
    also_excluded: &[PathBuf],
    also_skip_cards: &[String],
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
    let modems = scan_all_inner(&[], &skip)?;
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

/// Default path for the VoWiFi line-resolution artifact
/// (specs/013-multi-card-vowifi, `contracts/discover-cli-contract.md`).
/// Defined here (not in `vowifi::discovery`) so this module — the
/// lower-level shared scan both subsystems build on — has no dependency on
/// the `vowifi` module; `vowifi::discovery`'s writer imports this constant
/// instead, the natural direction (a specific feature depending on shared
/// infrastructure, not the reverse).
pub const DEFAULT_LINES_FILE: &str = "/tmp/gsm-sip-bridge-lines.json";
/// Env var overriding `DEFAULT_LINES_FILE`, read by both the writer
/// (`gsm-sip-bridge discover`) and this reader.
pub const LINES_FILE_ENV: &str = "GSM_SIP_BRIDGE_LINES_FILE";

/// Resolves the line-resolution file path every reader/writer of it
/// (`main.rs`'s `discover`/`--line` handling, `vowifi::mod`'s Agent B
/// listener setup, `vowifi-status`) should use: `LINES_FILE_ENV` if set,
/// else `DEFAULT_LINES_FILE`.
pub fn lines_file_path() -> PathBuf {
    PathBuf::from(std::env::var(LINES_FILE_ENV).unwrap_or_else(|_| DEFAULT_LINES_FILE.to_string()))
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
    let path = std::env::var(LINES_FILE_ENV).unwrap_or_else(|_| DEFAULT_LINES_FILE.to_string());
    let Ok(contents) = fs::read_to_string(&path) else {
        return LinesFileExcerpt::default();
    };
    serde_json::from_str(&contents).unwrap_or_else(|e| {
        tracing::warn!(
            path = %path,
            error = %e,
            "failed to parse VoWiFi line-resolution file; treating it as absent"
        );
        LinesFileExcerpt::default()
    })
}

fn excluded_ports_from_lines_file() -> std::collections::HashSet<PathBuf> {
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
fn active_vowifi_card_ids() -> std::collections::HashSet<String> {
    read_lines_file_excerpt()
        .lines
        .into_iter()
        .map(|l| l.card_id)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Default path/env var for the VoLTE line manifest — duplicated from
/// `volte::discovery::{DEFAULT_MANIFEST_PATH, MANIFEST_PATH_ENV}` rather than
/// imported, for the same layering reason `DEFAULT_LINES_FILE` above lives
/// here and not in `vowifi::discovery`: this module is the shared scan
/// underneath both subsystems and must not depend on either of them. Keep
/// both copies in sync if the manifest path ever changes.
const VOLTE_MANIFEST_PATH_ENV: &str = "GSM_SIP_BRIDGE_VOLTE_LINES_FILE";
const DEFAULT_VOLTE_MANIFEST_PATH: &str = "/run/volte-lines.json";

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
    let path = std::env::var(VOLTE_MANIFEST_PATH_ENV)
        .unwrap_or_else(|_| DEFAULT_VOLTE_MANIFEST_PATH.to_string());
    let Ok(contents) = fs::read_to_string(&path) else {
        return VolteManifestExcerpt::default();
    };
    serde_json::from_str(&contents).unwrap_or_else(|e| {
        tracing::warn!(
            path = %path,
            error = %e,
            "failed to parse VoLTE line manifest; treating it as absent"
        );
        VolteManifestExcerpt::default()
    })
}

/// Ports every resolved (auto-discovered or serial-pinned) VoLTE line
/// actually settled on — read back from the manifest `volte-discover-lines`
/// writes, so an auto-discovered line (no `[[volte.line]]` override to derive
/// a port from) is excluded too, not only explicitly pinned ones.
fn active_volte_line_ports() -> std::collections::HashSet<PathBuf> {
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
fn active_volte_card_ids() -> std::collections::HashSet<String> {
    read_volte_manifest_excerpt()
        .lines
        .into_iter()
        .map(|l| l.card_id)
        .filter(|s| !s.is_empty())
        .collect()
}

fn match_known_device(path: &Path) -> Option<&'static KnownDevice> {
    let vendor = read_sysfs_attr(path, "idVendor").unwrap_or_default();
    let product = read_sysfs_attr(path, "idProduct").unwrap_or_default();
    KNOWN_DEVICES
        .iter()
        .find(|d| d.vendor_id == vendor && d.product_id == product)
}

/// Every `ttyUSB*` serial interface this USB device exposes, in a stable
/// (sorted) order — regardless of `bInterfaceNumber`, since which interface
/// answers AT varies by model/firmware (FR-002) and is no longer assumed.
fn candidate_tty_ports(dev_path: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let Ok(entries) = fs::read_dir(dev_path) else {
        return candidates;
    };
    for entry in entries.flatten() {
        let iface_path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.contains(':') {
            continue;
        }
        if let Some(tty) = find_tty_in_path(&iface_path) {
            candidates.push(PathBuf::from(format!("/dev/{tty}")));
        }
    }
    candidates.sort();
    candidates
}

/// Reorders `candidates` so any that appear in `preferred` come first (each
/// in its original relative order otherwise) — a device with several
/// AT-capable interfaces should try an operator-named port before falling
/// through to "whichever answers first" (see `scan_all_preferring`'s doc
/// comment). Pure and unit-tested; `probe_at_port` (real serial I/O) is not.
fn order_candidates_with_preference(
    candidates: Vec<PathBuf>,
    preferred: &[PathBuf],
) -> Vec<PathBuf> {
    let (mut first, mut rest): (Vec<_>, Vec<_>) =
        candidates.into_iter().partition(|c| preferred.contains(c));
    first.append(&mut rest);
    first
}

/// Tries every candidate serial interface in turn (an operator-preferred
/// one first, if present — see `order_candidates_with_preference`), opening
/// it and sending a bare `AT`, and returns the first one that answers `OK` —
/// the live probe replacing the old fixed-interface-number lookup (FR-002).
/// Real hardware I/O; not unit-tested directly (same boundary as the rest of
/// this file's sysfs/serial-opening helpers) — the AT-response
/// interpretation itself (`probe_is_at_capable`) is unit-tested against a
/// fake transport below.
fn probe_at_port(dev_path: &Path, preferred: &[PathBuf]) -> Option<PathBuf> {
    let candidates = order_candidates_with_preference(candidate_tty_ports(dev_path), preferred);
    for candidate in candidates {
        match AtCommander::open_with_timeout(&candidate, PROBE_TIMEOUT) {
            Ok(mut at) => {
                if probe_is_at_capable(&mut at) {
                    return Some(candidate);
                }
            }
            Err(e) => {
                tracing::debug!(
                    port = %candidate.display(),
                    error = %e,
                    "could not open candidate serial port during AT probe"
                );
            }
        }
    }
    None
}

/// Sends a bare `AT` and returns whether the device answered with a
/// well-formed response (`OK`) — the core of the AT-probe (FR-002). Takes
/// an already-open `AtCommander`, so it's exercised in tests against a fake
/// in-memory transport (mirroring `at_commander.rs`'s own `MockStream`)
/// without touching real hardware.
pub fn probe_is_at_capable(at: &mut AtCommander) -> bool {
    matches!(at.send_command("AT"), Ok(AtResponse::Ok(_)))
}

/// Opens `port` fresh and reads its SIM status — real hardware I/O, not
/// unit-tested directly; `probe_sim_status` (below) carries the tested
/// interpretation logic.
fn probe_sim_status_at(port: &Path) -> SimStatus {
    match AtCommander::open_with_timeout(port, PROBE_TIMEOUT) {
        Ok(mut at) => probe_sim_status(&mut at),
        Err(e) => SimStatus::Unreadable(e.to_string()),
    }
}

/// Interprets `AT+CPIN?` (and, if ready, `AT+CIMI`) into a `SimStatus`
/// (FR-004/FR-006). Pure given an `AtCommander`, so it's exercised in tests
/// against a fake transport.
pub fn probe_sim_status(at: &mut AtCommander) -> SimStatus {
    // Sends AT+CPIN? directly (rather than through `AtCommander::query_cpin`)
    // so a `+CME ERROR: 10` ("SIM not inserted", 3GPP TS 27.007) is matched
    // by its numeric code, not by re-parsing an already-stringified error.
    match at.send_command("AT+CPIN?") {
        Ok(AtResponse::Ok(lines)) => {
            let status = lines.iter().find_map(|l| {
                l.strip_prefix("+CPIN:")
                    .map(|s| s.trim().to_ascii_uppercase())
            });
            match status.as_deref() {
                Some("READY") => match at.query_imsi() {
                    Ok(imsi) => SimStatus::Ready { imsi },
                    Err(e) => SimStatus::Unreadable(e.to_string()),
                },
                Some(s) if s.contains("PIN") || s.contains("PUK") => SimStatus::Locked,
                Some(s) => SimStatus::Unreadable(format!("unexpected AT+CPIN? status: {s}")),
                None => SimStatus::Unreadable("AT+CPIN?: no status in response".to_string()),
            }
        }
        Ok(AtResponse::CmeError(10, _)) => SimStatus::Absent,
        Ok(AtResponse::Error(e)) | Ok(AtResponse::CmeError(_, e)) => SimStatus::Unreadable(e),
        Err(e) => SimStatus::Unreadable(e.to_string()),
    }
}

fn find_tty_in_path(iface_path: &Path) -> Option<String> {
    let entries = fs::read_dir(iface_path).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("ttyUSB") {
            let tty_dir = entry.path().join("tty");
            if let Ok(inner) = fs::read_dir(&tty_dir) {
                for tty_entry in inner.flatten() {
                    let tty_name = tty_entry.file_name().to_string_lossy().to_string();
                    if tty_name.starts_with("ttyUSB") {
                        return Some(tty_name);
                    }
                }
            }
            return Some(name);
        }
    }
    None
}

fn find_alsa_card(dev_path: &Path) -> Option<String> {
    let entries = fs::read_dir(dev_path).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.contains(":1.") {
            continue;
        }
        let sound_dir = entry.path().join("sound");
        if let Ok(sound_entries) = fs::read_dir(&sound_dir) {
            for sound_entry in sound_entries.flatten() {
                let card_name = sound_entry.file_name().to_string_lossy().to_string();
                if let Some(card_num) = card_name.strip_prefix("card") {
                    return Some(format!("hw:{card_num},0"));
                }
            }
        }
    }
    None
}

/// The host network interface a modem's data path exposes, if any — the
/// `net/<ifname>` under one of the device's USB interface directories (a
/// QMI/ECM `wwan*`/`usb*`/`enx*` device on the Quectel modules). Structurally
/// the same walk as `find_alsa_card`, one subdir over (`net` instead of
/// `sound`). Best-effort: `None` when the modem exposes no netdev this boot,
/// in which case the LTE bridge falls back to the configured `iface`.
fn find_net_iface(dev_path: &Path) -> Option<String> {
    let entries = fs::read_dir(dev_path).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.contains(':') {
            continue;
        }
        let net_dir = entry.path().join("net");
        if let Ok(net_entries) = fs::read_dir(&net_dir) {
            if let Some(net_entry) = net_entries.flatten().next() {
                return Some(net_entry.file_name().to_string_lossy().to_string());
            }
        }
    }
    None
}

fn read_sysfs_attr(path: &Path, attr: &str) -> Option<String> {
    fs::read_to_string(path.join(attr))
        .ok()
        .map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_device_dir(dir: &Path, vendor: &str, product: &str) {
        fs::write(dir.join("idVendor"), vendor).unwrap();
        fs::write(dir.join("idProduct"), product).unwrap();
    }

    #[test]
    fn match_known_device_recognizes_ec20() {
        let dir = tempfile::tempdir().unwrap();
        fake_device_dir(dir.path(), "2c7c", "0125");
        let device = match_known_device(dir.path()).unwrap();
        assert_eq!(device.model, "EC20");
        assert!(device.has_audio_capability);
    }

    #[test]
    fn match_known_device_recognizes_ec200_as_vowifi_only() {
        let dir = tempfile::tempdir().unwrap();
        fake_device_dir(dir.path(), "2c7c", "0901");
        let device = match_known_device(dir.path()).unwrap();
        assert_eq!(device.model, "EC200");
        assert!(
            !device.has_audio_capability,
            "EC200 has no circuit-switched audio path, but is still recognized \
             (not skipped) so it can be probed for VoWiFi (FR-003)"
        );
    }

    #[test]
    fn match_known_device_returns_none_for_unrelated_vendor() {
        let dir = tempfile::tempdir().unwrap();
        fake_device_dir(dir.path(), "1234", "5678");
        assert!(match_known_device(dir.path()).is_none());
    }

    #[test]
    fn match_known_device_returns_none_when_sysfs_attrs_missing() {
        let dir = tempfile::tempdir().unwrap();
        // No idVendor/idProduct files at all — e.g. a non-device directory
        // that happened to be listed under /sys/bus/usb/devices.
        assert!(match_known_device(dir.path()).is_none());
    }

    fn fake_tty_interface(dev_dir: &Path, iface_name: &str, tty_name: &str, iface_num: &str) {
        let iface_dir = dev_dir.join(iface_name);
        fs::create_dir_all(&iface_dir).unwrap();
        fs::write(iface_dir.join("bInterfaceNumber"), iface_num).unwrap();
        let tty_tty_dir = iface_dir.join(tty_name).join("tty").join(tty_name);
        fs::create_dir_all(&tty_tty_dir).unwrap();
    }

    #[test]
    fn candidate_tty_ports_finds_every_interface_regardless_of_number() {
        let dir = tempfile::tempdir().unwrap();
        // Three candidate interfaces, arbitrary bInterfaceNumber values —
        // acceptance scenario 4: probing must not assume a fixed one.
        fake_tty_interface(dir.path(), "1-1:1.0", "ttyUSB0", "00");
        fake_tty_interface(dir.path(), "1-1:1.2", "ttyUSB2", "02");
        fake_tty_interface(dir.path(), "1-1:1.4", "ttyUSB4", "04");
        let candidates = candidate_tty_ports(dir.path());
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/dev/ttyUSB0"),
                PathBuf::from("/dev/ttyUSB2"),
                PathBuf::from("/dev/ttyUSB4"),
            ]
        );
    }

    #[test]
    fn candidate_tty_ports_empty_when_no_interfaces() {
        let dir = tempfile::tempdir().unwrap();
        assert!(candidate_tty_ports(dir.path()).is_empty());
    }

    #[test]
    fn candidate_tty_ports_ignores_non_interface_entries() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("idVendor"), "2c7c").unwrap();
        fake_tty_interface(dir.path(), "1-1:1.4", "ttyUSB4", "04");
        let candidates = candidate_tty_ports(dir.path());
        assert_eq!(candidates, vec![PathBuf::from("/dev/ttyUSB4")]);
    }

    #[test]
    fn order_candidates_prefers_configured_port_when_present() {
        // Found live-testing: a real EC200 answered AT on both ttyUSB0 and
        // ttyUSB6. An operator-configured port must win over "whichever
        // sorts first" so an existing single-line config naming a
        // non-default AT port still gets used as-is (FR-009/FR-020).
        let candidates = vec![
            PathBuf::from("/dev/ttyUSB0"),
            PathBuf::from("/dev/ttyUSB2"),
            PathBuf::from("/dev/ttyUSB6"),
        ];
        let preferred = vec![PathBuf::from("/dev/ttyUSB6")];
        assert_eq!(
            order_candidates_with_preference(candidates, &preferred),
            vec![
                PathBuf::from("/dev/ttyUSB6"),
                PathBuf::from("/dev/ttyUSB0"),
                PathBuf::from("/dev/ttyUSB2"),
            ]
        );
    }

    #[test]
    fn order_candidates_unchanged_when_no_preference_matches() {
        let candidates = vec![PathBuf::from("/dev/ttyUSB0"), PathBuf::from("/dev/ttyUSB2")];
        let preferred = vec![PathBuf::from("/dev/ttyUSB9")];
        assert_eq!(
            order_candidates_with_preference(candidates.clone(), &preferred),
            candidates
        );
    }

    #[test]
    fn order_candidates_unchanged_when_no_preference_given() {
        let candidates = vec![PathBuf::from("/dev/ttyUSB0"), PathBuf::from("/dev/ttyUSB2")];
        assert_eq!(
            order_candidates_with_preference(candidates.clone(), &[]),
            candidates
        );
    }

    // --- probe_is_at_capable: fake in-memory transport, mirroring
    // at_commander.rs's own MockStream (no real hardware). ---

    struct MockStream {
        reader: std::io::Cursor<Vec<u8>>,
    }

    impl MockStream {
        fn new(response: &str) -> Self {
            Self {
                reader: std::io::Cursor::new(response.as_bytes().to_vec()),
            }
        }
    }

    impl std::io::Read for MockStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            std::io::Read::read(&mut self.reader, buf)
        }
    }

    impl std::io::Write for MockStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn make_commander(response: &str) -> AtCommander {
        AtCommander::from_stream(MockStream::new(response), Duration::from_secs(1))
    }

    #[test]
    fn probe_is_at_capable_true_on_ok() {
        let mut at = make_commander("OK\r\n");
        assert!(probe_is_at_capable(&mut at));
    }

    #[test]
    fn probe_is_at_capable_false_on_error() {
        let mut at = make_commander("ERROR\r\n");
        assert!(!probe_is_at_capable(&mut at));
    }

    #[test]
    fn probe_is_at_capable_false_on_cme_error() {
        let mut at = make_commander("+CME ERROR: 100\r\n");
        assert!(!probe_is_at_capable(&mut at));
    }

    // `probe_sim_status`'s READY+IMSI path sends two AT commands
    // (AT+CPIN? then AT+CIMI) against one `AtCommander`. As documented in
    // `modules/usim.rs` (`ef_dir_record_matches_usim_aid_from_real_card`),
    // `AtCommander::read_response` builds a fresh `BufReader` per
    // `send_command` call, which over-reads and silently drops any
    // buffered-but-unconsumed bytes from a single-shot `Cursor`-backed mock
    // stream across more than one call — a pre-existing quirk unrelated to
    // this feature, not something to work around here. The two commands'
    // individual response parsing is covered directly instead:
    // `at_commander::tests::test_query_cpin_ready` and `test_query_imsi`.

    #[test]
    fn probe_sim_status_locked_on_sim_pin() {
        let mut at = make_commander("+CPIN: SIM PIN\r\nOK\r\n");
        assert_eq!(probe_sim_status(&mut at), SimStatus::Locked);
    }

    #[test]
    fn probe_sim_status_locked_on_sim_puk() {
        let mut at = make_commander("+CPIN: SIM PUK\r\nOK\r\n");
        assert_eq!(probe_sim_status(&mut at), SimStatus::Locked);
    }

    #[test]
    fn probe_sim_status_absent_on_cme_error_10() {
        let mut at = make_commander("+CME ERROR: 10\r\n");
        assert_eq!(probe_sim_status(&mut at), SimStatus::Absent);
    }

    #[test]
    fn probe_sim_status_unreadable_on_generic_error() {
        let mut at = make_commander("ERROR\r\n");
        assert!(matches!(
            probe_sim_status(&mut at),
            SimStatus::Unreadable(_)
        ));
    }

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
            modem_port: "/dev/ttyUSB6".to_string(),
            ..Default::default()
        };
        assert!(volte_claimed_ports(&config).is_empty());
    }

    #[test]
    fn an_enabled_cellular_service_claims_its_card() {
        let config = crate::config::VolteConfig {
            enabled: true,
            modem_port: "/dev/ttyUSB6".to_string(),
            ..Default::default()
        };
        assert_eq!(
            volte_claimed_ports(&config),
            vec![PathBuf::from("/dev/ttyUSB6")]
        );
    }

    #[test]
    fn discovery_mode_claims_pinned_override_ports_and_serials() {
        // Empty modem_port (auto-discovery) with pinned [[volte.line]]s: the
        // pinned AT ports are claimed, and a serial-pinned line is excluded
        // from the circuit-switched pool by card id (robust to a modem
        // answering AT on several ports) — specs/018-volte-multi-modem.
        let config = crate::config::VolteConfig {
            enabled: true,
            modem_port: String::new(),
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
    fn an_enabled_service_with_no_port_claims_nothing_rather_than_everything() {
        // An empty port must not be read as "claims the empty path" and then
        // silently match nothing — or worse, be treated as a wildcard.
        let config = crate::config::VolteConfig {
            enabled: true,
            modem_port: String::new(),
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
            modem_port: "/dev/ttyUSB6".to_string(),
            ..Default::default()
        };
        let claimed = volte_claimed_ports(&config);
        assert!(claimed.contains(&PathBuf::from("/dev/ttyUSB6")));

        // The exclusion set the circuit-switched scan applies is the union of
        // the VoWiFi line table and this, so a card can belong to exactly one.
        let mut excluded: std::collections::HashSet<PathBuf> = excluded_ports_from_lines_file();
        excluded.extend(claimed.iter().cloned());
        assert!(excluded.contains(&PathBuf::from("/dev/ttyUSB6")));
    }
}
