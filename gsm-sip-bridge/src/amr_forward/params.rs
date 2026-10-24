//! The parameters one AMR leg negotiated and the rule that decides whether two
//! legs can have their frames forwarded unchanged (FR-003).

use crate::ims::amr_rtp::AmrKind;
use serde::{Deserialize, Serialize};

/// One frame is 20 ms of audio.
const FRAME_MS: u32 = 20;

/// Why a call that could have been forwarded was transcoded instead. Each
/// variant has a stable [`Reason::as_str`] used verbatim in logs and in
/// `CallRecord.media_path` (`contracts/logs.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Disabled,
    Outbound,
    CarrierNotAmr,
    ClientNotAmr,
    FlavourDiffers,
    ModeSet,
    Feature,
    Ptime,
    Dtmf,
    AckTimeout,
    ReinviteFailed,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Disabled => "disabled",
            Reason::Outbound => "outbound",
            Reason::CarrierNotAmr => "carrier_not_amr",
            Reason::ClientNotAmr => "client_not_amr",
            Reason::FlavourDiffers => "flavour_differs",
            Reason::ModeSet => "mode_set",
            Reason::Feature => "feature",
            Reason::Ptime => "ptime",
            Reason::Dtmf => "dtmf",
            Reason::AckTimeout => "ack_timeout",
            Reason::ReinviteFailed => "reinvite_failed",
        }
    }
}

impl Reason {
    /// The inverse of [`Reason::as_str`], for a reason named by the other
    /// agent on the control channel.
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "disabled" => Reason::Disabled,
            "outbound" => Reason::Outbound,
            "carrier_not_amr" => Reason::CarrierNotAmr,
            "client_not_amr" => Reason::ClientNotAmr,
            "flavour_differs" => Reason::FlavourDiffers,
            "mode_set" => Reason::ModeSet,
            "feature" => Reason::Feature,
            "ptime" => Reason::Ptime,
            "dtmf" => Reason::Dtmf,
            "ack_timeout" => Reason::AckTimeout,
            "reinvite_failed" => Reason::ReinviteFailed,
            _ => return None,
        })
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What to do about a carrier `mode-set` the client did not echo
/// (`[vowifi].amr_mode_set`, #111). Carried on the carrier leg's params, which
/// both agents already hold, so every `compatible` call and Agent A's relay
/// see the same policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeSetPolicy {
    /// FR-003 (b) as specified: the client's modes must equal the carrier's.
    #[default]
    Strict,
    /// Forward when the carrier's modes are within the client's, and count the
    /// client frames that fall outside the carrier's set (still sent).
    Relax,
    /// As `Relax`, and drop those frames instead of sending them.
    RelaxDrop,
}

impl ModeSetPolicy {
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "strict" => Self::Strict,
            "relax" => Self::Relax,
            "relax_drop" => Self::RelaxDrop,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Relax => "relax",
            Self::RelaxDrop => "relax_drop",
        }
    }
}

/// What one leg negotiated. Built once from the carrier's SDP (Agent A) and
/// once from the client's answered call (Agent B), serialised in the control
/// protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AmrLegParams {
    pub flavour: AmrKind,
    /// The packing on this leg. May differ between legs.
    pub octet_aligned: bool,
    /// Bit `n` set = mode `n` allowed. `None` = unrestricted (every mode of
    /// the flavour).
    #[serde(default)]
    pub mode_set: Option<u16>,
    /// Frames per packet this leg sends (from `ptime`; 1 for the usual 20 ms).
    pub frames_per_packet: u8,
    /// Most frames per packet this leg accepts (from `maxptime`, else what it
    /// asked for with `ptime`).
    pub max_frames_per_packet: u8,
    #[serde(default)]
    pub crc: bool,
    #[serde(default)]
    pub robust_sorting: bool,
    #[serde(default)]
    pub interleaving: bool,
    /// Redundancy (`max-red` above zero).
    #[serde(default)]
    pub redundancy: bool,
    pub payload_type: u8,
    #[serde(default)]
    pub dtmf_payload_type: Option<u8>,
    /// Only meaningful on the carrier leg; set by Agent A from config after
    /// parsing the offer. Absent in an older peer's message = `Strict`.
    #[serde(default)]
    pub mode_set_policy: ModeSetPolicy,
}

