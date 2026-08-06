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
use crate::line::Rejection;
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
    ///
    /// `cs_enabled` is `[cs].enabled` (specs/026-disable-circuit-switched,
    /// FR-010a). With the circuit-switched path off, nothing is reserved for
    /// it: every successfully probed modem — including one that would
    /// otherwise be claimed by the audio-capable default above — is offered
    /// to VoWiFi instead. `resolve_lines`'s readiness filter and `max_lines`
    /// bound still apply afterward (FR-010b), so this only *offers*
    /// candidates; it doesn't bypass admission. With `cs_enabled` true this
    /// is exactly the prior behaviour (FR-010c).
    pub fn from_probed(
        modems: &[ProbedModem],
        overrides: &[VowifiLineOverride],
        cs_enabled: bool,
    ) -> Self {
        let mut circuit_switched = Vec::new();
        let mut vowifi = Vec::new();
        for modem in modems {
            if modem.at_port.is_none() {
                continue;
            }
            if !cs_enabled
                || is_overridden_to_vowifi(modem, overrides)
                || !modem.has_audio_capability
            {
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

/// Derives every per-line isolated resource from the line's index, uniformly
/// for every line including the first. No config knob backs any of these (see
/// `VowifiConfig`'s field docs) — they exist so lines cannot collide.
///
/// Shared by both resolvers deliberately. This block used to be copy-pasted
/// into the modem and the card-reader path, and the copies drifted exactly as
/// you would expect: `pcscf_source_path` was fixed in one and silently left
/// shared in the other, so a two-line deployment still had its card-reader line
/// overwriting the modem line's P-CSCF. One function means a resource added
/// here cannot be derived for only some kinds of line.
fn derive_line_resources(config: &mut VowifiConfig, base: &VowifiConfig, index: u32) {
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
    // Per-line like the rest, though it was long left shared on the reasoning
    // that it is a global scratch file. It is not: each line's tunnel is
    // assigned its own P-CSCF by its own carrier, each line's supervisor writes
    // it here, and each line's Agent A reads it back. With one file the loser
    // of the race registers against the *other* carrier's proxy — unreachable
    // from its own netns — and crash-loops. Observed live 2026-07-29 holding an
    // address belonging to neither line (a stale one from an earlier tunnel),
    // which is the same race one step further along.
    config.pcscf_source_path = resources::indexed(&format!("{}-", base.pcscf_source_path), index);
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
    /// Which configured `[[vowifi.line]]` override produced this line, named
    /// by [`override_identifier`] — `None` for an auto-discovered modem that
    /// no override pins.
    ///
    /// Recorded here, at the one point the override→modem match is actually
    /// known ([`override_for`]), rather than reconstructed later by
    /// comparing strings: `supervise`'s discovery-retry loop needs to tell
    /// whether the line it was waiting on has now resolved, and every
    /// attempt to answer that *after the fact* was wrong. Comparing the
    /// override identifier to `card_id` never matched at all (they are
    /// different identifier spaces); re-deriving one from the other via
    /// `derive_module_id` matched too eagerly, since that transform keeps
    /// only the last six alphanumerics uppercased, so two distinct serials
    /// ending `…abcdef`/`…ABCDEF` collapse to the same `card_id` and one
    /// modem resolving would have been read as the *other* one recovering.
    /// Both were P1 review findings on specs/027-discover-retry-health.
    pub configured_identifier: Option<String>,
    pub config: VowifiConfig,
}

#[derive(Debug, Clone, Default)]
pub struct LineTableResult {
    pub lines: Vec<ResolvedLine>,
    pub failed: Vec<FailedLine>,
}

/// Resolves `assignment.vowifi` into an ordered, bounded `LineTable`
/// (FR-012/FR-016): only SIM-ready candidates become lines, capped at
/// `base.max_lines` with the excess reported as failed rather than dropped.
///
/// **Membership** — which candidates make the cut — is decided by three
/// priority tiers sharing that one budget, each filled to capacity before
/// the next gets any of what remains, so an auto-discovered candidate can
/// never displace an operator's explicit configuration:
///
/// 1. A modem matched by an explicit `[[vowifi.line]]` `modem_serial`/
///    `modem_port` override.
/// 2. Every `pcsc_reader` line — always explicit; there is no such thing as
///    an auto-discovered card-reader line.
/// 3. Every other (auto-discovered, unpinned) modem.
///
/// Before specs/026-disable-circuit-switched, the second and third tiers
/// were the only ones that could ever compete for a scarce slot, and a
/// modem happened to win ties (specs/023-omnikey-pcsc-vowifi US2). That
/// stopped being safe once `[cs].enabled = false` could substantially
/// enlarge the auto-discovered pool with modems the circuit-switched path
/// used to reserve for itself (greptile P1 on that PR): an auto-discovered
/// modem must never bump an explicit pin, of either kind.
///
/// **Indices**, once membership is settled, are assigned independently of
/// tier: every kept modem (pinned or not) ordered by card id, then every
/// kept pcsc line ordered by position among the configured overrides — the
/// same convention this function used before priority tiers existed. That
/// split matters operationally: a line's index determines its whole network
/// identity (namespace, veth pair, ports), so reassigning indices on every
/// upgrade — even to deployments with no actual contention, where nothing
/// was ever at risk of being bumped — would needlessly tear down and
/// rebuild every tunnel on restart. Tier order only ever changes *who* is
/// kept; a deployment where everything already fit within `max_lines` gets
/// byte-identical indices to before this priority scheme existed.
pub fn resolve_lines(assignment: &RoleAssignment, base: &VowifiConfig) -> LineTableResult {
    let max_lines = base.max_lines as usize;
    let mut failed = Vec::new();

    // Role assignment has already established each candidate's AT port, so
    // a `Ready` SIM without one cannot occur here.
    let mut ready: Vec<&ProbedModem> = Vec::new();
    for modem in &assignment.vowifi {
        match crate::line::classify(modem, crate::line::AtPortRequirement::AlreadyEstablished) {
            Ok(()) => ready.push(modem),
            Err(r) => {
                // Provenance must be checked here, before the ready-only
                // partition below ever runs: a modem that fails classify()
                // never reaches it, so without this check every classify
                // failure — pinned or auto-discovered alike — was
                // indistinguishable (review finding on this PR: an
                // auto-discovered modem's `sim_unreadable` was reported as a
                // "configured line from config.toml").
                let configured = is_overridden_to_vowifi(modem, &base.line_overrides);
                failed.push(
                    FailedLine::new(modem.card_id.clone(), r.reason()).configured(configured),
                );
            }
        }
    }
    // Sorted before the tier split below so each tier's slice remains
    // card-id ordered (`partition` preserves relative order), which is what
    // "the excluded suffix is the highest card ids" and the index-assignment
    // pass both depend on.
    ready.sort_by(|a, b| a.card_id.cmp(&b.card_id));
    let (pinned_modems, unpinned_modems): (Vec<&ProbedModem>, Vec<&ProbedModem>) = ready
        .into_iter()
        .partition(|m| is_overridden_to_vowifi(m, &base.line_overrides));

    let pcsc_overrides: Vec<&VowifiLineOverride> = base
        .line_overrides
        .iter()
        .filter(|o| o.pcsc_reader)
        .collect();

    // --- Membership: tier 1 (pinned modems) → tier 2 (pcsc) → tier 3
    // (unpinned modems), each capped by whatever budget the tiers before it
    // left behind.
    let pinned_kept = pinned_modems.len().min(max_lines);
    let pcsc_kept = pcsc_overrides
        .len()
        .min(max_lines.saturating_sub(pinned_kept));
    let unpinned_kept = unpinned_modems
        .len()
        .min(max_lines.saturating_sub(pinned_kept + pcsc_kept));

    for modem in &pinned_modems[pinned_kept..] {
        failed.push(
            FailedLine::new(
                modem.card_id.clone(),
                crate::line::Rejection::MaxLinesExceeded.reason(),
            )
            .configured(true),
        );
    }
    // A synthetic `pcscN` card id (N = position among pcsc overrides) since
    // there is no USB modem identity to report instead.
    for (i, _) in pcsc_overrides.iter().enumerate().skip(pcsc_kept) {
        failed.push(
            FailedLine::new(
                format!("pcsc{i}"),
                crate::line::Rejection::MaxLinesExceeded.reason(),
            )
            .configured(true),
        );
    }
    for modem in &unpinned_modems[unpinned_kept..] {
        failed.push(FailedLine::new(
            modem.card_id.clone(),
            crate::line::Rejection::MaxLinesExceeded.reason(),
        ));
    }

    // --- Indices: every kept modem together, by card id, regardless of
    // which tier kept it; then every kept pcsc line.
    let mut kept_modems: Vec<&ProbedModem> = pinned_modems[..pinned_kept]
        .iter()
        .chain(unpinned_modems[..unpinned_kept].iter())
        .copied()
        .collect();
    kept_modems.sort_by(|a, b| a.card_id.cmp(&b.card_id));

    let mut lines: Vec<ResolvedLine> = kept_modems
        .iter()
        .enumerate()
        .map(|(i, modem)| resolve_one_line(i as u32, modem, base))
        .collect();

    for (i, over) in pcsc_overrides.iter().take(pcsc_kept).enumerate() {
        let card_id = format!("pcsc{i}");
        let index = lines.len() as u32;
        lines.push(resolve_one_pcsc_line(index, card_id, over, base));
    }

    LineTableResult { lines, failed }
}

/// Configured modem overrides (`modem_port`/`modem_serial`) with no
/// matching entry in `all_probed` — specs/027-discover-retry-health FR-001.
///
/// `resolve_lines`'s own `failed` list only ever reports on candidates that
/// made it into `assignment.vowifi` (i.e. were probed *and* answered AT) —
/// an override whose modem never enumerated on the USB bus at all is
/// invisible to it. `all_probed` must be the full pre-role-assignment scan
/// result (`scan_all_preferring`'s return value in `discover.rs`, before
/// `RoleAssignment::from_probed` filters it), so a device that was seen but
/// never got a working AT port still counts as "matched" here — that case
/// already has its own `Rejection::NoAtPort`/SIM-status reason once it
/// reaches `resolve_lines`; this function is only for "never seen at all".
///
/// `pcsc_reader` overrides are deliberately excluded: `scan_all_preferring`
/// only ever scans USB modems, so it has no way to confirm or deny a PC/SC
/// card reader's presence — that would need a distinct probe this feature
/// doesn't add.
pub fn unmatched_overrides(
    overrides: &[VowifiLineOverride],
    all_probed: &[ProbedModem],
) -> Vec<FailedLine> {
    overrides
        .iter()
        .filter(|o| !o.pcsc_reader)
        .filter(|o| {
            !all_probed.iter().any(|m| {
                o.modem_serial.as_deref().is_some_and(|s| s == m.usb_serial)
                    || o.modem_port.as_deref().is_some_and(|p| {
                        m.at_port
                            .as_deref()
                            .is_some_and(|port| port == Path::new(p))
                    })
            })
        })
        .filter_map(|o| {
            let identifier = override_identifier(o)?;
            Some(FailedLine::new(identifier, Rejection::NotFound.reason()).configured(true))
        })
        .collect()
}

/// How a configured `[[vowifi.line]]` override names itself, in the one
/// identifier space that both the "this override never resolved"
/// ([`unmatched_overrides`], via `FailedLine::card_id`) and the "this
/// override *did* resolve" ([`ResolvedLine::configured_identifier`]) sides
/// agree on.
///
/// Deliberately a single shared function rather than the same
/// `modem_port.or(modem_serial)` expression written twice. `supervise`'s
/// discovery-retry loop keys its pending set by exactly this string and has
/// to decide whether a given resolved line is the override it was waiting
/// on; two independent copies of this rule that drift apart would make that
/// comparison quietly wrong, which is the class of bug that produced two
/// separate P1 review findings on specs/027-discover-retry-health (first
/// comparing an override identifier against a USB-derived `card_id`, then
/// re-deriving one from the other through the *lossy* `derive_module_id`).
pub fn override_identifier(o: &VowifiLineOverride) -> Option<String> {
    o.modem_port.clone().or_else(|| o.modem_serial.clone())
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

    derive_line_resources(&mut config, base, index);

    ResolvedLine {
        index,
        card_id: modem.card_id.clone(),
        modem_port,
        mcc,
        mnc,
        imsi_override,
        pcsc_reader: false,
        // `over` is the exact override this modem matched, so this is the
        // authoritative answer to "which configured line is this" — see the
        // field's doc comment.
        configured_identifier: over.and_then(override_identifier),
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
/// port), but the network identity comes from the override rather than from a
/// probed modem. `imsi_override` is mandatory (config validation guarantees
/// it is `Some`) because it names which reader's card this line owns;
/// `mcc`/`mnc` are optional exactly as on a modem line — left unset they stay
/// empty here and are derived later from the card's own EF_IMSI/EF_AD over
/// PC/SC (`vowifi-plmn --pcsc-imsi`, `plmn::derive_plmn_from_card`).
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

    derive_line_resources(&mut config, base, index);

    ResolvedLine {
        index,
        card_id,
        modem_port: PathBuf::new(),
        mcc,
        mnc,
        imsi_override,
        pcsc_reader: true,
        // A pcsc line is always explicitly configured; `override_identifier`
        // still returns `None` for the usual reader override that pins
        // neither a port nor a serial, which is correct — the retry loop
        // excludes pcsc overrides entirely (`unmatched_overrides`), since
        // nothing in the USB scan can confirm or deny a reader's presence.
        configured_identifier: override_identifier(over),
        config,
    }
}

/// The serialized artifact `gsm-sip-bridge discover` writes so the
/// circuit-switched daemon and `supervise::orchestrate` agree on the same
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
    /// See [`ResolvedLine::configured_identifier`]. `#[serde(default)]` so a
    /// resolution file written by an older build (a `docker restart` keeps
    /// `/tmp`) still deserializes — it reads back as `None`, i.e. "not a
    /// configured line", which is the safe direction: the retry loop then
    /// keeps waiting rather than falsely reporting recovery.
    #[serde(default)]
    pub configured_identifier: Option<String>,
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
            configured_identifier: line.configured_identifier.clone(),
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
        let assignment = RoleAssignment::from_probed(&modems, &[], true);
        assert_eq!(assignment.circuit_switched.len(), 1);
        assert_eq!(assignment.circuit_switched[0].card_id, "ec20-AAAAAA");
        assert_eq!(assignment.vowifi.len(), 1);
        assert_eq!(assignment.vowifi[0].card_id, "ec20-BBBBBB");
    }

    /// specs/026-disable-circuit-switched FR-010a: with the circuit-switched
    /// path off, a voice-capable modem that the default rule would otherwise
    /// reserve for it (`role_assignment_default_splits_by_audio` above, same
    /// fixture) is offered to VoWiFi instead — nothing is reserved for a
    /// path that is disabled.
    #[test]
    fn role_assignment_offers_every_modem_to_vowifi_when_cs_is_disabled() {
        let modems = vec![
            ready_modem("ec20-AAAAAA", "/dev/ttyUSB0", true, "404011111111111"),
            ready_modem("ec20-BBBBBB", "/dev/ttyUSB1", false, "404022222222222"),
        ];
        let assignment = RoleAssignment::from_probed(&modems, &[], false);
        assert!(
            assignment.circuit_switched.is_empty(),
            "nothing should be reserved for a disabled path"
        );
        assert_eq!(assignment.vowifi.len(), 2);
        let ids: Vec<&str> = assignment
            .vowifi
            .iter()
            .map(|m| m.card_id.as_str())
            .collect();
        assert!(ids.contains(&"ec20-AAAAAA"));
        assert!(ids.contains(&"ec20-BBBBBB"));
    }

    /// FR-010c: the flag-on case is untouched — same fixture, same result as
    /// `role_assignment_default_splits_by_audio`, just spelled out with the
    /// new parameter explicit rather than relied on implicitly.
    #[test]
    fn role_assignment_default_splits_by_audio_when_cs_enabled() {
        let modems = vec![
            ready_modem("ec20-AAAAAA", "/dev/ttyUSB0", true, "404011111111111"),
            ready_modem("ec20-BBBBBB", "/dev/ttyUSB1", false, "404022222222222"),
        ];
        let assignment = RoleAssignment::from_probed(&modems, &[], true);
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
        let assignment = RoleAssignment::from_probed(&modems, &overrides, true);
        assert!(assignment.circuit_switched.is_empty());
        assert_eq!(assignment.vowifi.len(), 1);
    }

    /// The invariant `supervise`'s discovery-retry loop is built on: the
    /// identifier an override is reported under while it is missing
    /// (`unmatched_overrides` → `FailedLine::card_id`) is *the same string*
    /// it is reported under once it resolves
    /// (`resolve_lines` → `ResolvedLine::configured_identifier`). If these
    /// two ever disagree, a recovered line is never recognised as the one
    /// that was being waited for — which is exactly what the first P1
    /// review finding on specs/027-discover-retry-health was.
    #[test]
    fn a_modem_port_overrides_identifier_is_the_same_missing_and_resolved() {
        let over = VowifiLineOverride {
            modem_port: Some("/dev/ttyUSB3".to_string()),
            ..Default::default()
        };
        let base = VowifiConfig {
            line_overrides: vec![over.clone()],
            ..Default::default()
        };

        // Missing: nothing probed at all.
        let missing = unmatched_overrides(&base.line_overrides, &[]);
        assert_eq!(missing.len(), 1);
        let while_missing = missing[0].card_id.clone();

        // Resolved: the pinned port now answers.
        let modems = vec![ready_modem(
            "ec20-ABCDEF",
            "/dev/ttyUSB3",
            false,
            "404011111111111",
        )];
        let assignment = RoleAssignment::from_probed(&modems, &base.line_overrides, false);
        let result = resolve_lines(&assignment, &base);
        assert!(unmatched_overrides(&base.line_overrides, &modems).is_empty());
        assert_eq!(result.lines.len(), 1);
        let when_resolved = result.lines[0]
            .configured_identifier
            .clone()
            .expect("a pinned line must record which override produced it");

        assert_eq!(
            while_missing, when_resolved,
            "the identifier must survive the missing → resolved transition unchanged"
        );
    }

    /// Same invariant for a `modem_serial` pin, where the resolved line's
    /// `card_id` is a *lossy* derivation of the serial — so the recorded
    /// identifier must be the serial itself, not something reconstructed
    /// from `card_id` (the second P1 review finding: two serials sharing
    /// their last six alphanumerics collapse to one `card_id`).
    #[test]
    fn a_modem_serial_overrides_identifier_is_the_raw_serial_not_the_derived_card_id() {
        let serial = "AAAAAAAAAAabcdef";
        let base = VowifiConfig {
            line_overrides: vec![VowifiLineOverride {
                modem_serial: Some(serial.to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };

        let while_missing = unmatched_overrides(&base.line_overrides, &[])[0]
            .card_id
            .clone();
        assert_eq!(while_missing, serial);

        let mut modem = ready_modem("ec20-IGNORED", "/dev/ttyUSB3", false, "404011111111111");
        modem.usb_serial = serial.to_string();
        modem.card_id = crate::modules::discovery::derive_module_id(serial);
        let modems = vec![modem];
        let assignment = RoleAssignment::from_probed(&modems, &base.line_overrides, false);
        let result = resolve_lines(&assignment, &base);
        assert_eq!(result.lines.len(), 1);

        assert_eq!(
            result.lines[0].configured_identifier.as_deref(),
            Some(serial),
            "the raw pinned serial must be recorded verbatim, not re-derived"
        );
        assert_ne!(
            result.lines[0].configured_identifier.as_deref(),
            Some(result.lines[0].card_id.as_str()),
            "fixture sanity: card_id is a lossy derivation and must not be what is recorded"
        );
    }

    /// An auto-discovered modem records no identifier, so it can never be
    /// mistaken for someone's pinned line.
    #[test]
    fn an_auto_discovered_line_records_no_configured_identifier() {
        let base = VowifiConfig::default();
        let modems = vec![ready_modem(
            "ec20-AAAAAA",
            "/dev/ttyUSB0",
            false,
            "404011111111111",
        )];
        let assignment = RoleAssignment::from_probed(&modems, &base.line_overrides, false);
        let result = resolve_lines(&assignment, &base);
        assert_eq!(result.lines.len(), 1);
        assert_eq!(result.lines[0].configured_identifier, None);
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
        let assignment = RoleAssignment::from_probed(&modems, &overrides, true);
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
        let assignment = RoleAssignment::from_probed(&modems, &[], true);
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
        let assignment = RoleAssignment::from_probed(&modems, &overrides, true);
        assert!(assignment.circuit_switched.is_empty());
        assert_eq!(assignment.vowifi.len(), 1);
    }

    #[test]
    fn unmatched_overrides_flags_a_modem_port_absent_from_the_probed_list() {
        let overrides = vec![VowifiLineOverride {
            modem_port: Some("/dev/ttyUSB3".to_string()),
            ..Default::default()
        }];
        let failed = unmatched_overrides(&overrides, &[]);
        assert_eq!(
            failed,
            vec![FailedLine::new("/dev/ttyUSB3", "not_found").configured(true)]
        );
    }

    #[test]
    fn unmatched_overrides_flags_a_modem_serial_absent_from_the_probed_list() {
        let overrides = vec![VowifiLineOverride {
            modem_serial: Some("ec20-AAAAAA".to_string()),
            ..Default::default()
        }];
        let failed = unmatched_overrides(&overrides, &[]);
        assert_eq!(
            failed,
            vec![FailedLine::new("ec20-AAAAAA", "not_found").configured(true)]
        );
    }

    #[test]
    fn unmatched_overrides_matches_by_serial_even_without_a_working_at_port() {
        // A device pinned by serial is "seen" the moment it's on the USB
        // bus at all — usb_serial comes from sysfs, independent of whether
        // any AT-capable interface was found. Port-based matching can't
        // make this same claim (there's no port to compare against a
        // device that never got one), so serial is the identity that
        // survives a device answering nothing at all.
        let overrides = vec![VowifiLineOverride {
            modem_serial: Some("ec20-AAAAAA".to_string()),
            ..Default::default()
        }];
        let probed = vec![unusable_modem("ec20-AAAAAA", None)];
        assert!(unmatched_overrides(&overrides, &probed).is_empty());
    }

    #[test]
    fn unmatched_overrides_matches_by_at_port_not_by_card_id() {
        let overrides = vec![VowifiLineOverride {
            modem_port: Some("/dev/ttyUSB3".to_string()),
            ..Default::default()
        }];
        let probed = vec![ready_modem(
            "ec20-AAAAAA",
            "/dev/ttyUSB3",
            false,
            "404011111111111",
        )];
        assert!(unmatched_overrides(&overrides, &probed).is_empty());
    }

    #[test]
    fn unmatched_overrides_excludes_pcsc_reader_lines() {
        let overrides = vec![VowifiLineOverride {
            pcsc_reader: true,
            imsi_override: Some("404011111111111".to_string()),
            ..Default::default()
        }];
        assert!(unmatched_overrides(&overrides, &[]).is_empty());
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

    /// specs/026-disable-circuit-switched, greptile P1: an explicit
    /// `[[vowifi.line]]` override must never be displaced by auto-discovered
    /// modems that merely sort earlier by card id — the scenario `[cs].
    /// enabled = false` makes newly reachable by enlarging the auto-
    /// discovered pool with modems the circuit-switched path used to
    /// reserve for itself.
    #[test]
    fn resolve_lines_never_displaces_an_explicit_override_with_auto_discovered_modems() {
        let base = VowifiConfig {
            max_lines: 2,
            line_overrides: vec![VowifiLineOverride {
                modem_serial: Some("ec20-ZZZZZZ".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        // Two unpinned candidates that sort ahead of the pinned one by card
        // id alone, plus the pinned one itself — three candidates for two
        // slots.
        let modems = vec![
            ready_modem("ec20-AAAAAA", "/dev/ttyUSB0", true, "1"),
            ready_modem("ec20-BBBBBB", "/dev/ttyUSB1", true, "2"),
            ready_modem("ec20-ZZZZZZ", "/dev/ttyUSB2", true, "3"),
        ];
        let assignment = RoleAssignment::from_probed(&modems, &base.line_overrides, false);
        let result = resolve_lines(&assignment, &base);

        let ids: Vec<&str> = result.lines.iter().map(|l| l.card_id.as_str()).collect();
        assert!(
            ids.contains(&"ec20-ZZZZZZ"),
            "the explicitly pinned modem must always get a line: got {ids:?}"
        );
        assert_eq!(result.lines.len(), 2);
        assert_eq!(
            result
                .failed
                .iter()
                .filter(|f| f.reason == "max_lines_exceeded")
                .count(),
            1
        );
    }

    /// A `pcsc_reader` line is always explicit configuration — there is no
    /// such thing as an auto-discovered card-reader line — so it must never
    /// lose its capacity to auto-discovered modems either, even when they
    /// alone would fill every available slot.
    #[test]
    fn resolve_lines_reserves_capacity_for_pcsc_lines_against_auto_discovered_modems() {
        let base = VowifiConfig {
            max_lines: 1,
            line_overrides: vec![VowifiLineOverride {
                pcsc_reader: true,
                imsi_override: Some("404940123456789".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        // A single auto-discovered modem candidate would, on its own,
        // consume the entire max_lines=1 budget before the pcsc line ever
        // gets a chance.
        let modems = vec![ready_modem("ec20-AAAAAA", "/dev/ttyUSB0", true, "1")];
        let assignment = RoleAssignment::from_probed(&modems, &base.line_overrides, false);
        let result = resolve_lines(&assignment, &base);

        assert_eq!(result.lines.len(), 1);
        assert_eq!(result.lines[0].card_id, "pcsc0");
        assert!(result
            .failed
            .iter()
            .any(|f| f.card_id == "ec20-AAAAAA" && f.reason == "max_lines_exceeded"));
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
        let assignment = RoleAssignment::from_probed(&modems, &[], true);
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
        let assignment = RoleAssignment::from_probed(&modems, &overrides, true);
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
    fn resolve_lines_pcsc_without_a_configured_plmn_leaves_it_to_be_derived() {
        // mcc/mnc are optional on a pcsc line: both come from the card's own
        // EF_IMSI/EF_AD. Empty here is the same "auto-derive" sentinel a
        // modem line uses, which is what makes `resolve_mcc_mnc` and
        // `ims::agent`'s startup path go read the card.
        let base = VowifiConfig {
            line_overrides: vec![VowifiLineOverride {
                pcsc_reader: true,
                imsi_override: Some("404438083996440".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let assignment = RoleAssignment {
            circuit_switched: vec![],
            vowifi: vec![],
        };
        let result = resolve_lines(&assignment, &base);
        assert_eq!(result.lines.len(), 1);
        let line = &result.lines[0];
        assert!(line.pcsc_reader);
        assert_eq!(line.mcc, "", "must not be invented at resolution time");
        assert_eq!(line.mnc, "");
        assert_eq!(line.config.mcc, "");
        assert_eq!(line.config.mnc, "");
        assert_eq!(line.imsi_override.as_deref(), Some("404438083996440"));
        // The IMEI is still auto-generated — that one genuinely isn't on the
        // card, so an empty mcc/mnc must not have disturbed it.
        assert_eq!(
            line.config.imei_override.as_deref().map(str::len),
            Some(15),
            "a pcsc line still needs a synthetic IMEI for +sip.instance"
        );
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
        // Regression test for a live failure in exactly this configuration.
        // The two resolvers used to derive resources through copy-pasted
        // blocks, and they drifted: `pcscf_source_path` was made per-line on
        // the modem path only, so the card-reader line kept the shared base and
        // went on overwriting the modem line's tunnel-assigned P-CSCF.
        assert_ne!(
            modem_line.config.pcscf_source_path,
            pcsc_line.config.pcscf_source_path
        );
        assert_eq!(
            modem_line.config.pcscf_source_path,
            format!("{}-0", base.pcscf_source_path)
        );
        assert_eq!(
            pcsc_line.config.pcscf_source_path,
            format!("{}-1", base.pcscf_source_path)
        );
    }

    #[test]
    fn resolve_lines_pcsc_overflow_reported_like_excess_modem_line() {
        // specs/023-omnikey-pcsc-vowifi US2 (T018): pcsc lines share
        // max_lines with modem lines, not an unbounded separate pool.
        //
        // Which one wins a contested slot changed under specs/026-disable-
        // circuit-switched (greptile P1): a pcsc_reader entry is *always*
        // explicit configuration — there is no such thing as an
        // auto-discovered card-reader line — so it must never lose to a
        // modem that only reached this candidate pool by auto-discovery
        // (no matching [[vowifi.line]] override). The modem here has none,
        // so the pcsc line now wins and the modem is the one reported as
        // max_lines_exceeded — the reverse of this test's original
        // assertion, which predates that guarantee.
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
        assert!(
            result.lines[0].pcsc_reader,
            "the pcsc line — always explicit config — wins the slot over an auto-discovered modem"
        );
        assert_eq!(result.failed.len(), 1);
        assert_eq!(result.failed[0].card_id, "ec20-AAAAAA");
        assert_eq!(result.failed[0].reason, "max_lines_exceeded");
    }

    /// Companion to the above: an auto-discovered modem competing against a
    /// pcsc line for the last slot loses; but the *same* modem, pinned by an
    /// explicit [[vowifi.line]] override, wins instead — the pcsc
    /// reservation only outranks *unpinned* modems, not explicit ones.
    #[test]
    fn resolve_lines_pinned_modem_still_beats_a_pcsc_line_for_a_contested_slot() {
        let base = VowifiConfig {
            max_lines: 1,
            line_overrides: vec![
                pcsc_override("404940123456789", "404", "043"),
                VowifiLineOverride {
                    modem_serial: Some("ec20-AAAAAA".to_string()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let modems = vec![ready_modem(
            "ec20-AAAAAA",
            "/dev/ttyUSB0",
            false,
            "404011111111111",
        )];
        let assignment = RoleAssignment::from_probed(&modems, &base.line_overrides, true);
        let result = resolve_lines(&assignment, &base);

        assert_eq!(result.lines.len(), 1);
        assert_eq!(result.lines[0].card_id, "ec20-AAAAAA");
        assert_eq!(result.failed.len(), 1);
        assert_eq!(result.failed[0].card_id, "pcsc0");
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
