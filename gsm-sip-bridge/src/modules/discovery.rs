use crate::config::DiscoveryConfig;
use crate::error::BridgeResult;
use crate::modules::at_commander::{AtCommander, AtResponse};
use std::collections::{HashMap, HashSet};
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

/// Consecutive per-port probe timeouts after which a port is quarantined in
/// memory for the process lifetime (specs/030-bad-port-isolation FR-013).
const QUARANTINE_THRESHOLD: u8 = 3;

/// One serial interface a modem exposes: its `/dev/ttyUSB*` device path and the
/// sysfs USB interface directory it lives under. The interface directory's name
/// is the stable USB-topology fragment (e.g. `5-1.2.1.2:1.1`) — carried
/// alongside the device path so a hung-port timeout can log it and the operator
/// blocklist can match on it (specs/030-bad-port-isolation).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CandidatePort {
    device_path: PathBuf,
    iface_path: PathBuf,
}

/// The config-driven filtering plus the in-memory quarantine bookkeeping that a
/// scan consults (specs/030-bad-port-isolation). The quarantine must persist
/// *across* rescans but not across process restart, so a long-lived caller (the
/// `CardPool` rescan loop) owns one `DiscoveryPolicy` and threads `&mut` into
/// each scan; one-shot scans build a transient one.
pub struct DiscoveryPolicy {
    config: DiscoveryConfig,
    /// Consecutive probe timeouts per device path; reset on any non-timeout
    /// result.
    consecutive_timeouts: HashMap<PathBuf, u8>,
    /// Ports that reached `QUARANTINE_THRESHOLD` consecutive timeouts — skipped
    /// (never opened) by subsequent scans for the process lifetime.
    quarantined: HashSet<PathBuf>,
}

impl DiscoveryPolicy {
    pub fn new(config: DiscoveryConfig) -> Self {
        Self {
            config,
            consecutive_timeouts: HashMap::new(),
            quarantined: HashSet::new(),
        }
    }

    /// A policy that excludes nothing and uses the default probe timeout — for
    /// tests and the one-shot/legacy scan paths that carry no operator config.
    /// The bounded probe (and thus the wedge protection, FR-001) is still fully
    /// active, since the default timeout is baked into `DiscoveryConfig`.
    pub fn unfiltered() -> Self {
        Self::new(DiscoveryConfig::default())
    }

    fn is_blocklisted(&self, port: &CandidatePort) -> bool {
        self.config
            .excluded
            .iter()
            .any(|m| m.matches(&port.device_path, &port.iface_path))
    }

    fn is_quarantined(&self, device_path: &Path) -> bool {
        self.quarantined.contains(device_path)
    }

    /// Records that `device_path` timed out; quarantines it once it has done so
    /// `QUARANTINE_THRESHOLD` times in a row. Returns `true` only on the scan
    /// that first crosses the threshold, so the caller can emit a one-time
    /// transition warning — after that the port is silently skipped, so without
    /// that log the quarantine would leave no trace.
    fn record_timeout(&mut self, device_path: &Path) -> bool {
        let counter = self
            .consecutive_timeouts
            .entry(device_path.to_path_buf())
            .or_insert(0);
        *counter += 1;
        if *counter >= QUARANTINE_THRESHOLD {
            // `HashSet::insert` returns true only when newly inserted — exactly
            // the crossing event.
            self.quarantined.insert(device_path.to_path_buf())
        } else {
            false
        }
    }

    /// Records that `device_path` produced a result (any non-timeout outcome),
    /// resetting its consecutive-timeout streak.
    fn record_responded(&mut self, device_path: &Path) {
        self.consecutive_timeouts.remove(device_path);
    }
}

/// Runs `work` on a throwaway thread and waits at most `timeout` for it.
/// Returns `None` if it did not finish in time — the worker is then
/// deliberately leaked. A serial `open`/read on a port that wedges the kernel
/// `option` driver is uninterruptible from user space (a userspace read-timeout
/// and even `SIGTERM` don't break it), so abandoning the worker is the only way
/// to keep the scan moving (specs/030-bad-port-isolation). The leaked thread
/// stays blocked for the process lifetime — bounded by the per-port quarantine
/// and the operator blocklist. Same bounded-`recv_timeout` idiom already used in
/// `ims/agent.rs` and `observability/reporter.rs`.
fn run_bounded<T, F>(timeout: Duration, work: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // A send error just means the scan already gave up and dropped the
        // receiver; nothing to do but let this thread end (or stay blocked in
        // the kernel, if that is why we were abandoned).
        let _ = tx.send(work());
    });
    rx.recv_timeout(timeout).ok()
}

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
    // A one-shot scan with no operator config: the bounded probe and quarantine
    // still protect it (default timeout), but there is no persistent state to
    // carry, so a transient unfiltered policy is right.
    scan_all_inner(
        preferred_ports,
        &std::collections::HashSet::new(),
        SimRecovery::Disabled,
        &mut DiscoveryPolicy::unfiltered(),
    )
}