/// The pieces of a leg's SDP that [`AmrLegParams::from_sdp`] reads.
#[derive(Debug, Clone, Copy)]
pub struct SdpLeg<'a> {
    pub flavour: AmrKind,
    /// The payload type's `a=fmtp` parameters, verbatim (may be empty).
    pub fmtp: &'a str,
    pub payload_type: u8,
    pub dtmf_payload_type: Option<u8>,
    pub ptime_ms: Option<u32>,
    pub maxptime_ms: Option<u32>,
}

impl AmrKind {
    /// Number of bitrate modes the flavour has (TS 26.101 / 26.201).
    fn mode_count(self) -> u32 {
        match self {
            AmrKind::Nb => 8,
            AmrKind::Wb => 9,
        }
    }

    pub fn clock_rate(self) -> u32 {
        match self {
            AmrKind::Nb => 8000,
            AmrKind::Wb => 16000,
        }
    }

    /// `amr-wb` / `amr-nb`, as written in logs and call records.
    pub fn label(self) -> &'static str {
        match self {
            AmrKind::Nb => "amr-nb",
            AmrKind::Wb => "amr-wb",
        }
    }

    fn all_modes(self) -> u16 {
        ((1u32 << self.mode_count()) - 1) as u16
    }
}

fn frames_for(ms: u32) -> u8 {
    (ms / FRAME_MS).clamp(1, u8::MAX as u32) as u8
}

impl AmrLegParams {
    /// Parse a leg from its SDP. A parameter that is present but malformed
    /// (a `mode-set` naming a mode the flavour doesn't have, or an unparseable
    /// list) is `Err(Reason::Feature)`: the call is transcoded rather than
    /// guessed at.
    pub fn from_sdp(leg: &SdpLeg<'_>) -> Result<Self, Reason> {
        let mut octet_aligned = false;
        let mut mode_set = None;
        let (mut crc, mut robust, mut interleaving, mut redundancy) = (false, false, false, false);

        for param in leg.fmtp.split(';') {
            let param = param.trim();
            let Some((key, value)) = param.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
            match key.as_str() {
                "octet-align" => octet_aligned = value == "1",
                "mode-set" => mode_set = Some(parse_mode_set(leg.flavour, value)?),
                "crc" => crc = value == "1",
                "robust-sorting" => robust = value == "1",
                // Interleaving is a depth: any value, even 0, names the
                // feature, but `0` is not in use.
                "interleaving" => interleaving = value.parse::<u32>().map_or(true, |d| d > 0),
                "max-red" => redundancy = value.parse::<u32>().map_or(true, |ms| ms > 0),
                _ => {}
            }
        }

        let frames = leg.ptime_ms.map_or(1, frames_for);
        let max_frames = leg.maxptime_ms.map_or(frames, frames_for).max(frames);

        Ok(Self {
            flavour: leg.flavour,
            octet_aligned,
            mode_set,
            frames_per_packet: frames,
            max_frames_per_packet: max_frames,
            crc,
            robust_sorting: robust,
            interleaving,
            redundancy,
            payload_type: leg.payload_type,
            dtmf_payload_type: leg.dtmf_payload_type,
            mode_set_policy: ModeSetPolicy::Strict,
        })
    }

    /// This leg with the policy for its `mode-set` applied.
    pub fn with_mode_set_policy(mut self, policy: ModeSetPolicy) -> Self {
        self.mode_set_policy = policy;
        self
    }

    /// The speech modes the relay lets through towards this (carrier) leg, or
    /// `None` when the leg is unrestricted or the policy is `Strict` (the
    /// compatibility rule already guarantees the sets match).
    pub fn mode_guard(&self) -> Option<(u16, bool)> {
        match (self.mode_set_policy, self.mode_set) {
            (ModeSetPolicy::Relax, Some(m)) => Some((m, false)),
            (ModeSetPolicy::RelaxDrop, Some(m)) => Some((m, true)),
            _ => None,
        }
    }

    pub fn clock_rate(&self) -> u32 {
        self.flavour.clock_rate()
    }

    /// The modes this leg allows, with `None` expanded to every mode.
    fn modes(&self) -> u16 {
        self.mode_set.unwrap_or_else(|| self.flavour.all_modes())
    }

