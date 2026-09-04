//! RFC 4028 session-timer refresh for the outbound (UAC) leg (specs/049).
//!
//! A pure state machine, mirroring `agent::ping`'s `PingState` shape
//! exactly: [`SessionRefreshState::verdict`] decides what to do given
//! `now`, the `on_*` mutators record what happened, and nothing in this
//! file touches a socket or a clock other than through its `now`/`Instant`
//! parameters — the same discipline that makes `ping.rs`'s tests run with
//! no real sleep, applied here too.
//!
//! This bridge never advertises `Supported: timer` on its own outbound
//! INVITEs (`[vowifi] originating_headers` stays off by default,
//! unchanged by this feature — spec.md User Story 3) — everything here
//! reacts only to what a carrier's own `200 OK` says, per RFC 4028 §7.2.

use std::time::{Duration, Instant};

/// How long to wait for a response to a session refresh this bridge itself
/// sent, before treating it as failed. RFC 4028 assumes a full SIP
/// transaction-timer stack this bridge doesn't implement; like
/// `ping.rs`'s `PING_RESPONSE_TIMEOUT` (same value), this is a
/// bridge-chosen bound, generous against a P-CSCF's normal response time,
/// not an RFC-specified one.
pub(super) const SESSION_REFRESH_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Who performs refreshes for a dialog (RFC 4028 §7.1's `refresher`
/// parameter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refresher {
    /// This bridge (the call's UAC) performs refreshes.
    Uac,
    /// The carrier (the call's UAS) performs refreshes.
    Uas,
}

/// Parses a `Session-Expires` header value:
/// `"<delta-seconds>[;refresher=uac|uas][;other=params]"`. Unknown/extra
/// parameters are ignored rather than failing the parse — the same
/// permissive posture this codebase already applies elsewhere (e.g.
/// `sdp.rs`'s `proto`). `None` only when the leading delta-seconds token
/// itself cannot be parsed as a number.
pub(super) fn parse_session_expires(value: &str) -> Option<(u32, Option<Refresher>)> {
    let mut parts = value.split(';').map(str::trim);
    let delta: u32 = parts.next()?.parse().ok()?;
    let refresher = parts.find_map(|p| match p.split_once('=') {
        Some(("refresher", "uac")) => Some(Refresher::Uac),
        Some(("refresher", "uas")) => Some(Refresher::Uas),
        _ => None,
    });
    Some((delta, refresher))
}

/// RFC 4028 §10: "slightly before the session expiration... The minimum of
/// 32 seconds and one third of the session interval is RECOMMENDED." The
/// duration from `now` at which this bridge gives up waiting for the
/// peer's own refresh is `interval` minus this margin.
fn peer_deadline_margin(interval: Duration) -> Duration {
    let third = interval / 3;
    let cap = Duration::from_secs(32);
    interval - third.min(cap)
}

/// Where in the refresh cycle an active call currently is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RefreshPhase {
    /// `refresher == Uac`: no refresh in flight; send one once `due_at`
    /// passes (RFC 4028 §7.2/§9: half the session interval).
    WaitingToSend { due_at: Instant },
    /// `refresher == Uac`: a refresh was sent; waiting for its response.
    AwaitingResponse { cseq: u32, sent_at: Instant },
    /// `refresher == Uac`: the sent refresh's response was a failure (a
    /// non-2xx final, or the `send()` call itself errored) — resolved to
    /// `Overdue` on the very next `verdict()` check. A distinct variant
    /// rather than back-dating `sent_at` past the timeout, so the cause
    /// survives into any future debug logging.
    Failed,
    /// `refresher == Uas`: waiting for the carrier's own in-dialog refresh
    /// before `deadline` (RFC 4028 §10's `min(32s, interval/3)` margin).
    WaitingForPeer { deadline: Instant },
}

/// What `handle_session_refresh` should do this dispatch-loop tick.
/// Mirrors `agent::ping::PingVerdict`'s shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RefreshVerdict {
    /// Nothing to do this tick.
    Idle,
    /// `refresher == Uac`, `due_at` has passed: send a refresh now.
    SendNow,
    /// The refresh cycle has failed (our own refresh's timeout/failure) or
    /// the carrier never refreshed in time — end the call. One verdict for
    /// the one action both cases require (spec.md FR-004/FR-008 both end
    /// the call the same way), even though the two causes are distinct
    /// phases.
    Overdue,
}