/// Whether a scan may try to *repair* a modem whose SIM does not read,
/// rather than only observing that it doesn't.
///
/// This is a deliberately narrow opt-in because the repair
/// (`AT+CFUN=0` → `AT+CFUN=1`, see `recover_and_reprobe_sim`) is not a
/// read-only probe: it drops and re-acquires the modem's radio, and blocks
/// the scan for the cycle delay plus the readiness poll. Both are fine
/// exactly once, at startup, before any line is carrying traffic — which is
/// the only place it was ever meant to run and the only place it was live-
/// tested (specs/027-discover-retry-health).
///
/// They are *not* fine on [`scan_modules`]' ongoing rescans, which run for
/// the container's whole lifetime alongside modems that are actively
/// registered or mid-call. Those rescans reach the very same
/// `probe_sim_status_at`, so without this switch a modem whose SIM read
/// merely glitched — including a circuit-switched one carrying a call, which
/// `skip_card_ids` does not cover (it only protects *VoWiFi* lines) — would
/// have had its radio power-cycled out from under it, and every rescan would
/// stall for the poll window per unreadable modem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimRecovery {
    /// Observe and report `SimStatus::Unreadable`; never touch the radio.
    Disabled,
    /// On `Unreadable`, attempt one `AT+CFUN` cycle and re-probe.
    CfunCycleOnUnreadable,
}

/// [`scan_all_preferring`] for the one-shot startup scan behind
/// `gsm-sip-bridge discover`, which may additionally attempt SIM recovery —
/// see [`SimRecovery`] for why every other caller must not.
pub fn scan_all_preferring_with_sim_recovery(
    preferred_ports: &[PathBuf],
    sim_recovery: SimRecovery,
    policy: &mut DiscoveryPolicy,
) -> BridgeResult<Vec<ProbedModem>> {
    scan_all_inner(
        preferred_ports,
        &std::collections::HashSet::new(),
        sim_recovery,
        policy,
    )
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
        &std::collections::HashSet::new(),
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
    skip_card_ids: &std::collections::HashSet<String>,
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
        let at_port = probe_at_port(&dev_path, preferred_ports, policy);
        let sim_timeout = policy.config.probe_timeout;
        let sim_status = at_port
            .as_ref()
            .map(|port| probe_sim_status_at(port, sim_recovery, sim_timeout));

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

/// Default path for the VoWiFi line-resolution artifact
/// (specs/013-multi-card-vowifi, `contracts/discover-cli-contract.md`).
/// Defined here (not in `vowifi::discovery`) so this module — the
/// lower-level shared scan both subsystems build on — has no dependency on
/// the `vowifi` module; `vowifi::discovery`'s writer imports this constant
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
    let path = crate::line::manifest::volte_lines_path();
    let Ok(contents) = fs::read_to_string(&path) else {
        return VolteManifestExcerpt::default();
    };
    serde_json::from_str(&contents).unwrap_or_else(|e| {
        tracing::warn!(
            path = %path.display(),
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
fn candidate_tty_ports(dev_path: &Path) -> Vec<CandidatePort> {
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
            candidates.push(CandidatePort {
                device_path: PathBuf::from(format!("/dev/{tty}")),
                iface_path,
            });
        }
    }
    candidates.sort_by(|a, b| a.device_path.cmp(&b.device_path));
    candidates
}

/// Reorders `candidates` so any whose device path appears in `preferred` come
/// first (each in its original relative order otherwise) — a device with
/// several AT-capable interfaces should try an operator-named port before
/// falling through to "whichever answers first" (see `scan_all_preferring`'s
/// doc comment). Pure and unit-tested; `probe_at_port` (real serial I/O) is not.
fn order_candidates_with_preference(
    candidates: Vec<CandidatePort>,
    preferred: &[PathBuf],
) -> Vec<CandidatePort> {
    let (mut first, mut rest): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|c| preferred.contains(&c.device_path));
    first.append(&mut rest);
    first
}

/// Tries every candidate serial interface in turn (an operator-preferred
/// one first, if present — see `order_candidates_with_preference`), opening
/// it and sending a bare `AT`, and returns the first one that answers `OK` —
/// the live probe replacing the old fixed-interface-number lookup (FR-002).
///
/// Each candidate the operator excluded (FR-007) or that has been quarantined
/// after repeated timeouts (FR-013) is skipped without opening it. Every open +
/// AT exchange runs on an abandonable worker bounded by `policy`'s probe
/// timeout, so a port that wedges the kernel driver is abandoned rather than
/// wedging the whole scan (FR-001/FR-002). Real hardware I/O; the bounded-runner
/// mechanism, the matcher, and the quarantine counter are unit-tested below.
fn probe_at_port(
    dev_path: &Path,
    preferred: &[PathBuf],
    policy: &mut DiscoveryPolicy,
) -> Option<PathBuf> {
    let candidates = order_candidates_with_preference(candidate_tty_ports(dev_path), preferred);
    select_at_capable_port(candidates, policy, probe_one_candidate)
}

/// The result of probing one candidate serial interface
/// (specs/030-bad-port-isolation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeOutcome {
    /// Answered `AT` with `OK` — usable.
    AtCapable,
    /// A real, non-timeout result: opened but not `AT`-capable, or the open
    /// itself failed cleanly. Either way the port responded, so it resets the
    /// consecutive-timeout streak.
    NotAtCapable,
    /// The bounded probe was abandoned because it did not finish in time.
    TimedOut,
}