    fn uses_feature(&self) -> bool {
        self.crc || self.robust_sorting || self.interleaving || self.redundancy
    }
}

fn parse_mode_set(flavour: AmrKind, value: &str) -> Result<u16, Reason> {
    let mut mask = 0u16;
    for part in value.split(',') {
        let mode: u32 = part.trim().parse().map_err(|_| Reason::Feature)?;
        if mode >= flavour.mode_count() {
            return Err(Reason::Feature);
        }
        mask |= 1 << mode;
    }
    if mask == 0 {
        return Err(Reason::Feature);
    }
    Ok(mask)
}

/// FR-003. Whether frames can cross the bridge between `carrier` and `client`
/// unchanged (apart from packing and payload-type relabelling).
///
/// A leg's `mode-set` is the set both sides of that leg use: the receiver's
/// declaration restricts what the peer may send it, and each answer echoes the
/// offered set. So forwarding is safe only when what each side may send is
/// within what the other accepts, which with one set per leg means the sets
/// are equal, an unrestricted set counting as every mode.
pub fn compatible(carrier: &AmrLegParams, client: &AmrLegParams) -> Result<(), Reason> {
    // (a) same flavour
    if carrier.flavour != client.flavour {
        return Err(Reason::FlavourDiffers);
    }
    // (c) no feature in use on either leg
    if carrier.uses_feature() || client.uses_feature() {
        return Err(Reason::Feature);
    }
    // (b) each sender's modes within the receiver's, both directions
    // Relaxed: only what the carrier sends must be acceptable to the client;
    // the client's own frames are policed by the relay (`mode_guard`).
    let (c, k) = (carrier.modes(), client.modes());
    let relaxed = carrier.mode_set_policy != ModeSetPolicy::Strict;
    if c & !k != 0 || (!relaxed && k & !c != 0) {
        return Err(Reason::ModeSet);
    }
    // (d) frames per packet each side sends fit the other's maximum
    if carrier.frames_per_packet > client.max_frames_per_packet
        || client.frames_per_packet > carrier.max_frames_per_packet
    {
        return Err(Reason::Ptime);
    }
    // DTMF: forwarding relabels events between the legs' telephone-event
    // types, so both legs must have one or neither (research R10).
    if carrier.dtmf_payload_type.is_some() != client.dtmf_payload_type.is_some() {
        return Err(Reason::Dtmf);
    }
    Ok(())
}

/// The outcome recorded for a call. Decided once, before media flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaPath {
    Forwarded {
        flavour: AmrKind,
        carrier_oa: bool,
        client_oa: bool,
    },
    Transcoded {
        reason: Reason,
    },
}

impl MediaPath {
    pub fn forwarded(carrier: &AmrLegParams, client: &AmrLegParams) -> Self {
        MediaPath::Forwarded {
            flavour: carrier.flavour,
            carrier_oa: carrier.octet_aligned,
            client_oa: client.octet_aligned,
        }
    }

    pub fn is_forwarded(&self) -> bool {
        matches!(self, MediaPath::Forwarded { .. })
    }

    /// The `CallRecord.media_path` string (`contracts/logs.md`):
    /// `forwarded:amr-wb:oa/be`, `transcoded:mode_set`.
    pub fn record_string(&self) -> String {
        match self {
            MediaPath::Forwarded {
                flavour,
                carrier_oa,
                client_oa,
            } => {
                let tag = |oa: bool| if oa { "oa" } else { "be" };
                format!(
                    "forwarded:{}:{}/{}",
                    flavour.label(),
                    tag(*carrier_oa),
                    tag(*client_oa)
                )
            }
            MediaPath::Transcoded { reason } => format!("transcoded:{}", reason.as_str()),
        }
    }

