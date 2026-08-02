//! Outbound calling: a request arriving from the SIP side (a PBX-originated
//! INVITE, or — once redirected by `sip::server`'s registrar — a phone
//! registered in SIP server mode) that must be placed out over the mobile
//! network on whichever configured line is idle.
//!
//! See `specs/025-outbound-calling/data-model.md` for the full entity and
//! state-transition model this module implements.

/// Who originated an `OutboundCallRequest`. Never affects routing or
/// eligibility beyond what already gated the INVITE reaching this module
/// (FR-003) — carried only for logs and metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Pbx,
    SipServerPhone { aor: String },
}

/// Created the moment an eligible INVITE is accepted. Lives only for the
/// duration of one attempt — never persisted.
#[derive(Debug, Clone)]
pub struct OutboundCallRequest {
    /// Request-URI user part, byte-for-byte (FR-010) — no digit stripping,
    /// prefix insertion, or reformatting anywhere in this pipeline.
    pub destination: String,
    pub origin: Origin,
    /// The originating SIP dialog's Call-ID, for log correlation.
    pub call_id: String,
}

/// Why an `OutboundCallRequest` did not result in a connected call, or that
/// it did. Matches the outcome categories in `data-model.md` and the
/// `outcome` label on `metrics::OUTBOUND_ATTEMPTS_TOTAL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundOutcome {
    Placed,
    RefusedNoIdleLine,
    RefusedInvalidDestination,
    RefusedNetworkFailure,
    Unanswered,
}

impl OutboundOutcome {
    pub fn as_label(&self) -> &'static str {
        match self {
            OutboundOutcome::Placed => "placed",
            OutboundOutcome::RefusedNoIdleLine => "refused_no_idle_line",
            OutboundOutcome::RefusedInvalidDestination => "refused_invalid_destination",
            OutboundOutcome::RefusedNetworkFailure => "refused_network_failure",
            OutboundOutcome::Unanswered => "unanswered",
        }
    }
}

/// Which carrier path a `CandidateLine` reaches the mobile network over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarrierPath {
    CircuitSwitched,
    VoWifi,
    Volte,
}

/// The SIP-owning process's view of one configured line, built from the
/// existing liveness/state tracking (no new state source — this is a read
/// model over data already reported).
#[derive(Debug, Clone)]
pub struct CandidateLine {
    pub id: String,
    pub path: CarrierPath,
    pub registered: bool,
    pub busy: bool,
    pub recovering: bool,
}

impl CandidateLine {
    /// The sole definition of "idle" (FR-005) — no additional
    /// outbound-specific eligibility rule exists anywhere else in this
    /// module.
    pub fn idle(&self) -> bool {
        self.registered && !self.busy && !self.recovering
    }
}

/// Validates a destination before any `CandidateLine` is touched (FR-014):
/// non-empty and composed only of characters `ATD`/an IMS Request-URI user
/// part can actually carry.
pub fn validate_destination(destination: &str) -> Result<(), OutboundOutcome> {
    if destination.is_empty() {
        return Err(OutboundOutcome::RefusedInvalidDestination);
    }
    if !destination
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '*' | '#' | '+'))
    {
        return Err(OutboundOutcome::RefusedInvalidDestination);
    }
    Ok(())
}

/// Selects the first idle `CandidateLine`, in whatever order the caller
/// hands them — no path preference (FR-007, spec 025 Clarifications
/// 2026-08-02).
pub fn select_idle_line(candidates: &[CandidateLine]) -> Option<&CandidateLine> {
    candidates.iter().find(|c| c.idle())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_destination_is_rejected() {
        assert_eq!(
            validate_destination(""),
            Err(OutboundOutcome::RefusedInvalidDestination)
        );
    }

    #[test]
    fn destination_with_letters_is_rejected() {
        assert_eq!(
            validate_destination("abc123"),
            Err(OutboundOutcome::RefusedInvalidDestination)
        );
    }

    #[test]
    fn plain_digits_are_accepted() {
        assert_eq!(validate_destination("15551234567"), Ok(()));
    }

    #[test]
    fn plus_star_hash_are_accepted() {
        assert_eq!(validate_destination("+1555*1#234567"), Ok(()));
    }

    fn line(
        id: &str,
        path: CarrierPath,
        registered: bool,
        busy: bool,
        recovering: bool,
    ) -> CandidateLine {
        CandidateLine {
            id: id.to_string(),
            path,
            registered,
            busy,
            recovering,
        }
    }

    #[test]
    fn idle_requires_registered_and_not_busy_and_not_recovering() {
        assert!(line("a", CarrierPath::CircuitSwitched, true, false, false).idle());
        assert!(!line("a", CarrierPath::CircuitSwitched, false, false, false).idle());
        assert!(!line("a", CarrierPath::CircuitSwitched, true, true, false).idle());
        assert!(!line("a", CarrierPath::CircuitSwitched, true, false, true).idle());
    }

    #[test]
    fn select_idle_line_skips_busy_lines_with_no_path_preference() {
        let candidates = vec![
            line("cs-0", CarrierPath::CircuitSwitched, true, true, false),
            line("vowifi-0", CarrierPath::VoWifi, true, false, false),
            line("volte-0", CarrierPath::Volte, true, false, false),
        ];
        let selected = select_idle_line(&candidates).expect("one idle line");
        assert_eq!(selected.id, "vowifi-0");
    }

    #[test]
    fn select_idle_line_returns_none_when_all_busy() {
        let candidates = vec![line(
            "cs-0",
            CarrierPath::CircuitSwitched,
            true,
            true,
            false,
        )];
        assert!(select_idle_line(&candidates).is_none());
    }
}
