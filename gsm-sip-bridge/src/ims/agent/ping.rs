//! specs/028-gm-tcp-reconnect: keeping the Gm signaling connection honest.
//!
//! A silently-reset Gm connection is otherwise not noticed until the next
//! scheduled renewal (~55 min) or until an attempted call fails halfway. This
//! module is the idle-time `OPTIONS` keepalive that catches it, the listener
//! half's liveness check, and the repair/escalation ladder both feed.
//!
//! Split out of `agent::mod` because [`PingState`] is a pure state machine
//! that was already written to be unit-testable without a socket (R12) — the
//! tests below never sleep and never open anything.

use super::observability;
use crate::error::BridgeResult;
use crate::ims::session::{restart_client_reader, restart_gm_server, Inbound};
use std::time::{Instant, SystemTime};

/// How often, while idle, an `OPTIONS` keepalive probes the Gm client
/// connection for liveness. A silently-reset connection is otherwise not
/// noticed until the next scheduled renewal (~55 min) or an attempted call
/// fails mid-way. 120s bounds the worst-case dead-line duration to ~130s
/// (this interval + `PING_RESPONSE_TIMEOUT`) at ~30 exchanges/line/hour —
/// negligible against an hour-long registration. See
/// specs/028-gm-tcp-reconnect (clarification Q2, R10).
const PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(120);
/// How long to wait for the `OPTIONS` response before scoring the ping — and
/// thus the connection — dead. Generous against a P-CSCF's normal response
/// time, and 12× inside `PING_INTERVAL`. The unanswered-ping case is the one
/// that catches a blackholed connection, where the `send` itself still
/// succeeds (R2).
const PING_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Consecutive failed transport rebuilds before escalating to a full
/// re-registration. Three failures is strong evidence the layer underneath
/// (the Gm IPsec SA) is the problem, which a bare TCP rebind cannot fix —
/// only a re-registration renegotiates a fresh SA (R6).
const MAX_RECONNECT_ATTEMPTS: u32 = 3;

/// The `OPTIONS` keepalive currently in flight on the Gm client connection,
/// if any. See specs/028-gm-tcp-reconnect R1: the ping is sent
/// fire-and-forget and its response is correlated by `CSeq`, because the
/// reader thread owns the read half of the socket — a second reader
/// (`send_and_recv`) would race it and corrupt SIP framing.
#[derive(Debug, Clone, Copy)]
struct PendingPing {
    /// The `CSeq` number the `OPTIONS` went out with — the correlation key.
    cseq: u32,
    /// When it was sent, for the `PING_RESPONSE_TIMEOUT` deadline.
    sent_at: Instant,
}

/// Liveness-probe state for one line's Gm client connection, living on the
/// dispatch loop's state. At most one ping is in flight at a time.
#[derive(Debug, Default)]
pub(super) struct PingState {
    /// When the last ping was sent, driving the `PING_INTERVAL`.
    last_sent: Option<Instant>,
    /// The unanswered ping, if one is outstanding.
    pending: Option<PendingPing>,
}

/// What the idle-poll branch should do about liveness this iteration. Kept a
/// pure function of state + `now` + whether a call is up, so it is unit
/// testable without a socket (R12).
#[derive(Debug, PartialEq, Eq)]
enum PingVerdict {
    /// Do nothing this iteration — a call is in progress (it proves liveness
    /// by itself, R10), or the interval hasn't elapsed and nothing is pending.
    Idle,
    /// No ping is pending and the interval has elapsed (or none was ever
    /// sent): send one now.
    Send,
    /// A ping is pending and still within its response deadline: keep waiting.
    Await,
    /// A ping is pending and past its deadline: the connection is dead.
    Dead,
}

impl PingState {
    /// Decide what to do about the liveness probe this iteration. Pure: no I/O,
    /// takes `now` so tests never sleep.
    fn verdict(&self, now: Instant, call_in_progress: bool) -> PingVerdict {
        if call_in_progress {
            return PingVerdict::Idle;
        }
        match self.pending {
            Some(p) => {
                if now.duration_since(p.sent_at) >= PING_RESPONSE_TIMEOUT {
                    PingVerdict::Dead
                } else {
                    PingVerdict::Await
                }
            }
            None => match self.last_sent {
                Some(t) if now.duration_since(t) < PING_INTERVAL => PingVerdict::Idle,
                _ => PingVerdict::Send,
            },
        }
    }