    /// The `media path decided` log fields after `call_id=`.
    pub fn log_fields(&self) -> String {
        match self {
            MediaPath::Forwarded {
                flavour,
                carrier_oa,
                client_oa,
            } => {
                let packing = |oa: bool| {
                    if oa {
                        "octet-aligned"
                    } else {
                        "bandwidth-efficient"
                    }
                };
                format!(
                    "path=forwarded flavour={} carrier_packing={} client_packing={}",
                    flavour.label(),
                    packing(*carrier_oa),
                    packing(*client_oa)
                )
            }
            MediaPath::Transcoded { reason } => {
                format!("path=transcoded reason={}", reason.as_str())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(flavour: AmrKind, fmtp: &str) -> AmrLegParams {
        AmrLegParams::from_sdp(&SdpLeg {
            flavour,
            fmtp,
            payload_type: 96,
            dtmf_payload_type: Some(101),
            ptime_ms: Some(20),
            maxptime_ms: None,
        })
        .unwrap()
    }

    fn wb(fmtp: &str) -> AmrLegParams {
        leg(AmrKind::Wb, fmtp)
    }

    // ----- parsing -----------------------------------------------------

    #[test]
    fn parses_packing_modes_and_flags() {
        let p = wb(
            "octet-align=1; mode-set=0,2,4; crc=1; robust-sorting=1; interleaving=5; max-red=40",
        );
        assert!(p.octet_aligned);
        assert_eq!(p.mode_set, Some(0b10101));
        assert!(p.crc && p.robust_sorting && p.interleaving && p.redundancy);
        assert_eq!((p.payload_type, p.dtmf_payload_type), (96, Some(101)));
        assert_eq!(p.clock_rate(), 16000);
        assert_eq!(leg(AmrKind::Nb, "").clock_rate(), 8000);
    }

    #[test]
    fn defaults_are_bandwidth_efficient_unrestricted_and_featureless() {
        let p = wb("");
        assert!(!p.octet_aligned);
        assert_eq!(p.mode_set, None);
        assert!(!p.crc && !p.robust_sorting && !p.interleaving && !p.redundancy);
        assert_eq!((p.frames_per_packet, p.max_frames_per_packet), (1, 1));
    }

    #[test]
    fn features_set_to_zero_are_not_in_use() {
        let p = wb("octet-align=1; crc=0; robust-sorting=0; interleaving=0; max-red=0");
        assert!(!p.crc && !p.robust_sorting && !p.interleaving && !p.redundancy);
    }

    #[test]
    fn ptime_and_maxptime_become_frame_counts() {
        let p = AmrLegParams::from_sdp(&SdpLeg {
            flavour: AmrKind::Wb,
            fmtp: "",
            payload_type: 96,
            dtmf_payload_type: None,
            ptime_ms: Some(40),
            maxptime_ms: Some(80),
        })
        .unwrap();
        assert_eq!((p.frames_per_packet, p.max_frames_per_packet), (2, 4));
    }

    #[test]
    fn parsing_tolerates_spacing_case_and_unknown_keys() {
        let p = wb("Octet-Align = 1 ;mode-change-capability=2; MODE-SET=1");
        assert!(p.octet_aligned);
        assert_eq!(p.mode_set, Some(0b10));
    }

    #[test]
    fn a_malformed_mode_set_is_a_feature_error() {
        for bad in ["mode-set=9", "mode-set=x", "mode-set=", "mode-set=0,,1"] {
            assert_eq!(
                AmrLegParams::from_sdp(&SdpLeg {
                    flavour: AmrKind::Wb,
                    fmtp: bad,
                    payload_type: 96,
                    dtmf_payload_type: None,
                    ptime_ms: None,
                    maxptime_ms: None,
                }),
                Err(Reason::Feature),
                "{bad}"
            );
        }
        // Mode 8 exists in wideband but not narrowband.
        assert!(AmrLegParams::from_sdp(&SdpLeg {
            flavour: AmrKind::Wb,
            fmtp: "mode-set=8",
            payload_type: 96,
            dtmf_payload_type: None,
            ptime_ms: None,
            maxptime_ms: None,
        })
        .is_ok());
        assert_eq!(
            AmrLegParams::from_sdp(&SdpLeg {
                flavour: AmrKind::Nb,
                fmtp: "mode-set=8",
                payload_type: 96,
                dtmf_payload_type: None,
                ptime_ms: None,
                maxptime_ms: None,
            }),
            Err(Reason::Feature)
        );
    }

    // ----- compatibility, FR-003 clause by clause, both directions ------

    fn both(a: &AmrLegParams, b: &AmrLegParams) -> (Result<(), Reason>, Result<(), Reason>) {
        (compatible(a, b), compatible(b, a))
    }

    #[test]
    fn identical_legs_are_compatible() {
        let (a, b) = (wb("octet-align=1"), wb("octet-align=1"));
        assert_eq!(both(&a, &b), (Ok(()), Ok(())));
    }

    #[test]
    fn packing_may_differ() {
        let (a, b) = (wb("octet-align=1"), wb(""));
        assert_eq!(both(&a, &b), (Ok(()), Ok(())));
    }

    #[test]
    fn different_flavours_are_not_compatible() {
        let (a, b) = (wb(""), leg(AmrKind::Nb, ""));
        assert_eq!(
            both(&a, &b),
            (Err(Reason::FlavourDiffers), Err(Reason::FlavourDiffers))
        );
    }

    #[test]
    fn narrowband_pairs_are_compatible() {
        let (a, b) = (
            leg(AmrKind::Nb, "octet-align=1"),
            leg(AmrKind::Nb, "mode-set=0,1,2,3,4,5,6,7"),
        );
        assert_eq!(both(&a, &b), (Ok(()), Ok(())));
    }

    #[test]
    fn mode_sets_must_match_in_effect() {
        let any = wb("octet-align=1");
        let low = wb("octet-align=1; mode-set=0,1,2");
        let lower = wb("octet-align=1; mode-set=0,1");
        let other = wb("octet-align=1; mode-set=3,4");
        let all = wb("octet-align=1; mode-set=0,1,2,3,4,5,6,7,8");

        // Equal sets, written differently, are the same set.
        assert_eq!(both(&any, &all), (Ok(()), Ok(())));
        assert_eq!(both(&low, &low.clone()), (Ok(()), Ok(())));
        // Unrestricted vs restricted: one side may send what the other refuses.
        assert_eq!(
            both(&any, &low),
            (Err(Reason::ModeSet), Err(Reason::ModeSet))
        );
        // Subset / superset.
        assert_eq!(
            both(&low, &lower),
            (Err(Reason::ModeSet), Err(Reason::ModeSet))
        );
        // Disjoint.
        assert_eq!(
            both(&low, &other),
            (Err(Reason::ModeSet), Err(Reason::ModeSet))
        );
    }

    #[test]
    fn a_relaxed_policy_only_requires_the_carriers_modes_within_the_clients() {
        let carrier = |policy| wb("octet-align=1; mode-set=0,1,2,3").with_mode_set_policy(policy);
        let any = wb("octet-align=1");
        let wide = wb("octet-align=1; mode-set=0,1,2,3,4");
        let narrow = wb("octet-align=1; mode-set=0,1");
        let strict = carrier(ModeSetPolicy::Strict);
        assert_eq!(compatible(&strict, &any), Err(Reason::ModeSet));
        for policy in [ModeSetPolicy::Relax, ModeSetPolicy::RelaxDrop] {
            let c = carrier(policy);
            assert_eq!(compatible(&c, &any), Ok(()));
            assert_eq!(compatible(&c, &wide), Ok(()));
            // The carrier may send modes the client refuses: still transcoded.
            assert_eq!(compatible(&c, &narrow), Err(Reason::ModeSet));
        }
    }

    #[test]
    fn the_mode_guard_exists_only_for_a_restricted_relaxed_carrier() {
        let restricted = wb("octet-align=1; mode-set=0,1,2,3");
        assert_eq!(restricted.mode_guard(), None);
        let relax = restricted
            .clone()
            .with_mode_set_policy(ModeSetPolicy::Relax);
        assert_eq!(relax.mode_guard(), Some((0b1111, false)));
        let drop = restricted.with_mode_set_policy(ModeSetPolicy::RelaxDrop);
        assert_eq!(drop.mode_guard(), Some((0b1111, true)));
        let open = wb("octet-align=1").with_mode_set_policy(ModeSetPolicy::Relax);
        assert_eq!(open.mode_guard(), None);
    }

    #[test]
    fn any_feature_on_either_leg_blocks_forwarding() {
        let plain = wb("octet-align=1");
        for flag in ["crc=1", "robust-sorting=1", "interleaving=3", "max-red=60"] {
            let featured = wb(&format!("octet-align=1; {flag}"));
            assert_eq!(
                both(&plain, &featured),
                (Err(Reason::Feature), Err(Reason::Feature)),
                "{flag}"
            );
            assert_eq!(compatible(&featured, &featured), Err(Reason::Feature));
        }
    }

    #[test]
    fn frames_per_packet_must_fit_the_receiver() {
        let one = wb("octet-align=1");
        let two = AmrLegParams::from_sdp(&SdpLeg {
            flavour: AmrKind::Wb,
            fmtp: "octet-align=1",
            payload_type: 96,
            dtmf_payload_type: Some(101),
            ptime_ms: Some(40),
            maxptime_ms: Some(40),
        })
        .unwrap();
        // two sends 2 frames, one accepts only 1.
        assert_eq!(both(&one, &two), (Err(Reason::Ptime), Err(Reason::Ptime)));
        // A receiver that accepts more is fine for a single-frame sender.
        let roomy = AmrLegParams::from_sdp(&SdpLeg {
            flavour: AmrKind::Wb,
            fmtp: "octet-align=1",
            payload_type: 96,
            dtmf_payload_type: Some(101),
            ptime_ms: Some(20),
            maxptime_ms: Some(80),
        })
        .unwrap();
        assert_eq!(both(&one, &roomy), (Ok(()), Ok(())));
    }

    #[test]
    fn dtmf_must_be_negotiated_on_both_legs_or_neither() {
        let with = wb("octet-align=1");
        let mut without = wb("octet-align=1");
        without.dtmf_payload_type = None;
        assert_eq!(
            both(&with, &without),
            (Err(Reason::Dtmf), Err(Reason::Dtmf))
        );
        assert_eq!(compatible(&without, &without.clone()), Ok(()));
    }

    #[test]
    fn the_first_failing_clause_is_the_reason() {
        // Flavour wins over everything else.
        let nb_featured = leg(AmrKind::Nb, "crc=1; mode-set=0");
        assert_eq!(
            compatible(&wb("octet-align=1"), &nb_featured),
            Err(Reason::FlavourDiffers)
        );
    }

    // ----- recorded outcome ---------------------------------------------

    #[test]
    fn reason_strings_are_the_ones_the_contract_names() {
        let all = [
            (Reason::Disabled, "disabled"),
            (Reason::Outbound, "outbound"),
            (Reason::CarrierNotAmr, "carrier_not_amr"),
            (Reason::ClientNotAmr, "client_not_amr"),
            (Reason::FlavourDiffers, "flavour_differs"),
            (Reason::ModeSet, "mode_set"),
            (Reason::Feature, "feature"),
            (Reason::Ptime, "ptime"),
            (Reason::Dtmf, "dtmf"),
            (Reason::AckTimeout, "ack_timeout"),
            (Reason::ReinviteFailed, "reinvite_failed"),
        ];
        for (reason, text) in all {
            assert_eq!(reason.as_str(), text);
            assert_eq!(reason.to_string(), text);
            assert_eq!(Reason::parse(text), Some(reason));
            assert_eq!(
                MediaPath::Transcoded { reason }.record_string(),
                format!("transcoded:{text}")
            );
            assert_eq!(
                MediaPath::Transcoded { reason }.log_fields(),
                format!("path=transcoded reason={text}")
            );
        }
    }

    #[test]
    fn forwarded_record_and_log_strings_show_each_legs_packing() {
        let path = MediaPath::forwarded(&wb("octet-align=1"), &wb(""));
        assert!(path.is_forwarded());
        assert_eq!(path.record_string(), "forwarded:amr-wb:oa/be");
        assert_eq!(
            path.log_fields(),
            "path=forwarded flavour=amr-wb carrier_packing=octet-aligned client_packing=bandwidth-efficient"
        );
        let nb = MediaPath::forwarded(
            &leg(AmrKind::Nb, "octet-align=1"),
            &leg(AmrKind::Nb, "octet-align=1"),
        );
        assert_eq!(nb.record_string(), "forwarded:amr-nb:oa/oa");
        assert!(!MediaPath::Transcoded {
            reason: Reason::Dtmf
        }
        .is_forwarded());
    }

    #[test]
    fn params_survive_the_wire() {
        let p = wb("octet-align=1; mode-set=0,2");
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"flavour\":\"wb\""), "{json}");
        assert_eq!(serde_json::from_str::<AmrLegParams>(&json).unwrap(), p);
    }
}