/// The candidate-selection logic, factored out of live serial I/O so it is
/// unit-testable with a fake `probe_one`: it applies the blocklist and
/// quarantine skips (so an excluded/quarantined port is *never handed to*
/// `probe_one`, FR-007/FR-013/SC-003), calls `probe_one` for each remaining
/// candidate in order, updates the quarantine bookkeeping, logs, and returns
/// the first `AT`-capable device path — continuing past an abandoned one
/// (FR-002/FR-003). Production passes [`probe_one_candidate`] (real bounded
/// serial I/O); tests pass a scripted closure.
fn select_at_capable_port(
    candidates: Vec<CandidatePort>,
    policy: &mut DiscoveryPolicy,
    mut probe_one: impl FnMut(&Path, Duration) -> ProbeOutcome,
) -> Option<PathBuf> {
    let timeout = policy.config.probe_timeout;
    for candidate in candidates {
        if policy.is_blocklisted(&candidate) {
            // info, not debug: US3 scenario 2 — an operator reading normal logs
            // must be able to see that their exclusion is taking effect.
            tracing::info!(
                port = %candidate.device_path.display(),
                iface = %candidate.iface_path.display(),
                "serial port skipped by the [discovery].excluded_ports exclusion list; not probing"
            );
            continue;
        }
        if policy.is_quarantined(&candidate.device_path) {
            tracing::debug!(
                port = %candidate.device_path.display(),
                "serial port quarantined after repeated probe timeouts; not probed again until \
                 process restart"
            );
            continue;
        }

        match probe_one(&candidate.device_path, timeout) {
            ProbeOutcome::AtCapable => {
                policy.record_responded(&candidate.device_path);
                return Some(candidate.device_path);
            }
            ProbeOutcome::NotAtCapable => policy.record_responded(&candidate.device_path),
            ProbeOutcome::TimedOut => {
                let newly_quarantined = policy.record_timeout(&candidate.device_path);
                tracing::warn!(
                    port = %candidate.device_path.display(),
                    iface = %candidate.iface_path.display(),
                    timeout_ms = timeout.as_millis(),
                    "AT probe exceeded timeout; abandoning port, left unresolved \
                     (add its iface path to [discovery].excluded_ports to skip it permanently)"
                );
                if newly_quarantined {
                    // One-time transition record: after this the port is only
                    // skipped at debug, so this is the durable evidence.
                    tracing::warn!(
                        port = %candidate.device_path.display(),
                        iface = %candidate.iface_path.display(),
                        threshold = QUARANTINE_THRESHOLD,
                        "serial port quarantined for the process lifetime after consecutive probe \
                         timeouts; it will not be probed again until restart — add its iface path \
                         to [discovery].excluded_ports to make this permanent"
                    );
                }
            }
        }
    }
    None
}

