//! Outbound-calling destination validation, shared by the two real
//! selection implementations (`modules::mod::handle_outbound_request` for
//! the circuit-switched path, `vowifi::mod::run_outbound_listener` for
//! VoWiFi/VoLTE) — each owns its own line-selection logic against its own
//! state representation (`SlotState` vs. `RuntimeLine`) rather than a
//! shared abstraction here.
//!
//! **2026-08-03 (specs/025-outbound-calling review)**: this module used to
//! also define `Origin`/`OutboundCallRequest`/`CarrierPath`/`CandidateLine`/
//! `select_idle_line` — a generic line-selection abstraction with zero real
//! callers, `CandidateLine::idle`'s own doc comment nonetheless claiming to
//! be "the sole definition of 'idle' (FR-005)". Both real paths had already
//! grown their own selection logic independently (`modules/mod.rs`'s slot
//! iteration, `vowifi/mod.rs`'s line iteration) well before this module was
//! ever wired up, and retrofitting either onto this shape risked the
//! already-live-verified outbound-calling behavior for what amounted to a
//! documentation cleanup. Deleted rather than left as dead code
//! contradicting itself — see `data-model.md` for where this conceptual
//! model still lives.

/// Why an outbound call attempt did not result in a connected call, or that
/// it did. Matches the outcome categories in `data-model.md` and the
/// `outcome` label on `metrics::OUTBOUND_ATTEMPTS_TOTAL` — mirrored (not
/// imported) by `control::protocol::OutboundAttemptOutcome`, which stays a
/// plain wire-protocol type, the same layering `CallStatus`/`SmsOutcome`
/// already keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundOutcome {
    Placed,
    RefusedNoIdleLine,
    RefusedInvalidDestination,
    RefusedNetworkFailure,
    Unanswered,
    /// The originating caller hung up before the call connected
    /// (specs/029-interruptible-origination-wait, FR-018). Kept in lockstep
    /// with `control::protocol::OutboundAttemptOutcome`'s mirror variant.
    CallerAbandoned,
}

impl OutboundOutcome {
    pub fn as_label(&self) -> &'static str {
        match self {
            OutboundOutcome::Placed => "placed",
            OutboundOutcome::RefusedNoIdleLine => "refused_no_idle_line",
            OutboundOutcome::RefusedInvalidDestination => "refused_invalid_destination",
            OutboundOutcome::RefusedNetworkFailure => "refused_network_failure",
            OutboundOutcome::Unanswered => "unanswered",
            OutboundOutcome::CallerAbandoned => "caller_abandoned",
        }
    }
}

/// Validates a destination before any line is touched (FR-014): non-empty
/// and composed only of characters `ATD`/an IMS Request-URI user part can
/// actually carry.
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
}