    /// Record that a ping with `cseq` just went out at `now`.
    fn on_sent(&mut self, cseq: u32, now: Instant) {
        self.last_sent = Some(now);
        self.pending = Some(PendingPing { cseq, sent_at: now });
    }

    /// A response arrived. Returns `true` if it answers the pending ping
    /// (clearing it); `false` if it doesn't match and should be ignored — a
    /// late response to a superseded ping must not revive a dead connection.
    pub(super) fn on_response(&mut self, cseq: u32) -> bool {
        match self.pending {
            Some(p) if p.cseq == cseq => {
                self.pending = None;
                true
            }
            _ => false,
        }
    }

    /// Drop any in-flight ping. Called when the session (and thus the
    /// transport the ping referenced) is replaced, so a stale `CSeq` can't be
    /// scored as a failure against the fresh connection (R11).
    pub(super) fn reset(&mut self) {
        self.last_sent = None;
        self.pending = None;
    }
}

/// Extract the numeric part of a `CSeq` header value (`"5 OPTIONS"` → `5`).
/// Responses echo the request's `CSeq`, so this is how a keepalive answer is
/// matched back to the ping that provoked it.
pub(super) fn parse_cseq_number(cseq: &str) -> Option<u32> {
    cseq.split_whitespace().next()?.parse().ok()
}

/// Extract the method from a `CSeq` header value (`"5 INVITE"` → `"INVITE"`).
/// A response echoes its request's `CSeq`, so this is how the `200` answering a
/// CANCEL (`"N CANCEL"`) is told apart from the INVITE's own final (`"N INVITE"`)
/// — they share a `Call-ID` but resolve different transactions (greptile PR #35).
pub(super) fn cseq_method(cseq: &str) -> Option<&str> {
    cseq.split_whitespace().nth(1)
}

/// When the current Gm failure episode began — carried across
/// `Reconnecting`/`Failed`, restarted at "now" for a connection that was `Up`.
pub(super) fn gm_episode_since(gm_conn: crate::ims::GmConnectionState) -> SystemTime {
    match gm_conn {
        crate::ims::GmConnectionState::Reconnecting { since, .. }
        | crate::ims::GmConnectionState::Failed { since } => since,
        crate::ims::GmConnectionState::Up => SystemTime::now(),
    }
}

/// Rebuild the Gm **client** connection: reconnect the transport on the
/// still-live Gm SA and restart the reader thread that had died with the old
/// socket. Mirrors what `hangup_carrier` does reactively — reused here
/// proactively (specs/028 R6).
fn reconnect_gm_client(
    session: &mut crate::ims::RegisteredSession,
    inbound: &Inbound,
) -> BridgeResult<()> {
    session.reconnect_transport()?;
    restart_client_reader(session, inbound)
}