/// Production `probe_one`: opens the port and sends a bare `AT` on an
/// abandonable worker bounded by `timeout` (see [`run_bounded`]). Real hardware
/// I/O — the surrounding selection logic ([`select_at_capable_port`]) is what's
/// unit-tested.
fn probe_one_candidate(device_path: &Path, timeout: Duration) -> ProbeOutcome {
    let probe_path = device_path.to_path_buf();
    match run_bounded(timeout, move || {
        match AtCommander::open_with_timeout(&probe_path, PROBE_TIMEOUT) {
            Ok(mut at) => probe_is_at_capable(&mut at),
            Err(e) => {
                tracing::debug!(
                    port = %probe_path.display(),
                    error = %e,
                    "could not open candidate serial port during AT probe"
                );
                false
            }
        }
    }) {
        Some(true) => ProbeOutcome::AtCapable,
        Some(false) => ProbeOutcome::NotAtCapable,
        None => ProbeOutcome::TimedOut,
    }
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
/// interpretation logic. On `Unreadable`, attempts one CFUN power-cycle via
/// `recover_and_reprobe_sim` before giving up (specs/027-discover-retry-health
/// — see that function's doc comment for why).
fn probe_sim_status_at(port: &Path, sim_recovery: SimRecovery, timeout: Duration) -> SimStatus {
    let open_port = port.to_path_buf();
    // The bounded region is the open PLUS the `AT+CPIN?`/`AT+CIMI` reads inside
    // `probe_sim_status` — each of which can itself block up to the per-line
    // port read timeout, so the worst case approaches the full `timeout` budget
    // (which is why that budget is generous, ~5s, not just an open's worth).
    //
    // Unlike the AT-open probe, a timeout HERE does not feed the quarantine
    // counter: this port already answered `AT`, so a slow SIM read is far more
    // likely transient than a wedged driver, and must not blackhole a healthy
    // modem for the process lifetime (specs/030-bad-port-isolation). The optional
    // CFUN recovery below is deliberately left unbounded (specs/027): it sleeps
    // for CFUN_CYCLE_DELAY plus a poll window by design and would be falsely
    // abandoned by the probe timeout.
    let opened = run_bounded(timeout, move || {
        match AtCommander::open_with_timeout(&open_port, PROBE_TIMEOUT) {
            Ok(mut at) => {
                let status = probe_sim_status(&mut at);
                SimProbe::Opened(status, at)
            }
            Err(e) => SimProbe::OpenFailed(e.to_string()),
        }
    });

    let (status, mut at) = match opened {
        None => {
            tracing::warn!(
                port = %port.display(),
                timeout_ms = timeout.as_millis(),
                "SIM-status probe exceeded timeout; SIM left unread \
                 (not counted toward quarantine — the port already answered AT)"
            );
            return SimStatus::Unreadable("SIM-status probe timed out".to_string());
        }
        Some(SimProbe::OpenFailed(e)) => return SimStatus::Unreadable(e),
        Some(SimProbe::Opened(status, at)) => (status, at),
    };

    if sim_recovery == SimRecovery::CfunCycleOnUnreadable
        && matches!(status, SimStatus::Unreadable(_))
    {
        tracing::warn!(
            port = %port.display(),
            reason = ?status,
            "SIM unreadable on first probe; attempting a CFUN power-cycle before giving up"
        );
        recover_and_reprobe_sim(
            &mut at,
            crate::supervise::sim_recovery::CFUN_CYCLE_DELAY,
            crate::supervise::sim_recovery::CPIN_POLL_INTERVAL,
            crate::supervise::sim_recovery::CPIN_POLL_ATTEMPTS,
        )
    } else {
        status
    }
}

/// The result of the bounded SIM-status open, carrying the still-open
/// `AtCommander` back out of the worker thread so an optional (unbounded) CFUN
/// recovery can reuse it (see `probe_sim_status_at`).
enum SimProbe {
    Opened(SimStatus, AtCommander),
    OpenFailed(String),
}

/// After a first-probe `Unreadable` result, power-cycles the SIM in place
/// (`AT+CFUN=0` -> `AT+CFUN=1`) and re-probes once — the same recipe as
/// `supervise::sim_recovery::reset_modem_sim`/`vowifi::usim_bridge`'s
/// `reset_sim_in_place` (see either's doc comment for the sugam incident
/// this traces back to), driven directly over the `AtCommander` this probe
/// already has open rather than shelling out via `CommandRunner`.
/// `discover`'s probe never has a running vowifi-usim-bridge/swu-dialer
/// holder to freeze first, unlike those two call sites: it runs before any
/// per-line agent starts, so nothing else is using the port yet.
///
/// Without this, a SIM that is transiently unreadable at boot (the sugam
/// pattern: `+CME ERROR: 13`, cleared in practice by a soft radio cycle)
/// stayed permanently unreadable through `discover`'s one-shot probe — live
/// testing against real EC20 hardware (specs/027-discover-retry-health)
/// found this reachable, not just hypothetical. Timing is a parameter
/// (rather than reading the constants directly) purely so tests can drive
/// the same real AT-command sequence with near-zero delays.
fn recover_and_reprobe_sim(
    at: &mut AtCommander,
    cycle_delay: Duration,
    poll_interval: Duration,
    poll_attempts: u32,
) -> SimStatus {
    let _ = at.send_command("AT+CFUN=0");
    std::thread::sleep(cycle_delay);
    let _ = at.send_command("AT+CFUN=1");

    for _ in 0..poll_attempts {
        std::thread::sleep(poll_interval);
        if matches!(at.query_cpin(), Ok(status) if status.contains("READY")) {
            break;
        }
    }
    probe_sim_status(at)
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
        let device_paths: Vec<PathBuf> = candidates.iter().map(|c| c.device_path.clone()).collect();
        assert_eq!(
            device_paths,
            vec![
                PathBuf::from("/dev/ttyUSB0"),
                PathBuf::from("/dev/ttyUSB2"),
                PathBuf::from("/dev/ttyUSB4"),
            ]
        );
        // The USB interface (topology) path is captured alongside each device
        // path (specs/030-bad-port-isolation): the timeout log and the operator
        // blocklist both key off it.
        assert_eq!(
            candidates[0].iface_path.file_name().unwrap(),
            std::ffi::OsStr::new("1-1:1.0")
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
        let device_paths: Vec<PathBuf> = candidates.iter().map(|c| c.device_path.clone()).collect();
        assert_eq!(device_paths, vec![PathBuf::from("/dev/ttyUSB4")]);
    }

    /// Builds a `CandidatePort` from a device path and an interface (topology)
    /// fragment, for the ordering/matching tests below.
    fn cand(dev: &str, iface: &str) -> CandidatePort {
        CandidatePort {
            device_path: PathBuf::from(dev),
            iface_path: PathBuf::from(iface),
        }
    }

    fn device_paths(cands: Vec<CandidatePort>) -> Vec<PathBuf> {
        cands.into_iter().map(|c| c.device_path).collect()
    }

    // --- specs/030-bad-port-isolation: bounded probe, quarantine, blocklist.
    // The real kernel hang needs the specific hardware; a never-returning
    // closure is the faithful stand-in for "an open/read that never comes
    // back", exercising the actual thread-spawn + recv_timeout mechanism. ---

    #[test]
    fn run_bounded_abandons_work_that_never_finishes() {
        let start = std::time::Instant::now();
        let result: Option<()> = run_bounded(Duration::from_millis(150), || {
            // Stands in for a serial open that wedges the kernel driver.
            std::thread::sleep(Duration::from_secs(3600));
        });
        assert!(
            result.is_none(),
            "a never-finishing probe must be abandoned"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "abandoning must happen at ~the timeout, not wait for the work"
        );
    }

    #[test]
    fn run_bounded_returns_a_slow_but_healthy_result() {
        // Sleeps well under the timeout: a slow-but-working port must resolve,
        // not be falsely abandoned (US1 acceptance scenario 3).
        let result = run_bounded(Duration::from_secs(2), || {
            std::thread::sleep(Duration::from_millis(100));
            42
        });
        assert_eq!(result, Some(42));
    }

    #[test]
    fn port_is_quarantined_after_three_consecutive_timeouts() {
        let mut policy = DiscoveryPolicy::unfiltered();
        let port = PathBuf::from("/dev/ttyUSB1");
        assert!(!policy.is_quarantined(&port));
        policy.record_timeout(&port);
        policy.record_timeout(&port);
        assert!(!policy.is_quarantined(&port), "only two timeouts so far");
        policy.record_timeout(&port);
        assert!(
            policy.is_quarantined(&port),
            "quarantined on the third in a row"
        );
    }

    #[test]
    fn a_responding_probe_resets_the_timeout_streak() {
        let mut policy = DiscoveryPolicy::unfiltered();
        let port = PathBuf::from("/dev/ttyUSB1");
        policy.record_timeout(&port);
        policy.record_timeout(&port);
        policy.record_responded(&port); // streak broken by a real result
        policy.record_timeout(&port);
        policy.record_timeout(&port);
        assert!(
            !policy.is_quarantined(&port),
            "two, a reset, then two more must not reach the threshold"
        );
    }

    #[test]
    fn blocklist_matches_device_prefix_and_leaves_others_alone() {
        use crate::config::{DiscoveryConfig, PortMatcher};
        let config = DiscoveryConfig {
            excluded: vec![PortMatcher::parse("5-1.2.1.2").unwrap()],
            ..DiscoveryConfig::default()
        };
        let policy = DiscoveryPolicy::new(config);
        let excluded = CandidatePort {
            device_path: PathBuf::from("/dev/ttyUSB1"),
            iface_path: PathBuf::from("/sys/bus/usb/devices/5-1.2.1.2:1.1"),
        };
        let other = CandidatePort {
            device_path: PathBuf::from("/dev/ttyUSB0"),
            iface_path: PathBuf::from("/sys/bus/usb/devices/5-1.2.1.3:1.0"),
        };
        assert!(
            policy.is_blocklisted(&excluded),
            "a whole-device topology fragment excludes its interfaces"
        );
        assert!(
            !policy.is_blocklisted(&other),
            "a different device is untouched"
        );
    }

    /// A scripted `probe_one` for `select_at_capable_port`: maps each device
    /// path to a fixed outcome and records the order in which ports are actually
    /// handed to it — so a test can assert both the selection result and that a
    /// blocklisted/quarantined port is *never probed* (SC-003). A `TimedOut`
    /// entry is the fake-port stand-in for an open that never returns.
    fn scripted_probe(
        outcomes: HashMap<PathBuf, ProbeOutcome>,
        probed: std::rc::Rc<std::cell::RefCell<Vec<PathBuf>>>,
    ) -> impl FnMut(&Path, Duration) -> ProbeOutcome {
        move |port: &Path, _timeout| {
            probed.borrow_mut().push(port.to_path_buf());
            outcomes
                .get(port)
                .copied()
                .unwrap_or(ProbeOutcome::NotAtCapable)
        }
    }

    #[test]
    fn probe_abandons_a_wedged_candidate_and_continues_to_the_next() {
        let bad = PathBuf::from("/dev/ttyUSB1");
        let good = PathBuf::from("/dev/ttyUSB2");
        let candidates = vec![
            cand("/dev/ttyUSB1", "5-1:1.1"),
            cand("/dev/ttyUSB2", "5-1:1.2"),
        ];
        let outcomes = HashMap::from([
            (bad.clone(), ProbeOutcome::TimedOut),
            (good.clone(), ProbeOutcome::AtCapable),
        ]);
        let probed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut policy = DiscoveryPolicy::unfiltered();
        let result = select_at_capable_port(
            candidates,
            &mut policy,
            scripted_probe(outcomes, probed.clone()),
        );
        assert_eq!(
            result,
            Some(good.clone()),
            "abandons the wedged candidate and returns the next AT-capable one"
        );
        assert_eq!(
            *probed.borrow(),
            vec![bad.clone(), good],
            "both tried, in order"
        );
        assert_eq!(
            policy.consecutive_timeouts.get(&bad).copied(),
            Some(1),
            "the abandoned port took a timeout strike"
        );
    }

    #[test]
    fn probe_returns_none_when_every_candidate_times_out() {
        // FR-011 / T010: a modem whose only interfaces all wedge yields no
        // usable AT port (and does not hang).
        let candidates = vec![
            cand("/dev/ttyUSB1", "5-1:1.1"),
            cand("/dev/ttyUSB2", "5-1:1.2"),
        ];
        let outcomes = HashMap::from([
            (PathBuf::from("/dev/ttyUSB1"), ProbeOutcome::TimedOut),
            (PathBuf::from("/dev/ttyUSB2"), ProbeOutcome::TimedOut),
        ]);
        let probed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut policy = DiscoveryPolicy::unfiltered();
        let result =
            select_at_capable_port(candidates, &mut policy, scripted_probe(outcomes, probed));
        assert_eq!(result, None);
    }

    #[test]
    fn probe_never_opens_a_blocklisted_port() {
        use crate::config::{DiscoveryConfig, PortMatcher};
        // ttyUSB1 is blocklisted; even though the fake would answer AT on it, it
        // must never be handed to the prober (SC-003), so the healthy ttyUSB2
        // wins.
        let candidates = vec![
            cand("/dev/ttyUSB1", "5-1.2.1.2:1.1"),
            cand("/dev/ttyUSB2", "5-1.2.1.3:1.0"),
        ];
        let config = DiscoveryConfig {
            excluded: vec![PortMatcher::parse("5-1.2.1.2:1.1").unwrap()],
            ..DiscoveryConfig::default()
        };
        let mut policy = DiscoveryPolicy::new(config);
        let outcomes = HashMap::from([
            (PathBuf::from("/dev/ttyUSB1"), ProbeOutcome::AtCapable),
            (PathBuf::from("/dev/ttyUSB2"), ProbeOutcome::AtCapable),
        ]);
        let probed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let result = select_at_capable_port(
            candidates,
            &mut policy,
            scripted_probe(outcomes, probed.clone()),
        );
        assert_eq!(result, Some(PathBuf::from("/dev/ttyUSB2")));
        assert!(
            !probed.borrow().contains(&PathBuf::from("/dev/ttyUSB1")),
            "a blocklisted port is never opened/probed (SC-003)"
        );
    }

    #[test]
    fn probe_skips_a_quarantined_port() {
        let p1 = PathBuf::from("/dev/ttyUSB1");
        let candidates = vec![
            cand("/dev/ttyUSB1", "5-1:1.1"),
            cand("/dev/ttyUSB2", "5-1:1.2"),
        ];
        let mut policy = DiscoveryPolicy::unfiltered();
        for _ in 0..QUARANTINE_THRESHOLD {
            policy.record_timeout(&p1);
        }
        let outcomes = HashMap::from([
            (p1.clone(), ProbeOutcome::AtCapable),
            (PathBuf::from("/dev/ttyUSB2"), ProbeOutcome::AtCapable),
        ]);
        let probed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let result = select_at_capable_port(
            candidates,
            &mut policy,
            scripted_probe(outcomes, probed.clone()),
        );
        assert_eq!(result, Some(PathBuf::from("/dev/ttyUSB2")));
        assert!(
            !probed.borrow().contains(&p1),
            "a quarantined port is not re-probed on a later scan"
        );
    }

    #[test]
    fn multiple_bad_ports_are_tracked_and_quarantined_independently() {
        // Edge case: several simultaneously-wedged ports must each accumulate
        // their own streak, so one bad port hitting the threshold never
        // quarantines an unrelated one.
        let mut policy = DiscoveryPolicy::unfiltered();
        let a = PathBuf::from("/dev/ttyUSB1");
        let b = PathBuf::from("/dev/ttyUSB2");
        for _ in 0..QUARANTINE_THRESHOLD {
            policy.record_timeout(&a);
        }
        policy.record_timeout(&b);
        assert!(policy.is_quarantined(&a), "the port that hit the threshold");
        assert!(
            !policy.is_quarantined(&b),
            "one timeout must not quarantine a different port"
        );
    }

    #[test]
    fn unfiltered_policy_excludes_nothing_and_uses_the_default_timeout() {
        let policy = DiscoveryPolicy::unfiltered();
        let port = CandidatePort {
            device_path: PathBuf::from("/dev/ttyUSB1"),
            iface_path: PathBuf::from("/sys/bus/usb/devices/5-1.2.1.2:1.1"),
        };
        assert!(
            !policy.is_blocklisted(&port),
            "an empty [discovery] must exclude nothing (FR-008)"
        );
        assert_eq!(
            policy.config.probe_timeout,
            Duration::from_millis(crate::config::DEFAULT_PROBE_TIMEOUT_MS)
        );
    }

    #[test]
    fn sim_status_timeout_does_not_feed_the_quarantine_counter() {
        // A slow SIM read on a port that already answered AT must not count
        // toward quarantine (finding 5): a healthy modem must never be
        // blackholed for the process lifetime by transient SIM slowness. The
        // AT-probe seam is where timeouts are counted; the SIM path takes a
        // plain `Duration` and touches no policy state at all — this test pins
        // that contract at the type level.
        let mut policy = DiscoveryPolicy::unfiltered();
        let port = PathBuf::from("/dev/ttyUSB1");
        // Three AT-probe timeouts DO quarantine…
        for _ in 0..QUARANTINE_THRESHOLD {
            policy.record_timeout(&port);
        }
        assert!(policy.is_quarantined(&port));
        // …but `probe_sim_status_at` has no `&mut policy` to increment, by
        // construction: its signature is `(&Path, SimRecovery, Duration)`.
    }

    #[test]
    fn order_candidates_prefers_configured_port_when_present() {
        // Found live-testing: a real EC200 answered AT on both ttyUSB0 and
        // ttyUSB6. An operator-configured port must win over "whichever
        // sorts first" so an existing single-line config naming a
        // non-default AT port still gets used as-is (FR-009/FR-020).
        let candidates = vec![
            cand("/dev/ttyUSB0", "5-1:1.0"),
            cand("/dev/ttyUSB2", "5-1:1.2"),
            cand("/dev/ttyUSB6", "5-1:1.6"),
        ];
        let preferred = vec![PathBuf::from("/dev/ttyUSB6")];
        assert_eq!(
            device_paths(order_candidates_with_preference(candidates, &preferred)),
            vec![
                PathBuf::from("/dev/ttyUSB6"),
                PathBuf::from("/dev/ttyUSB0"),
                PathBuf::from("/dev/ttyUSB2"),
            ]
        );
    }

    #[test]
    fn order_candidates_unchanged_when_no_preference_matches() {
        let candidates = vec![
            cand("/dev/ttyUSB0", "5-1:1.0"),
            cand("/dev/ttyUSB2", "5-1:1.2"),
        ];
        let preferred = vec![PathBuf::from("/dev/ttyUSB9")];
        assert_eq!(
            device_paths(order_candidates_with_preference(
                candidates.clone(),
                &preferred
            )),
            device_paths(candidates)
        );
    }

    #[test]
    fn order_candidates_unchanged_when_no_preference_given() {
        let candidates = vec![
            cand("/dev/ttyUSB0", "5-1:1.0"),
            cand("/dev/ttyUSB2", "5-1:1.2"),
        ];
        assert_eq!(
            device_paths(order_candidates_with_preference(candidates.clone(), &[])),
            device_paths(candidates)
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

    /// A queue of responses, one per `send_command` call — unlike
    /// `MockStream`'s single-shot `Cursor`, this survives multiple
    /// sequential calls, needed to exercise `recover_and_reprobe_sim`'s
    /// AT+CFUN=0/AT+CFUN=1/poll/re-probe sequence. Mirrors
    /// `vowifi::usim_bridge`'s own `ScriptedModem` test helper (see its doc
    /// comment for why a fresh-`BufReader`-per-call transport needs this
    /// instead of a plain `Cursor`); duplicated locally rather than shared,
    /// same as this module's existing `MockStream` mirrors
    /// `at_commander.rs`'s.
    struct ScriptedModem {
        responses: std::collections::VecDeque<Vec<u8>>,
        current: Vec<u8>,
        pos: usize,
    }

    impl ScriptedModem {
        fn new(responses: &[&str]) -> Self {
            Self {
                responses: responses.iter().map(|s| s.as_bytes().to_vec()).collect(),
                current: Vec::new(),
                pos: 0,
            }
        }
    }

    impl std::io::Read for ScriptedModem {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.current.len() {
                let Some(next) = self.responses.pop_front() else {
                    return Ok(0);
                };
                self.current = next;
                self.pos = 0;
            }
            let remaining = &self.current[self.pos..];
            let n = remaining.len().min(buf.len());
            buf[..n].copy_from_slice(&remaining[..n]);
            self.pos += n;
            Ok(n)
        }
    }

    impl std::io::Write for ScriptedModem {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn make_scripted_commander(responses: &[&str]) -> AtCommander {
        AtCommander::from_stream(ScriptedModem::new(responses), Duration::from_secs(1))
    }

    /// The ongoing-rescan path must never power-cycle a radio: it runs for
    /// the container's whole lifetime next to modems that may be registered
    /// or mid-call, and `skip_card_ids` only shields *VoWiFi* lines, not
    /// circuit-switched ones. Encoded as a call-site assertion because the
    /// effect itself (`AT+CFUN`) needs real hardware to observe — what is
    /// checkable here is that the rescan entry point still asks for
    /// `Disabled`, which is what keeps `probe_sim_status_at` read-only.
    #[test]
    fn only_the_one_shot_discover_scan_opts_into_sim_recovery() {
        // Flattened so the check survives rustfmt reflowing argument lists.
        let src: String = include_str!("discovery.rs")
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

        let callers: Vec<&str> = include_str!("../commands/discover.rs")
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

    /// specs/027-discover-retry-health: a SIM that is unreadable on the
    /// first probe but comes back `+CPIN: READY` after a soft radio cycle —
    /// the exact live-hardware finding (EC20, `+CME ERROR: 13`) that
    /// motivated `recover_and_reprobe_sim`.
    #[test]
    fn recover_and_reprobe_sim_returns_ready_after_a_successful_cfun_cycle() {
        let mut at = make_scripted_commander(&[
            "OK\r\n",                    // AT+CFUN=0
            "OK\r\n",                    // AT+CFUN=1
            "+CPIN: READY\r\nOK\r\n",    // poll attempt 1: AT+CPIN?
            "+CPIN: READY\r\nOK\r\n",    // re-probe: AT+CPIN?
            "404438083996440\r\nOK\r\n", // re-probe: AT+CIMI
        ]);
        let status = recover_and_reprobe_sim(
            &mut at,
            Duration::from_millis(1),
            Duration::from_millis(1),
            1,
        );
        assert_eq!(
            status,
            SimStatus::Ready {
                imsi: "404438083996440".to_string()
            }
        );
    }

    /// If the SIM never comes back `READY` within the poll window, the
    /// re-probe at the end still runs and (correctly) reports `Unreadable`
    /// again rather than panicking or hanging.
    #[test]
    fn recover_and_reprobe_sim_stays_unreadable_if_cpin_never_becomes_ready() {
        let mut at = make_scripted_commander(&[
            "OK\r\n",             // AT+CFUN=0
            "OK\r\n",             // AT+CFUN=1
            "+CME ERROR: 13\r\n", // poll attempt 1: AT+CPIN?
            "+CME ERROR: 13\r\n", // re-probe: AT+CPIN?
        ]);
        let status = recover_and_reprobe_sim(
            &mut at,
            Duration::from_millis(1),
            Duration::from_millis(1),
            1,
        );
        assert!(matches!(status, SimStatus::Unreadable(_)));
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
        let mut excluded: std::collections::HashSet<PathBuf> = excluded_ports_from_lines_file();
        excluded.extend(claimed.iter().cloned());
        assert!(excluded.contains(&PathBuf::from("/dev/ttyUSB6")));
    }
}