/// Per-call RFC 4028 session-refresh state — see `data-model.md`.
#[derive(Debug, Clone)]
pub(super) struct SessionRefreshState {
    /// The negotiated session interval — fixed for the call's lifetime
    /// (research.md Decision 4: never renegotiated on a later refresh).
    interval: Duration,
    refresher: Refresher,
    phase: RefreshPhase,
}

impl SessionRefreshState {
    /// Builds the refresh state for an outbound call's `200 OK`, per RFC
    /// 4028 §7.2. `None` when there is no `Session-Expires` header at all
    /// (§7.2: "there is no session expiration... no refreshes need to be
    /// sent" — today's behavior, unchanged), or its delta-seconds parses
    /// to zero (defensively treated the same as absent).
    pub(super) fn from_2xx(session_expires: Option<&str>, now: Instant) -> Option<Self> {
        let (delta, refresher_param) = parse_session_expires(session_expires?)?;
        if delta == 0 {
            return None;
        }
        let interval = Duration::from_secs(u64::from(delta));
        // research.md Decision 2: an explicit `refresher` param is used
        // exactly as stated (RFC 4028 §7.2). A response that carries
        // `Session-Expires` but omits `refresher` is non-compliant (§9
        // requires the UAS to set it) — this bridge defensively defaults
        // to taking on the duty itself, guaranteeing the call survives
        // either way rather than risking a silent drop on the one input
        // RFC 4028 leaves unspecified for this side.
        let refresher = refresher_param.unwrap_or(Refresher::Uac);
        let phase = match refresher {
            Refresher::Uac => RefreshPhase::WaitingToSend {
                due_at: now + interval / 2,
            },
            Refresher::Uas => RefreshPhase::WaitingForPeer {
                deadline: now + peer_deadline_margin(interval),
            },
        };
        Some(Self {
            interval,
            refresher,
            phase,
        })
    }

    pub(super) fn refresher(&self) -> Refresher {
        self.refresher
    }

    /// The `CSeq` of a refresh this bridge sent and is still waiting on a
    /// response for, if any — how `handle_carrier_response` matches an
    /// incoming response back to it.
    pub(super) fn awaiting_response_cseq(&self) -> Option<u32> {
        match self.phase {
            RefreshPhase::AwaitingResponse { cseq, .. } => Some(cseq),
            _ => None,
        }
    }

    /// The `Session-Expires` value this bridge sends on its own refresh
    /// `UPDATE` (`refresher == Uac` only — RFC 4028 §7.4: "RECOMMENDED
    /// that the refresher parameter be set to 'uac' if the element
    /// sending the request is currently performing refreshes").
    pub(super) fn refresh_header_value(&self) -> String {
        format!("{};refresher=uac", self.interval.as_secs())
    }

    /// Pure — takes `now` so tests never sleep, mirrors
    /// `PingState::verdict`.
    pub(super) fn verdict(&self, now: Instant) -> RefreshVerdict {
        match self.phase {
            RefreshPhase::WaitingToSend { due_at } => {
                if now >= due_at {
                    RefreshVerdict::SendNow
                } else {
                    RefreshVerdict::Idle
                }
            }
            RefreshPhase::AwaitingResponse { sent_at, .. } => {
                if now.duration_since(sent_at) >= SESSION_REFRESH_RESPONSE_TIMEOUT {
                    RefreshVerdict::Overdue
                } else {
                    RefreshVerdict::Idle
                }
            }
            RefreshPhase::Failed => RefreshVerdict::Overdue,
            RefreshPhase::WaitingForPeer { deadline } => {
                if now >= deadline {
                    RefreshVerdict::Overdue
                } else {
                    RefreshVerdict::Idle
                }
            }
        }
    }

    /// Record that a refresh with `cseq` just went out at `now`.
    pub(super) fn on_sent(&mut self, cseq: u32, now: Instant) {
        self.phase = RefreshPhase::AwaitingResponse { cseq, sent_at: now };
    }

    /// The `send()` call for a refresh itself errored — no transaction was
    /// ever created to time out, so this resolves `Overdue` directly.
    pub(super) fn on_send_failed(&mut self) {
        self.phase = RefreshPhase::Failed;
    }