/// One idle-poll pass of Gm connection liveness (specs/028). Probes both
/// halves of the association independently, repairs a detected failure, and —
/// after `MAX_RECONNECT_ATTEMPTS` consecutive failures — sets `*force_renewal`
/// so the caller escalates to a full re-registration.
///
/// Only ever called with no call in progress (the caller gates on it), so the
/// ping verdict is evaluated as not-in-a-call.
pub(super) fn probe_gm_connection(
    session: &mut crate::ims::RegisteredSession,
    inbound: &mut Inbound,
    obs: &observability::AgentObservability,
    ping: &mut PingState,
    gm_conn: &mut crate::ims::GmConnectionState,
    reconnect_attempts: &mut u32,
    force_renewal: &mut bool,
) {
    let now = Instant::now();

    // Listener half (R4): its accept loop dying is invisible to the client
    // ping (which only exercises the client connection), so poll it directly.
    let mut listener_restart_failed = false;
    if inbound._server.as_ref().is_some_and(|s| !s.is_alive()) {
        tracing::warn!("Gm server listener accept loop died; restarting");
        match restart_gm_server(session, inbound) {
            Ok(()) => tracing::info!("Gm server listener restarted"),
            Err(e) => {
                tracing::warn!(error = %e, "Gm server listener restart failed");
                listener_restart_failed = true;
            }
        }
    }

    // Client half (R1/R2): OPTIONS keepalive, correlated at the response arm.
    let mut client_down = false;
    match ping.verdict(now, false) {
        PingVerdict::Idle | PingVerdict::Await => {}
        PingVerdict::Send => match session.send_gm_ping() {
            Ok(cseq) => ping.on_sent(cseq, now),
            Err(e) => {
                tracing::warn!(error = %e, "Gm keepalive send failed; client connection is down");
                ping.pending = None;
                client_down = true;
            }
        },
        PingVerdict::Dead => {
            tracing::warn!("Gm keepalive went unanswered; client connection is down");
            ping.pending = None;
            client_down = true;
        }
    }

    if !client_down && !listener_restart_failed {
        return;
    }

    // A repair is needed. Attribute one failure to the current episode and
    // report the connection as reconnecting.
    *reconnect_attempts += 1;
    *gm_conn = crate::ims::GmConnectionState::Reconnecting {
        since: gm_episode_since(*gm_conn),
        attempts: *reconnect_attempts,
    };
    obs.set_gm_connection_up(false);

    if *reconnect_attempts >= MAX_RECONNECT_ATTEMPTS {
        // Bare rebuilds haven't helped; the Gm SA underneath is likely gone,
        // and only a re-registration can renegotiate it. Escalate — the caller
        // runs the renewal this same iteration (R6). Drop the ping so the
        // fresh session starts clean.
        tracing::warn!(
            attempts = *reconnect_attempts,
            "Gm reconnect exhausted; escalating to re-registration"
        );
        *force_renewal = true;
        ping.reset();
        return;
    }

    // Only the *client* half is rebuilt here — a failed listener restart is
    // retried on the next poll and, failing that, fixed by the escalation
    // above; tearing down a healthy client transport for it would be wrong.
    if client_down {
        match reconnect_gm_client(session, inbound) {
            // Confirm the rebuilt connection actually carries signaling before
            // reporting it up: send a fresh probe now, and let the response arm
            // flip `gm_conn` to `Up` only when it round-trips (R7). This is
            // what stops a rebuild over a dead SA from reporting a false
            // recovery.
            Ok(()) => match session.send_gm_ping() {
                Ok(cseq) => ping.on_sent(cseq, Instant::now()),
                Err(e) => {
                    tracing::warn!(error = %e, "confirming Gm probe failed to send; will retry")
                }
            },
            Err(e) => tracing::warn!(error = %e, "Gm client reconnect failed; will retry"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The CANCEL/INVITE `CSeq` distinction the `AwaitingCancel` handling relies
    /// on (specs/029, greptile PR #35): a `200` answering the CANCEL must not be
    /// mistaken for the INVITE's own final, or a racing answer is missed and a
    /// phantom leg leaks.
    #[test]
    fn cseq_method_distinguishes_cancel_from_invite() {
        assert_eq!(cseq_method("5 INVITE"), Some("INVITE"));
        assert_eq!(cseq_method("5 CANCEL"), Some("CANCEL"));
        assert_eq!(cseq_method("42 OPTIONS"), Some("OPTIONS"));
        assert_eq!(cseq_method("5"), None);
        assert_eq!(cseq_method(""), None);
        // Number and method are still individually recoverable.
        assert_eq!(parse_cseq_number("5 INVITE"), Some(5));
        // PRACK shares the INVITE's dialog and Call-ID but resolves its own
        // transaction: taking its `200 OK` for the INVITE's final made an
        // outbound call ACK a transaction that had no final response, and the
        // network killed it with `487 ... invalid SDP offer or answer`.
        assert_eq!(cseq_method("6 PRACK"), Some("PRACK"));
        assert_eq!(cseq_method("10 prack"), Some("prack"));
    }

    #[test]
    fn ping_verdict_idle_during_a_call() {
        // A call proves liveness by itself, so no probe is sent while one is up
        // — even when the interval has long since elapsed (R10, FR-006).
        let mut s = PingState::default();
        let t0 = Instant::now();
        s.on_sent(1, t0 - PING_INTERVAL * 2);
        assert_eq!(s.verdict(t0, true), PingVerdict::Idle);
    }

    #[test]
    fn ping_verdict_send_when_never_sent_or_interval_elapsed() {
        let s = PingState::default();
        let t0 = Instant::now();
        // Never pinged yet.
        assert_eq!(s.verdict(t0, false), PingVerdict::Send);

        let mut s2 = PingState {
            last_sent: Some(t0),
            pending: None,
        };
        // Interval not yet elapsed → idle.
        assert_eq!(s2.verdict(t0 + PING_INTERVAL / 2, false), PingVerdict::Idle);
        // Interval elapsed → send.
        s2.pending = None;
        assert_eq!(s2.verdict(t0 + PING_INTERVAL, false), PingVerdict::Send);
    }

    #[test]
    fn ping_verdict_await_then_dead_across_the_response_deadline() {
        let mut s = PingState::default();
        let t0 = Instant::now();
        s.on_sent(7, t0);
        // Within the deadline: keep waiting.
        assert_eq!(
            s.verdict(t0 + PING_RESPONSE_TIMEOUT / 2, false),
            PingVerdict::Await
        );
        // Past the deadline: the connection is dead.
        assert_eq!(
            s.verdict(t0 + PING_RESPONSE_TIMEOUT, false),
            PingVerdict::Dead
        );
    }

    #[test]
    fn ping_never_sends_a_second_while_one_is_pending() {
        let mut s = PingState::default();
        let t0 = Instant::now();
        s.on_sent(1, t0);
        // Even long after the interval, a pending ping means Await/Dead — never
        // a second concurrent Send.
        let v = s.verdict(t0 + PING_INTERVAL * 2, false);
        assert!(matches!(v, PingVerdict::Dead), "got {v:?}");
    }

    #[test]
    fn ping_on_response_matches_only_the_pending_cseq() {
        let mut s = PingState::default();
        s.on_sent(42, Instant::now());
        // A stale/mismatched CSeq must not clear the pending ping.
        assert!(!s.on_response(41));
        assert!(s.pending.is_some());
        // The matching CSeq clears it.
        assert!(s.on_response(42));
        assert!(s.pending.is_none());
    }

    #[test]
    fn ping_full_cycle_alive_then_dropped_then_dead() {
        // The end-to-end verdict flow the OPTIONS keepalive drives: a probe is
        // sent, answered (alive), then a later probe goes unanswered and, once
        // the response deadline passes, the connection is scored dead — which
        // is what triggers a reconnect. The socket round-trip itself is
        // covered by `sip_client::gm_server_reports_alive_and_delivers_a_real_message`.
        let mut s = PingState::default();
        let t0 = Instant::now();

        // First probe, answered within the deadline → alive.
        assert_eq!(s.verdict(t0, false), PingVerdict::Send);
        s.on_sent(1, t0);
        assert!(
            s.on_response(1),
            "matching response marks the connection alive"
        );
        assert!(s.pending.is_none());

        // Interval elapses, second probe sent, and no answer arrives.
        let t1 = t0 + PING_INTERVAL;
        assert_eq!(s.verdict(t1, false), PingVerdict::Send);
        s.on_sent(2, t1);
        assert_eq!(
            s.verdict(t1 + PING_RESPONSE_TIMEOUT / 2, false),
            PingVerdict::Await
        );
        assert_eq!(
            s.verdict(t1 + PING_RESPONSE_TIMEOUT, false),
            PingVerdict::Dead
        );
    }

    #[test]
    fn ping_response_alive_regardless_of_would_be_status() {
        // Any final response to the keepalive proves the connection carries
        // signaling — the response arm never inspects the status code, so a
        // 4xx/5xx is as good a liveness proof as a 200. `on_response` matching
        // purely on CSeq is what encodes that (specs/028 R1).
        let mut s = PingState::default();
        s.on_sent(3, Instant::now());
        assert!(s.on_response(3));
    }

    #[test]
    fn gm_episode_since_is_preserved_across_reconnecting_and_failed() {
        let t = SystemTime::now() - Duration::from_secs(42);
        let reconnecting = crate::ims::GmConnectionState::Reconnecting {
            since: t,
            attempts: 2,
        };
        assert_eq!(gm_episode_since(reconnecting), t);
        let failed = crate::ims::GmConnectionState::Failed { since: t };
        assert_eq!(gm_episode_since(failed), t);
        // A healthy connection has no episode; "since" starts now.
        let up_since = gm_episode_since(crate::ims::GmConnectionState::Up);
        assert!(up_since.elapsed().unwrap() < Duration::from_secs(1));
    }

    #[test]
    fn ping_reset_drops_in_flight_state() {
        let mut s = PingState::default();
        s.on_sent(9, Instant::now());
        s.reset();
        assert!(s.pending.is_none());
        assert!(s.last_sent.is_none());
        // After a reset the next verdict is Send, not a spurious Dead against a
        // CSeq that belonged to the replaced session (R11).
        assert_eq!(s.verdict(Instant::now(), false), PingVerdict::Send);
    }
}
