//! What a "line" is, independent of how its calls are carried.
//!
//! VoWiFi and VoLTE were built a feature apart and independently reinvented
//! the same pipeline: *probe modems → classify which ones can become lines and
//! why the rest cannot → order them stably → cap the count → derive each
//! line's isolated resources from its index → serialise a manifest the
//! per-line child processes read back*. Only the last two steps are actually
//! transport-specific.
//!
//! The duplication had started to cost real things, not just lines of code:
//!
//! - `shift_ipv4` existed twice, byte-identical.
//! - `volte::discovery` imported `FailedLine` from `vowifi::discovery` — a
//!   layering inversion where the LTE path depended on the Wi-Fi path for a
//!   type that belongs to neither.
//! - `modules::discovery` kept *private copies* of VoLTE's manifest path and
//!   env-var constants, with a comment explaining the layering dodge: two
//!   sources of truth for a path shared across process boundaries.
//! - The two subsystems had silently diverged on index-0 naming (see
//!   [`resources::indexed`]).
//!
//! Each transport now contributes only its own per-line payload; everything
//! above is here and tested once.

pub mod manifest;
pub mod resources;

use crate::modules::discovery::{ProbedModem, SimStatus};

/// A modem (or reader) that cannot become a line, and why.
///
/// The `reason` strings are a reported interface — they reach operators
/// through `discover`'s output and the metrics labels — so they are stable
/// identifiers, not prose.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct FailedLine {
    pub card_id: String,
    pub reason: String,
    /// Whether this candidate matched an explicit `[[vowifi.line]]`
    /// `modem_port`/`modem_serial`/`pcsc_reader` override, as opposed to an
    /// unpinned, auto-discovered modem that merely failed classification or
    /// lost a capacity cut (specs/027-discover-retry-health review finding:
    /// `resolve_lines`'s classify-failure loop used to push every rejected
    /// modem — pinned or not — with no way to tell them apart afterward,
    /// which made `vowifi::is_configured_line_failure` mislabel an
    /// auto-discovered modem's `sim_unreadable`/`no_at_port`/etc. as a
    /// "configured line from config.toml", sending operators looking for a
    /// config entry that does not exist). Defaults to `false` via `new` so
    /// every pre-existing call site — VoLTE's `select`, and every test that
    /// does not care about provenance — is unaffected; call sites that do
    /// know a candidate is pinned/overridden opt in via `.configured(true)`.
    pub configured: bool,
}

impl FailedLine {
    pub fn new(card_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            card_id: card_id.into(),
            reason: reason.into(),
            configured: false,
        }
    }

    pub fn configured(mut self, configured: bool) -> Self {
        self.configured = configured;
        self
    }
}

/// Why a probed modem was rejected. One variant per `reason` string, so the
/// strings are produced in exactly one place rather than spelled out at each
/// classification site (they were, twice, and had already drifted: VoWiFi
/// reported a missing AT port as `no_at_port` only when `sim_status` was
/// `None`, VoLTE also when the SIM was `Ready` but the port was missing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    SimAbsent,
    SimLocked,
    SimUnreadable(String),
    NoAtPort,
    MaxLinesExceeded,
    /// A configured override (`modem_port`/`modem_serial`/`pcsc_reader`)
    /// matched no probed device on the most recent discovery pass —
    /// distinct from `NoAtPort` (device found, nothing answers AT) and the
    /// `Sim*` variants (device found, SIM problem). specs/027-discover-
    /// retry-health.
    NotFound,
}

impl Rejection {
    pub fn reason(&self) -> String {
        match self {
            Rejection::SimAbsent => "sim_absent".to_string(),
            Rejection::SimLocked => "sim_locked".to_string(),
            Rejection::SimUnreadable(msg) => format!("sim_unreadable: {msg}"),
            Rejection::NoAtPort => "no_at_port".to_string(),
            Rejection::MaxLinesExceeded => "max_lines_exceeded".to_string(),
            Rejection::NotFound => "not_found".to_string(),
        }
    }
}