    /// A response to our own sent refresh arrived. Ignored (state
    /// unchanged) if it doesn't match the in-flight refresh's `cseq` — a
    /// late response to a superseded refresh must not revive a call
    /// already resolved on this dialog, mirroring
    /// `PingState::on_response`'s identical discipline.
    pub(super) fn on_response(&mut self, cseq: u32, status: u16, now: Instant) {
        if let RefreshPhase::AwaitingResponse { cseq: pending, .. } = self.phase {
            if pending != cseq {
                return;
            }
            self.phase = if (200..300).contains(&status) {
                RefreshPhase::WaitingToSend {
                    due_at: now + self.interval / 2,
                }
            } else {
                RefreshPhase::Failed
            };
        }
    }

    /// The carrier's own in-dialog refresh arrived (`refresher == Uas`).
    pub(super) fn on_peer_refresh(&mut self, now: Instant) {
        self.phase = RefreshPhase::WaitingForPeer {
            deadline: now + peer_deadline_margin(self.interval),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_session_expires_reads_the_refresher_parameter() {
        assert_eq!(
            parse_session_expires("300;refresher=uac"),
            Some((300, Some(Refresher::Uac)))
        );
        assert_eq!(
            parse_session_expires("300;refresher=uas"),
            Some((300, Some(Refresher::Uas)))
        );
    }

    #[test]
    fn parse_session_expires_defaults_refresher_to_none_when_absent() {
        assert_eq!(parse_session_expires("300"), Some((300, None)));
    }

    #[test]
    fn parse_session_expires_ignores_unknown_extra_params() {
        assert_eq!(
            parse_session_expires("300;refresher=uac;extra=whatever"),
            Some((300, Some(Refresher::Uac)))
        );
        assert_eq!(
            parse_session_expires("300;unknown=1;refresher=uas"),
            Some((300, Some(Refresher::Uas)))
        );
    }

    #[test]
    fn parse_session_expires_rejects_a_non_numeric_delta() {
        assert_eq!(parse_session_expires("not-a-number"), None);
        assert_eq!(parse_session_expires(""), None);
    }

    #[test]
    fn from_2xx_is_none_with_no_session_expires_header() {
        assert!(SessionRefreshState::from_2xx(None, Instant::now()).is_none());
    }

    #[test]
    fn from_2xx_is_none_with_a_zero_interval() {
        assert!(SessionRefreshState::from_2xx(Some("0"), Instant::now()).is_none());
    }

    #[test]
    fn from_2xx_defaults_to_uac_when_no_refresher_param_is_present() {
        // research.md Decision 2: a non-compliant response gets the
        // defensive default, not treated as "no obligation".
        let state = SessionRefreshState::from_2xx(Some("300"), Instant::now()).unwrap();
        assert_eq!(state.refresher(), Refresher::Uac);
    }

    #[test]
    fn from_2xx_honours_an_explicit_uas_refresher() {
        let state = SessionRefreshState::from_2xx(Some("300;refresher=uas"), Instant::now())
            .expect("Session-Expires present");
        assert_eq!(state.refresher(), Refresher::Uas);
        assert!(matches!(state.phase, RefreshPhase::WaitingForPeer { .. }));
    }

    #[test]
    fn from_2xx_uac_starts_waiting_to_send_at_half_the_interval() {
        let now = Instant::now();
        let state = SessionRefreshState::from_2xx(Some("300;refresher=uac"), now).unwrap();
        let RefreshPhase::WaitingToSend { due_at } = state.phase else {
            panic!("expected WaitingToSend");
        };
        assert_eq!(due_at, now + Duration::from_secs(150));
    }

    #[test]
    fn verdict_waits_until_due_at_then_says_send_now() {
        let now = Instant::now();
        let state = SessionRefreshState::from_2xx(Some("300;refresher=uac"), now).unwrap();
        assert_eq!(state.verdict(now), RefreshVerdict::Idle);
        assert_eq!(
            state.verdict(now + Duration::from_secs(149)),
            RefreshVerdict::Idle
        );
        assert_eq!(
            state.verdict(now + Duration::from_secs(150)),
            RefreshVerdict::SendNow
        );
    }

    #[test]
    fn awaiting_response_becomes_overdue_only_past_its_timeout() {
        let now = Instant::now();
        let mut state = SessionRefreshState::from_2xx(Some("300;refresher=uac"), now).unwrap();
        state.on_sent(1, now);
        assert_eq!(state.verdict(now), RefreshVerdict::Idle);
        assert_eq!(
            state.verdict(now + SESSION_REFRESH_RESPONSE_TIMEOUT - Duration::from_secs(1)),
            RefreshVerdict::Idle
        );
        assert_eq!(
            state.verdict(now + SESSION_REFRESH_RESPONSE_TIMEOUT),
            RefreshVerdict::Overdue
        );
    }

    #[test]
    fn on_response_ignores_a_mismatched_cseq() {
        // Mirrors PingState::on_response's identical discipline: a late
        // response to a superseded refresh must not revive this one.
        let now = Instant::now();
        let mut state = SessionRefreshState::from_2xx(Some("300;refresher=uac"), now).unwrap();
        state.on_sent(1, now);
        state.on_response(2, 200, now);
        assert!(matches!(
            state.phase,
            RefreshPhase::AwaitingResponse { cseq: 1, .. }
        ));
    }

    #[test]
    fn on_response_2xx_re_arms_for_the_next_half_interval() {
        let now = Instant::now();
        let mut state = SessionRefreshState::from_2xx(Some("300;refresher=uac"), now).unwrap();
        state.on_sent(1, now);
        let later = now + Duration::from_secs(151);
        state.on_response(1, 200, later);
        assert_eq!(
            state.phase,
            RefreshPhase::WaitingToSend {
                due_at: later + Duration::from_secs(150)
            }
        );
    }

    #[test]
    fn on_response_non_2xx_fails_and_is_immediately_overdue() {
        let now = Instant::now();
        let mut state = SessionRefreshState::from_2xx(Some("300;refresher=uac"), now).unwrap();
        state.on_sent(1, now);
        state.on_response(1, 405, now);
        assert_eq!(state.phase, RefreshPhase::Failed);
        assert_eq!(state.verdict(now), RefreshVerdict::Overdue);
    }

    #[test]
    fn on_send_failed_is_immediately_overdue() {
        let now = Instant::now();
        let mut state = SessionRefreshState::from_2xx(Some("300;refresher=uac"), now).unwrap();
        state.on_send_failed();
        assert_eq!(state.verdict(now), RefreshVerdict::Overdue);
    }

    #[test]
    fn waiting_for_peer_becomes_overdue_thirty_two_seconds_before_a_long_interval_expires() {
        // interval=300s: one third is 100s, capped at RFC 4028 §10's 32s
        // minimum, so the deadline is interval - 32s = 268s from `now`.
        let now = Instant::now();
        let state = SessionRefreshState::from_2xx(Some("300;refresher=uas"), now).unwrap();
        assert_eq!(
            state.verdict(now + Duration::from_secs(267)),
            RefreshVerdict::Idle
        );
        assert_eq!(
            state.verdict(now + Duration::from_secs(268)),
            RefreshVerdict::Overdue
        );
    }

    #[test]
    fn waiting_for_peer_uses_one_third_when_that_is_smaller_than_32s() {
        // interval=90s (RFC 4028's own floor): one third is 30s, smaller
        // than the 32s cap, so the deadline is interval - 30s = 60s.
        let now = Instant::now();
        let state = SessionRefreshState::from_2xx(Some("90;refresher=uas"), now).unwrap();
        assert_eq!(
            state.verdict(now + Duration::from_secs(59)),
            RefreshVerdict::Idle
        );
        assert_eq!(
            state.verdict(now + Duration::from_secs(60)),
            RefreshVerdict::Overdue
        );
    }

    #[test]
    fn on_peer_refresh_re_arms_the_waiting_for_peer_deadline() {
        let now = Instant::now();
        let mut state = SessionRefreshState::from_2xx(Some("300;refresher=uas"), now).unwrap();
        let later = now + Duration::from_secs(200);
        state.on_peer_refresh(later);
        assert_eq!(
            state.phase,
            RefreshPhase::WaitingForPeer {
                deadline: later + Duration::from_secs(268)
            }
        );
    }

    #[test]
    fn refresh_header_value_states_the_interval_and_uac_refresher() {
        let now = Instant::now();
        let state = SessionRefreshState::from_2xx(Some("300;refresher=uac"), now).unwrap();
        assert_eq!(state.refresh_header_value(), "300;refresher=uac");
    }
}