/// Whether a candidate needs a working AT port to be usable as a line.
///
/// VoWiFi's `Ready` classification did not check the port because its role
/// assignment had already established one; VoLTE's did. Making it an explicit
/// parameter keeps both behaviours available and named, rather than leaving
/// the difference implicit in two near-identical `match` arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtPortRequirement {
    Required,
    AlreadyEstablished,
}

/// Classifies one probed modem: `Ok` if it can become a line, `Err` with the
/// reason if not.
pub fn classify(modem: &ProbedModem, at_port: AtPortRequirement) -> Result<(), Rejection> {
    match &modem.sim_status {
        Some(SimStatus::Ready { .. }) => {
            if at_port == AtPortRequirement::Required && modem.at_port.is_none() {
                Err(Rejection::NoAtPort)
            } else {
                Ok(())
            }
        }
        Some(SimStatus::Absent) => Err(Rejection::SimAbsent),
        Some(SimStatus::Locked) => Err(Rejection::SimLocked),
        Some(SimStatus::Unreadable(msg)) => Err(Rejection::SimUnreadable(msg.clone())),
        None => Err(Rejection::NoAtPort),
    }
}

/// The candidates that survived classification and capping, plus everything
/// that did not and why.
#[derive(Debug, Clone)]
pub struct Candidates<'a> {
    /// Ready candidates, ordered by card id and capped at `max_lines`. Index
    /// in this vector is the line index.
    pub kept: Vec<&'a ProbedModem>,
    pub failed: Vec<FailedLine>,
}

/// Turns a set of probed modems into an ordered, bounded candidate list.
///
/// The returned `kept` list is **always in plain card-id order, regardless
/// of pin status** — enumeration order varies between boots, and a line's
/// index determines its namespace, veth addresses, and ports, so an
/// unstable order would silently reassign a SIM's entire network identity
/// across a restart (greptile P1 on specs/026-disable-circuit-switched: an
/// earlier version of this function sorted pinned candidates ahead of
/// unpinned ones in the *returned* order, which reassigned every kept
/// line's index — and therefore its whole identity — the moment a pinned
/// and an unpinned candidate's relative card-id order disagreed with their
/// pin status, even with no actual `max_lines` contention at all).
///
/// `is_pinned` — true for a modem an explicit `[[...line]]` override names
/// by `modem_serial`/`modem_port` — affects only *which* candidates survive
/// a capacity cut, never their order: when the pool exceeds `max_lines`,
/// pinned candidates are preferred for the remaining slots over unpinned
/// ones, but the survivors are still returned in the same card-id order
/// they would have been in without any pinning concept at all. Passing
/// `|_| false` when the caller has no such concept is equivalent to that
/// plain by-card-id behaviour throughout. Without the pin preference, a
/// pool enlarged by auto-discovery (e.g. specs/026-disable-circuit-switched
/// freeing modems a disabled circuit-switched path no longer reserves)
/// could bump an operator's explicit pin out to `max_lines_exceeded` in
/// favor of a modem that merely sorts earlier alphabetically.
///
/// Overflow past `max_lines` is *reported* as `max_lines_exceeded` rather
/// than silently dropped, so an operator who plugs in a fifth modem with
/// `max_lines = 4` sees why it did nothing.
pub fn select<'a>(
    modems: impl IntoIterator<Item = &'a ProbedModem>,
    max_lines: u32,
    at_port: AtPortRequirement,
    is_pinned: impl Fn(&ProbedModem) -> bool,
) -> Candidates<'a> {
    let mut failed = Vec::new();
    let mut ready: Vec<&ProbedModem> = Vec::new();

    for modem in modems {
        match classify(modem, at_port) {
            Ok(()) => ready.push(modem),
            Err(r) => failed.push(FailedLine::new(modem.card_id.clone(), r.reason())),
        }
    }

    // The order every caller actually indexes by. Established once, up
    // front, so it's what gets returned whether or not a cut is needed.
    ready.sort_by(|a, b| a.card_id.cmp(&b.card_id));

    let max_lines = max_lines as usize;
    let kept = if ready.len() <= max_lines {
        ready
    } else {
        // Decide who survives using pin priority — a clone sorted by pin
        // status alone; stable sort preserves the card-id order already
        // established above within each tier. Only the *membership* of the
        // first `max_lines` of that ranking is used; the ordering of
        // `ready` itself, and therefore of the returned `kept`, is
        // untouched by pin status.
        let mut by_priority = ready.clone();
        by_priority.sort_by_key(|m| std::cmp::Reverse(is_pinned(m)));
        let survivor_ids: std::collections::HashSet<&str> = by_priority[..max_lines]
            .iter()
            .map(|m| m.card_id.as_str())
            .collect();

        let (kept, overflow): (Vec<&ProbedModem>, Vec<&ProbedModem>) = ready
            .into_iter()
            .partition(|m| survivor_ids.contains(m.card_id.as_str()));
        for modem in overflow {
            failed.push(FailedLine::new(
                modem.card_id.clone(),
                Rejection::MaxLinesExceeded.reason(),
            ));
        }
        kept
    };

    Candidates { kept, failed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn modem(card_id: &str, sim: Option<SimStatus>, at_port: bool) -> ProbedModem {
        ProbedModem {
            card_id: card_id.to_string(),
            model: "EC20",
            usb_serial: card_id.to_string(),
            has_audio_capability: true,
            audio_device: None,
            net_device: None,
            at_port: at_port.then(|| PathBuf::from("/dev/ttyUSB0")),
            sim_status: sim,
        }
    }

    fn ready(card_id: &str) -> ProbedModem {
        modem(
            card_id,
            Some(SimStatus::Ready {
                imsi: "404940123456789".to_string(),
            }),
            true,
        )
    }

    #[test]
    fn candidates_are_ordered_by_card_id_not_by_probe_order() {
        // Probe order deliberately scrambled: USB enumeration order varies
        // between boots, and index determines a line's whole network identity.
        let m = [
            ready("ec20-CCCCCC"),
            ready("ec20-AAAAAA"),
            ready("ec20-BBBBBB"),
        ];
        let c = select(&m, 8, AtPortRequirement::Required, |_| false);

        let ids: Vec<&str> = c.kept.iter().map(|m| m.card_id.as_str()).collect();
        assert_eq!(ids, ["ec20-AAAAAA", "ec20-BBBBBB", "ec20-CCCCCC"]);
        assert!(c.failed.is_empty());
    }

    #[test]
    fn overflow_past_max_lines_is_reported_not_silently_dropped() {
        let m = [ready("a"), ready("b"), ready("c"), ready("d")];
        let c = select(&m, 2, AtPortRequirement::Required, |_| false);

        assert_eq!(c.kept.len(), 2);
        assert_eq!(
            c.failed,
            vec![
                FailedLine::new("c", "max_lines_exceeded"),
                FailedLine::new("d", "max_lines_exceeded"),
            ]
        );
    }

    /// The cap drops the *last* candidates in card-id order, so which modem
    /// keeps its line is stable across restarts rather than depending on
    /// which one happened to enumerate first.
    #[test]
    fn the_cap_keeps_the_lowest_card_ids_regardless_of_probe_order() {
        let m = [ready("d"), ready("b"), ready("a"), ready("c")];
        let c = select(&m, 2, AtPortRequirement::Required, |_| false);

        let ids: Vec<&str> = c.kept.iter().map(|m| m.card_id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
    }

    /// A pinned candidate must never lose its slot to an unpinned one that
    /// merely sorts earlier by card id — the gap specs/026-disable-
    /// circuit-switched surfaced: freeing previously CS-reserved modems to
    /// VoWiFi enlarges the auto-discovered pool, and without this priority
    /// an operator's explicit override could be silently bumped out. The
    /// *returned* order is still plain card-id order regardless of which
    /// tier won a candidate its slot — pin status decides membership only.
    #[test]
    fn a_pinned_candidate_is_kept_over_unpinned_ones_that_sort_earlier() {
        let m = [
            ready("aaa-unpinned"),
            ready("bbb-unpinned"),
            ready("zzz-pinned"),
        ];
        let c = select(&m, 2, AtPortRequirement::Required, |modem| {
            modem.card_id == "zzz-pinned"
        });

        let ids: Vec<&str> = c.kept.iter().map(|m| m.card_id.as_str()).collect();
        assert_eq!(
            ids,
            ["aaa-unpinned", "zzz-pinned"],
            "zzz-pinned must survive the cut, but the kept list stays card-id ordered"
        );
        assert_eq!(
            c.failed,
            vec![FailedLine::new("bbb-unpinned", "max_lines_exceeded")]
        );
    }

    /// specs/026-disable-circuit-switched, greptile P1 (second finding): with
    /// no actual `max_lines` contention, pin status must not reorder the
    /// kept list at all — a pinned candidate whose card id sorts *after* an
    /// unpinned one must not jump ahead of it, since a line's index drives
    /// its whole network identity (namespace, veth, ports) and reassigning
    /// it on every upgrade — even where nothing was ever at risk of being
    /// excluded — would needlessly tear down and rebuild a working line.
    #[test]
    fn pin_status_never_reorders_the_kept_list_when_nothing_is_cut() {
        let m = [ready("aaa-unpinned"), ready("zzz-pinned")];
        let c = select(&m, 8, AtPortRequirement::Required, |modem| {
            modem.card_id == "zzz-pinned"
        });

        let ids: Vec<&str> = c.kept.iter().map(|m| m.card_id.as_str()).collect();
        assert_eq!(
            ids,
            ["aaa-unpinned", "zzz-pinned"],
            "both fit under max_lines, so plain card-id order must be unchanged by pinning"
        );
        assert!(c.failed.is_empty());
    }

    #[test]
    fn max_lines_zero_keeps_nothing_and_reports_everything() {
        let m = [ready("a"), ready("b")];
        let c = select(&m, 0, AtPortRequirement::Required, |_| false);

        assert!(c.kept.is_empty());
        assert_eq!(c.failed.len(), 2);
    }

    #[test]
    fn each_unusable_sim_state_gets_its_own_stable_reason() {
        let m = [
            modem("absent", Some(SimStatus::Absent), true),
            modem("locked", Some(SimStatus::Locked), true),
            modem(
                "unreadable",
                Some(SimStatus::Unreadable("CME 13".to_string())),
                true,
            ),
            modem("noprobe", None, false),
        ];
        let c = select(&m, 8, AtPortRequirement::Required, |_| false);

        assert!(c.kept.is_empty());
        assert_eq!(
            c.failed,
            vec![
                FailedLine::new("absent", "sim_absent"),
                FailedLine::new("locked", "sim_locked"),
                FailedLine::new("unreadable", "sim_unreadable: CME 13"),
                FailedLine::new("noprobe", "no_at_port"),
            ]
        );
    }

    #[test]
    fn not_found_reason_is_stable() {
        assert_eq!(Rejection::NotFound.reason(), "not_found");
    }

    /// The one genuine behavioural difference between the two subsystems,
    /// now named instead of implicit in two near-identical match arms.
    #[test]
    fn a_ready_sim_with_no_at_port_is_rejected_only_when_the_port_is_required() {
        let m = [modem(
            "x",
            Some(SimStatus::Ready {
                imsi: "404940123456789".to_string(),
            }),
            false,
        )];

        assert_eq!(
            select(&m, 8, AtPortRequirement::Required, |_| false)
                .kept
                .len(),
            0
        );
        assert_eq!(
            select(&m, 8, AtPortRequirement::AlreadyEstablished, |_| false)
                .kept
                .len(),
            1
        );
    }
}
