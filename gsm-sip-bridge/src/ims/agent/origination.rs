//! Placing an outbound call on the carrier leg
//! (specs/025-outbound-calling, specs/029-interruptible-origination-wait).
//!
//! Split out of `agent::mod` as the single largest self-contained block there:
//! an explicit state machine ([`PendingOrigination`]) that the dispatch loop
//! advances one tick at a time. It replaced a blocking `originate_and_bridge`
//! that read the carrier socket directly — racing the always-running
//! client-reader thread (research R2) and wedging the whole loop for up to
//! ~80s. Carrier responses now arrive through `inbound.rx` like every other
//! message; everything here is the state they are applied against.

use super::call::{ActiveCall, DialogInfo};
use super::veth::{spawn_relay, spawn_veth_uas_listener, VethUasResult, VETH_INVITE_TIMEOUT};
use crate::error::BridgeResult;
use crate::ims::lifecycle::{BridgedCall, CallStage};
use crate::ims::sdp::{self, NegotiatedCodec};
use crate::ims::sip_client::{random_hex, SipResponse};
use crate::vowifi::control::{reason, write_msg, ControlMessage};
use chrono::Utc;
use std::net::{IpAddr, TcpStream, UdpSocket};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// The two carrier-response windows this state machine enforces. They live in
/// `agent::mod` because `vowifi::mod` cross-checks its own timeouts against
/// them; see their doc comments there.
use super::{OUTBOUND_INVITE_TIMEOUT, OUTBOUND_RING_TIMEOUT};

/// How long to wait for a final response to our own CANCEL of an
/// abandoned INVITE — normally a prompt `487 Request Terminated`, plus the
/// (legitimate, RFC 3261 §9.1) chance of a `200 OK` racing in from before
/// the CANCEL arrived at the carrier. Short: a carrier that hasn't reacted
/// to a CANCEL within a few seconds isn't going to.
const CANCEL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to keep retrying a CANCEL that could not be sent (the transport was
/// momentarily unavailable) before giving up. Generous: an INVITE we could not
/// cancel is a phantom-leg liability, so we keep trying well past a transient
/// blip (greptile PR #35). If the transport is dead this long, the dispatch
/// loop's own Gm-liveness/renewal machinery is already replacing the session
/// out from under this attempt anyway.
const CANCEL_SEND_MAX_WAIT: Duration = Duration::from_secs(30);

/// Hangs up a carrier leg that was answered (200 OK + ACK already
/// exchanged) but can't be usably bridged for some reason discovered
/// afterward — sends a BYE to the carrier (reusing `dialog`, built right
/// after ACK) and `CallEnded` to Agent B over `control`. Found live
/// (specs/025-outbound-calling review): by the time any of these failures
/// are discovered, Agent B has very likely *already* answered its own
/// phone/PBX leg (`Call::make` dispatches the veth INVITE and returns
/// immediately; `bridge_outbound_leg` answers the phone leg right after,
/// without waiting for it to connect) — leaving Agent B silent here would
/// strand a caller who thinks they're connected, on top of leaking the
/// carrier leg itself. Best-effort on both: there is nothing further to
/// retry from this point.
fn hangup_answered_carrier_leg(
    session: &mut crate::ims::RegisteredSession,
    control: &mut TcpStream,
    dialog: &DialogInfo,
    call_id: &str,
    reason: &str,
) {
    let bye = dialog.build_bye_for(call_id);
    let _ = session.transport_mut().and_then(|t| t.send(&bye));
    let _ = write_msg(
        control,
        &ControlMessage::CallEnded {
            call_id: call_id.to_string(),
            reason: reason.to_string(),
        },
    );
}

/// Spawns a fresh veth UAS listener for an already-answered carrier leg, or
/// hangs that leg up (and tells Agent B) if it can't be spawned — the
/// shared tail both the no-early-media path and specs/037-p-early-media's
/// stale-listener fallback need at the real `200 OK`. `None` means the
/// caller must treat the attempt as `Ended`; the hangup has already been
/// sent.
fn spawn_fresh_veth_listener_or_hangup(
    session: &mut crate::ims::RegisteredSession,
    control: &mut TcpStream,
    dialog: &DialogInfo,
    call_id: &str,
    veth_local_ip: IpAddr,
    veth_sip_port: u16,
    wideband: bool,
) -> Option<mpsc::Receiver<BridgeResult<VethUasResult>>> {
    match spawn_veth_uas_listener(veth_local_ip, veth_sip_port, wideband) {
        Ok(rx) => Some(rx),
        Err(e) => {
            tracing::warn!(call_id, error = %e, "outbound: veth listener failed");
            hangup_answered_carrier_leg(session, control, dialog, call_id, reason::VETH_LEG_FAILED);
            None
        }
    }
}

/// An outbound origination in flight, held by the dispatch loop across poll
/// ticks so the wait for the carrier (and then Agent B's veth leg) is
/// interruptible (specs/029-interruptible-origination-wait).
pub(super) struct PendingOrigination {
    step: OriginationStep,
    /// Correlates carrier responses (by `Call-ID`) and Agent B's `CallEnded`
    /// (by `call_id`) to *this* attempt — a message naming another call is
    /// ignored, never acted on (FR-010).
    call_id: String,
    from_tag: String,
    branch: String,
    invite_cseq: u32,
    /// In-dialog requests sent after the INVITE (each `PRACK`), so a later
    /// `BYE` picks a `CSeq` above them rather than reusing one — RFC 3261
    /// §12.2.1.1 requires the sequence to increase within a dialog.
    extra_cseq: u32,
    /// The `RSeq` already acknowledged, so retransmissions of the *same*
    /// reliable provisional are not PRACKed twice.
    pracked_rseq: Option<u32>,
    /// The SDP answer a provisional response carried, if any. With a reliable
    /// provisional the offer/answer exchange completes there and the `200 OK`
    /// has no body at all, so this is the only copy of the answer we get.
    provisional_answer: Option<String>,
    /// The veth UAS listener's receiver, if a provisional's SDP already
    /// triggered early media (specs/037-p-early-media) — `Some` exactly
    /// when RTP was connected and the listener spawned together, in the
    /// same step. Consulted (not necessarily consumed unchanged — see the
    /// real `200 OK` handling's stale-result fallback) once the real `200
    /// OK` arrives, in place of spawning a fresh listener from scratch.
    early_veth_rx: Option<mpsc::Receiver<BridgeResult<VethUasResult>>>,
    /// Guards `CallEarlyMedia` to one attempt per call — set the first time
    /// a provisional's body is examined for early media, whether or not
    /// that attempt actually succeeded (FR-006: a failed attempt is not
    /// retried on a later provisional; the call just proceeds without early
    /// media, exactly as it did before this feature existed).
    early_media_sent: bool,
    /// The codec the early SDP negotiated, captured alongside
    /// `early_media_sent` — needed once `early_veth_rx` resolves (which can
    /// happen well before the real final response) to decide whether the
    /// relay started then needs transcoding.
    early_media_codec: Option<NegotiatedCodec>,
    /// Set once `early_veth_rx` resolves successfully and the audio relay
    /// actually starts — *not* merely once early media was attempted.
    /// Without this, early media was signaling-only: Agent B would pair and
    /// answer `183`, but no RTP ever moved, because the relay this codebase
    /// already has (`spawn_relay`/`spawn_transcoding_relay`) was only ever
    /// started from `finish_origination`, reachable solely via the real
    /// `200 OK` → `AwaitingVeth` → veth-arrival path — a call that never
    /// reaches `200 OK` (e.g. carrier plays an announcement then rejects
    /// with `480`, exactly Jio's diagnosed behavior) never ran that path at
    /// all. Found live (specs/037-p-early-media, 2026-08-16): signaling
    /// paired and answered `183` correctly, but a real test call reported
    /// zero RTP packets in either direction. Once this is `Some`, the real
    /// `200 OK` handling reuses it instead of spawning a second relay; any
    /// termination path that drops this `PendingOrigination` without
    /// reaching `ActiveCall` must stop it (`stop.store(true, ..)`) or the
    /// relay thread leaks.
    /// `chosen` is what the relay was actually built to speak on the
    /// carrier side — compared against the real `200 OK`'s answer at reuse
    /// time (code review finding, specs/037-p-early-media): a carrier's
    /// final SDP can legitimately select a different codec than an early,
    /// non-`100rel` provisional's (RFC 3264 doesn't bind them together),
    /// and reusing a relay built for the wrong one would silently corrupt
    /// or drop audio rather than error. On a mismatch, the real `200 OK`
    /// handling stops this relay and rebuilds one from `early_veth_socket`
    /// instead of trusting it.
    early_relay: Option<(
        Arc<AtomicBool>,
        crate::ims::media_stats::MediaMeter,
        sdp::ChosenCodec,
    )>,
    /// A second handle to the veth-side RTP socket and its codec, kept only
    /// while `early_relay` is `Some` — the relay thread owns the other
    /// (cloned) handle. Needed solely to rebuild the relay if the real
    /// `200 OK`'s codec doesn't match `early_relay`'s; otherwise dropped
    /// unused, same as `rtp_socket` is in the ordinary reuse case.
    early_veth_socket: Option<(UdpSocket, sdp::ChosenCodec)>,
    callee_uri: String,
    route_headers: Vec<String>,
    via_transport: &'static str,
    destination: String,
    /// Connection to Agent B: written to here (`CallRinging`/`CallPlaced`/
    /// `CallFailed`), read from via `ctrl_rx`'s reader thread.
    control: TcpStream,
    /// Spawned in `begin_origination` — *before* the carrier answers, unlike
    /// the old code that only started reading Agent B once a call was fully
    /// bridged — so a caller hangup mid-attempt is observable at all.
    ctrl_rx: mpsc::Receiver<ControlMessage>,
    rtp_socket: UdpSocket,
    veth_local_ip: IpAddr,
    veth_sip_port: u16,
    wideband: bool,
    /// While `AwaitingCarrier`: the carrier-response deadline (initially
    /// `OUTBOUND_INVITE_TIMEOUT`, extended to `OUTBOUND_RING_TIMEOUT` once any
    /// response arrives). While `AwaitingVeth`: the veth-leg deadline
    /// (`VETH_INVITE_TIMEOUT`). One field, reset on the transition.
    deadline: Instant,
    any_response_seen: bool,
    ringing_relayed: bool,
    /// Set once the attempt has been abandoned, so the abandon path runs
    /// exactly once.
    ///
    /// `CallEnded` is consumed by the `try_recv` that observes it, but a
    /// *disconnected* control channel is a permanent condition: every later
    /// poll reports it again. Without this flag an attempt left in
    /// `AwaitingCancel` re-abandoned and re-logged on every tick — measured
    /// 2026-08-14, 463 iterations at ~10/s and still climbing. Worse, it kept
    /// the single pending slot occupied, so the next outbound call was
    /// refused with "no VoWiFi/VoLTE line available".
    abandoned: bool,
    pub(super) lifecycle: BridgedCall,
}

/// The waits the old `originate_and_bridge` blocked on, now explicit states.
/// The shared `Awaiting` prefix is deliberate — each is a distinct thing the
/// loop is waiting for.
#[allow(clippy::enum_variant_names)]
enum OriginationStep {
    /// INVITE sent; waiting for the carrier's final response.
    AwaitingCarrier,
    /// Carrier answered `200 OK` and was ACKed; waiting for Agent B's veth leg.
    /// The `VETH_INVITE_TIMEOUT` window, previously a blocking
    /// `veth_rx.recv_timeout`.
    AwaitingVeth {
        dialog: DialogInfo,
        /// `None` (specs/037-p-early-media) means the audio relay is
        /// already running — started once the early veth handshake
        /// resolved (`tick_pending_origination`) — so there is nothing left
        /// to wait for; `finish_origination` reuses `PendingOrigination::
        /// early_relay` instead of spawning or waiting for anything.
        veth_rx: Option<mpsc::Receiver<BridgeResult<VethUasResult>>>,
        /// The codec the carrier answered with, carried to `finish_origination`
        /// where the transcoding decision (and the "codec we never offered"
        /// check) is made — kept in the same order as the old blocking path.
        /// Unused when `veth_rx` is `None` (the relay's codecs were already
        /// resolved during early media).
        answer_codec: sdp::NegotiatedCodec,
    },
    /// A CANCEL is being sent (abandonment or our own timeout) and we are
    /// waiting for its outcome — a `487` for the INVITE (expected), or a `200`
    /// that raced the CANCEL (the carrier answered anyway; we ACK then BYE).
    /// That outcome arrives on `inbound.rx` like every other response, so it is
    /// handled at the response arm rather than by a direct socket read that
    /// would race the client reader (greptile PR #35 / research R2).
    ///
    /// `sent` tracks whether the CANCEL actually went out: if the transport was
    /// momentarily unavailable it is retried each tick, and the response window
    /// is only armed once it succeeds — otherwise a failed send would drop a
    /// still-live INVITE, leaking a phantom leg if the carrier later answered
    /// (greptile PR #35, round 2).
    AwaitingCancel { sent: bool },
}

/// Whether a pending origination is still in flight or has resolved this tick.
pub(super) enum OriginationStatus {
    /// Keep waiting — the caller retains the `PendingOrigination`.
    Pending,
    /// Resolved (failure, timeout, or abandonment). Any teardown and any
    /// `CallFailed` to Agent B has already been done; the caller drops it.
    Ended,
}

/// Best-effort `CallFailed` to Agent B, factored out of the old `fail` closure.
fn origination_failed(control: &mut TcpStream, call_id: &str, reason: &str) {
    tracing::warn!(call_id, reason, "outbound: could not place carrier call");
    let _ = write_msg(
        control,
        &ControlMessage::CallFailed {
            call_id: call_id.to_string(),
            reason: reason.to_string(),
        },
    );
}

/// Everything `begin_origination` needs about where the veth leg will land.
pub(super) struct OriginationSetup {
    pub(super) veth_local_ip: IpAddr,
    pub(super) veth_sip_port: u16,
    pub(super) wideband: bool,
    /// See `config::OriginatingHeaders` — empty everywhere by default.
    pub(super) originating_headers: crate::config::OriginatingHeaders,
}

/// Builds and sends the carrier INVITE and returns the in-flight state, or
/// `None` (having told Agent B `CallFailed`) if it could not even be sent. The
/// front half of the old `originate_and_bridge`, up to and including the
/// INVITE — plus spawning the Agent B control reader *now* so a mid-attempt
/// hangup can be heard. The dispatch loop has already sent `CallAttempting`.
pub(super) fn begin_origination(
    session: &mut crate::ims::RegisteredSession,
    mut control: TcpStream,
    call_id: String,
    destination: &str,
    setup: &OriginationSetup,
) -> Option<PendingOrigination> {
    // RFC 3608, same simplification `ims::call::run_call` already makes:
    // subsequent requests in this dialog route via the Service-Route the
    // registrar returned, computed once here and reused for the INVITE and
    // (via `DialogInfo::from_uac_response`) the eventual BYE.
    let route_headers: Vec<String> = session
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("Service-Route"))
        .map(|(_, v)| format!("Route: {v}"))
        .collect();

    let rtp_socket = match UdpSocket::bind((session.local_addr.ip(), 0)) {
        Ok(s) => s,
        Err(e) => {
            origination_failed(
                &mut control,
                &call_id,
                &format!("RTP socket bind failed: {e}"),
            );
            return None;
        }
    };
    let rtp_port = match rtp_socket.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            origination_failed(
                &mut control,
                &call_id,
                &format!("RTP local_addr failed: {e}"),
            );
            return None;
        }
    };

    let session_id: u64 = rand::random::<u32>() as u64;
    // Offering, not answering — "prefer wideband when available" for both
    // carrier paths, rather than reusing `AnswerPreference`'s legacy/cellular
    // split, which is about the *answer* fallback order (AMR-NB vs. PCMU)
    // when the far end's own offer lacks AMR-WB — not applicable to what we
    // ourselves offer here.
    let offer = sdp::build_offer(
        session.local_addr.ip(),
        rtp_port,
        session_id,
        sdp::CodecOffer::preferring_wideband(setup.wideband && amr_safe::is_available()),
    );

    // `;user=phone` (RFC 3261 §19.1.1 / TS 24.229): tells the network this is
    // a PSTN/mobile number, not a resolvable SIP address — the same header
    // `ims::call::run_call` adds after finding a bare `sip:` URI reached a
    // terminating application server that never rang the callee.
    let callee_uri = format!("{destination}@{};user=phone", session.realm);
    let from_tag = random_hex(4);
    let invite_cseq = session.cseq;
    let via_transport = if session.use_tcp { "TCP" } else { "UDP" };
    let branch = format!("z9hG4bK{}", random_hex(6));

    let invite = crate::ims::call::build_invite(&crate::ims::call::InviteParts {
        request_uri: &callee_uri,
        route_headers: &route_headers,
        via_transport,
        local_addr: session.local_addr,
        contact_addr: session.contact_addr,
        public_uri: &session.origination_identity(),
        callee_uri: &callee_uri,
        call_id: &call_id,
        from_tag: &from_tag,
        cseq: invite_cseq,
        branch: &branch,
        body: &offer,
        originating_headers: setup.originating_headers,
    });

    tracing::info!(call_id, destination, "outbound: sending INVITE to carrier");
    let transport = match session.transport_mut() {
        Ok(t) => t,
        Err(e) => {
            origination_failed(
                &mut control,
                &call_id,
                &format!("no carrier transport: {e}"),
            );
            return None;
        }
    };
    if let Err(e) = transport.send(&invite) {
        origination_failed(&mut control, &call_id, &format!("INVITE send failed: {e}"));
        return None;
    }

    // Start listening to Agent B now (not after the call is bridged, as the old
    // code did): this is the whole point of specs/029 — a caller hanging up
    // while the carrier is still ringing has to reach the dispatch loop, and
    // this reader is the only path for it.
    let ctrl_rx = match control.try_clone() {
        Ok(s) => super::call::spawn_control_reader(s),
        Err(e) => {
            origination_failed(
                &mut control,
                &call_id,
                &format!("control connection clone failed: {e}"),
            );
            return None;
        }
    };

    // Lifecycle from the moment the INVITE is sent: `Offered → Answering`. This
    // is also what makes the line read as busy to an inbound INVITE arriving
    // mid-attempt (`Admission::for_current`, FR-011) with no new rule.
    let mut lifecycle = BridgedCall::new(call_id.clone(), destination.to_string(), None);
    lifecycle.advance_to(CallStage::Answering);

    Some(PendingOrigination {
        step: OriginationStep::AwaitingCarrier,
        call_id,
        from_tag,
        branch,
        invite_cseq,
        extra_cseq: 0,
        pracked_rseq: None,
        provisional_answer: None,
        early_veth_rx: None,
        early_media_sent: false,
        early_media_codec: None,
        early_relay: None,
        early_veth_socket: None,
        callee_uri,
        route_headers,
        via_transport,
        destination: destination.to_string(),
        control,
        ctrl_rx,
        rtp_socket,
        veth_local_ip: setup.veth_local_ip,
        veth_sip_port: setup.veth_sip_port,
        wideband: setup.wideband,
        deadline: Instant::now() + OUTBOUND_INVITE_TIMEOUT,
        any_response_seen: false,
        ringing_relayed: false,
        abandoned: false,
        lifecycle,
    })
}

impl PendingOrigination {
    /// Does this carrier response belong to this attempt? Matched by `Call-ID`,
    /// so it never collides with the Gm keepalive's `OPTIONS` (a different
    /// `Call-ID`, correlated by `CSeq` at the response arm instead).
    pub(super) fn matches_response(&self, resp: &SipResponse) -> bool {
        resp.header("Call-ID")
            .is_some_and(|id| id.trim() == self.call_id)
    }

    /// Tell Agent B this attempt failed. Best-effort; Agent B may already have
    /// moved on (e.g. it initiated an abandonment).
    fn fail(&mut self, reason: &str) {
        let call_id = self.call_id.clone();
        origination_failed(&mut self.control, &call_id, reason);
        self.stop_early_relay();
    }

    /// Stops a relay already running from early media (specs/037-p-early-media),
    /// if one is. A no-op when none was ever started. Every termination path
    /// that drops this `PendingOrigination` without reaching
    /// `finish_origination`'s success case must call this, or the relay
    /// thread (and the sockets it owns) leaks — `finish_origination` itself
    /// takes ownership of `early_relay` via its consuming destructure, so it
    /// needs no separate stop call.
    fn stop_early_relay(&mut self) {
        if let Some((stop, _meter, _chosen)) = self.early_relay.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.early_veth_socket = None;
    }

    /// Send a CANCEL for a still-pending INVITE and move to `AwaitingCancel`,
    /// where the response arm handles the `487`/racing-`200` via `inbound.rx`.
    /// Valid from `AwaitingCarrier`. The origination is kept alive (a short
    /// `CANCEL_RESPONSE_TIMEOUT` deadline) so a racing answer is still ACKed and
    /// BYE'd rather than leaking (greptile PR #35).
    fn begin_cancel(&mut self, session: &mut crate::ims::RegisteredSession) {
        let sent = self.send_cancel_now(session);
        self.step = OriginationStep::AwaitingCancel { sent };
        // Once the CANCEL is out, wait a short window for its response; if it
        // could not be sent yet, allow a much longer window to keep retrying
        // rather than dropping a still-live INVITE (greptile PR #35).
        self.deadline = Instant::now()
            + if sent {
                CANCEL_RESPONSE_TIMEOUT
            } else {
                CANCEL_SEND_MAX_WAIT
            };
    }

    /// Sends CANCEL for a pending outbound INVITE we're giving up on before a
    /// final response ever arrived (RFC 3261 §9.1) — reusing the original
    /// INVITE's own branch/CSeq number, since this targets that same
    /// transaction rather than starting a new one. Found live
    /// (specs/025-outbound-calling review): abandoning the transaction without
    /// this left the carrier free to keep ringing the destination for as long
    /// as *it* was willing to wait, regardless of how long we'd already given
    /// up.
    ///
    /// **Does not read the response** (specs/029, greptile PR #35): the CANCEL's
    /// outcome — a `487` for the INVITE, or a `200` that raced it — arrives on
    /// `inbound.rx` and is handled via the `AwaitingCancel` step. The old
    /// version read the carrier socket directly here, which raced the
    /// always-running client-reader thread (research R2): if that thread grabbed
    /// the racing `200`, this timed out and sent no ACK/BYE, leaving a phantom
    /// carrier leg — the exact failure this whole feature exists to prevent.
    ///
    /// Returns whether the CANCEL was actually sent — a `false` means the
    /// transport was momentarily unavailable and the caller should keep the
    /// origination alive and retry, rather than assume the INVITE was cancelled.
    fn send_cancel_now(&mut self, session: &mut crate::ims::RegisteredSession) -> bool {
        let cancel = crate::ims::call::build_cancel(&crate::ims::call::CancelParts {
            request_uri: &self.callee_uri,
            route_headers: &self.route_headers,
            via_transport: self.via_transport,
            local_addr: session.local_addr,
            public_uri: &session.origination_identity(),
            callee_uri: &self.callee_uri,
            call_id: &self.call_id,
            from_tag: &self.from_tag,
            cseq: self.invite_cseq,
            branch: &self.branch,
        });
        let Ok(transport) = session.transport_mut() else {
            return false;
        };
        if transport.send(&cancel).is_ok() {
            tracing::info!(call_id = %self.call_id, "outbound: sent CANCEL for an abandoned INVITE");
            true
        } else {
            false
        }
    }

    /// The carrier answered despite our CANCEL — a `200` that raced the `487`.
    /// ACK it (reusing the INVITE's branch/CSeq, §17.1.1.3) then immediately
    /// BYE, or the carrier leg would stay up with nothing on our end tracking
    /// it. Best-effort; there is nothing to retry from here.
    fn ack_and_bye_racing_answer(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        resp: &SipResponse,
    ) {
        tracing::warn!(
            call_id = %self.call_id,
            "outbound: carrier answered despite CANCEL; sending ACK then BYE to hang up"
        );
        let to_header = resp.header("To").unwrap_or(&self.callee_uri).to_string();
        let ack = crate::ims::call::build_ack(&crate::ims::call::AckParts {
            request_uri: &self.callee_uri,
            route_headers: &self.route_headers,
            via_transport: self.via_transport,
            local_addr: session.local_addr,
            public_uri: &session.origination_identity(),
            to_header: &to_header,
            call_id: &self.call_id,
            from_tag: &self.from_tag,
            cseq: self.invite_cseq,
            branch: &self.branch,
        });
        let _ = session.transport_mut().and_then(|t| t.send(&ack));
        let bye = crate::ims::call::build_bye(&crate::ims::call::AckParts {
            request_uri: &self.callee_uri,
            route_headers: &self.route_headers,
            via_transport: self.via_transport,
            local_addr: session.local_addr,
            public_uri: &session.origination_identity(),
            to_header: &to_header,
            call_id: &self.call_id,
            from_tag: &self.from_tag,
            cseq: self.invite_cseq + self.extra_cseq + 1,
            branch: &format!("z9hG4bK{}", random_hex(6)),
        });
        let _ = session.transport_mut().and_then(|t| t.send(&bye));
    }

    /// Acknowledge a reliable provisional response (RFC 3262).
    ///
    /// A provisional carrying `Require: 100rel` is retransmitted at T1 backoff
    /// until PRACKed, and the network abandons the call if it never is — see
    /// [`crate::ims::call::build_prack`] for the measurement. Silent when the
    /// response is an ordinary unreliable provisional, so carriers that never
    /// use 100rel are unaffected.
    fn prack_if_required(
        &mut self,
        session: &mut crate::ims::RegisteredSession,
        resp: &SipResponse,
    ) {
        let requires_100rel = resp
            .header("Require")
            .is_some_and(|v| v.to_ascii_lowercase().contains("100rel"));
        let Some(rseq) = resp
            .header("RSeq")
            .and_then(|v| v.trim().parse::<u32>().ok())
        else {
            return;
        };
        // RFC 3262 §7.1: RSeq strictly increases across one dialog's reliable
        // provisionals, so `<=` (not just `==`) catches a reordered
        // retransmission of an *older* one arriving after a newer RSeq has
        // already been PRACKed — the Gm UDP transport this bridge uses makes
        // that a real possibility, not just a defensive check.
        if !requires_100rel || self.pracked_rseq.is_some_and(|last| rseq <= last) {
            return;
        }

        // The dialog's remote target and tag come from this very response; the
        // From/tag and route set stay those of the INVITE.
        let to_header = resp.header("To").unwrap_or(&self.callee_uri).to_string();
        self.extra_cseq += 1;
        let cseq = self.invite_cseq + self.extra_cseq;
        let prack = crate::ims::call::build_prack(
            &crate::ims::call::AckParts {
                request_uri: &self.callee_uri,
                route_headers: &self.route_headers,
                via_transport: self.via_transport,
                local_addr: session.local_addr,
                public_uri: &session.origination_identity(),
                to_header: &to_header,
                call_id: &self.call_id,
                from_tag: &self.from_tag,
                cseq,
                branch: &format!("z9hG4bK{}", random_hex(6)),
            },
            &format!("{rseq} {} INVITE", self.invite_cseq),
        );
        match session.transport_mut().and_then(|t| t.send(&prack)) {
            Ok(()) => {
                self.pracked_rseq = Some(rseq);
                tracing::info!(
                    call_id = %self.call_id,
                    rseq,
                    cseq,
                    "outbound: PRACKed a reliable provisional"
                );
            }
            Err(e) => tracing::warn!(
                call_id = %self.call_id,
                rseq,
                error = %e,
                "outbound: could not send PRACK; the carrier will retransmit and may abandon the call"
            ),
        }
    }

    /// Handle a carrier response while awaiting the CANCEL's outcome. Only the
    /// *INVITE*'s own final resolves the transaction: a `200` on the INVITE
    /// raced the CANCEL and is ACKed then BYE'd; a `487` (or any other INVITE
    /// final) means it is dead. The `200` answering the CANCEL request itself
    /// (CSeq `... CANCEL`) merely confirms the CANCEL — acting on it would end
    /// tracking too early and miss a racing INVITE answer, leaving a phantom leg
    /// (greptile PR #35). Provisionals and the CANCEL's own response keep us
    /// waiting.
    fn on_cancel_response(
        &mut self,
        resp: &SipResponse,
        session: &mut crate::ims::RegisteredSession,
    ) -> OriginationStatus {
        let is_invite = resp
            .header("CSeq")
            .and_then(super::ping::cseq_method)
            .is_some_and(|m| m.eq_ignore_ascii_case("INVITE"));
        if !is_invite || resp.status < 200 {
            return OriginationStatus::Pending;
        }
        if resp.status == 200 {
            self.ack_and_bye_racing_answer(session, resp);
        }
        OriginationStatus::Ended
    }

    /// Advance on a carrier response delivered via `inbound.rx`. Returns
    /// `Ended` (and has already sent `CallFailed`/ACKed as needed) when the
    /// attempt is resolved as a failure; `Pending` while it is still in flight
    /// (including the `200 OK → AwaitingVeth` transition).
    pub(super) fn on_carrier_response(
        &mut self,
        resp: &SipResponse,
        session: &mut crate::ims::RegisteredSession,
    ) -> OriginationStatus {
        // Only the carrier-wait phase interprets responses as the INVITE's
        // outcome. In the other phases a Call-ID-matched response is not a fresh
        // final to act on (greptile PR #35):
        match self.step {
            OriginationStep::AwaitingCancel { .. } => {
                return self.on_cancel_response(resp, session)
            }
            OriginationStep::AwaitingVeth { .. } => {
                // The INVITE's final was already handled and the veth leg is
                // being placed. Re-running the answer path would spawn a second
                // veth listener and send `CallPlaced` twice; ignore the response
                // instead, exactly as the old blocking code did (it never read
                // the socket again during the veth wait).
                //
                // A retransmitted `2xx` (our ACK was lost, only possible on a
                // UDP Gm) is therefore not re-ACKed here — and neither is one on
                // an already-bridged `ActiveCall`, nor on an inbound call
                // (greptile PR #35 round 4). Retransmitted-2xx re-ACK is a
                // pre-existing gap orthogonal to this feature and uniform across
                // both call directions; closing it belongs in a dedicated
                // change, not piecemeal on only the outbound veth window. Moot
                // on the TCP Gm transport this deployment uses.
                tracing::debug!(
                    call_id = %self.call_id,
                    status = resp.status,
                    "ignoring a carrier response after the final was already handled"
                );
                return OriginationStatus::Pending;
            }
            OriginationStep::AwaitingCarrier => {}
        }

        // Only the INVITE's own responses resolve this attempt. A response
        // echoes its request's `CSeq`, so anything else sharing the `Call-ID`
        // — notably the `200 OK` for a `PRACK` — must be dropped here.
        //
        // Measured on Jio 2026-08-15, and caused by adding PRACK without
        // teaching this path about it: the `183` was PRACKed, the PRACK's own
        // `200 OK` (`CSeq: 6 PRACK`) was taken for the INVITE's final, we sent
        // `CSeq: 5 ACK` for a transaction that had no final response, and the
        // network killed the call 66 ms later with
        // `487 Request Terminated - P - 14018 - invalid SDP offer or answer`.
        // The destination never rang, and the call reported itself "placed and
        // bridged".
        if let Some(method) = resp.header("CSeq").and_then(super::ping::cseq_method) {
            if !method.eq_ignore_ascii_case("INVITE") {
                tracing::debug!(
                    call_id = %self.call_id,
                    status = resp.status,
                    method,
                    "ignoring a response for another transaction in this dialog"
                );
                return OriginationStatus::Pending;
            }
        }

        // First response of any kind (even a provisional) switches from the
        // short "did the network acknowledge us at all" window to the long
        // ring window — the same rule the old blocking origination read
        // applied internally before this became a poll loop.
        if !self.any_response_seen {
            self.any_response_seen = true;
            self.deadline = Instant::now() + OUTBOUND_RING_TIMEOUT;
        }

        if resp.status < 200 {
            tracing::info!(status = resp.status, reason = %resp.reason, "provisional response");
            // Keep the first provisional SDP we see: with a reliable provisional
            // this is the answer, and the 2xx that follows will be empty.
            if self.provisional_answer.is_none() && !resp.body.trim().is_empty() {
                tracing::debug!(
                    status = resp.status,
                    "outbound: provisional carried the SDP answer"
                );
                self.provisional_answer = Some(resp.body.clone());
            }
            self.prack_if_required(session, resp);
            // Relay ringback exactly once, and advance the lifecycle to
            // `PbxRinging` — the transition the old outbound path skipped, so
            // no successful outbound call ever recorded as reaching `Bridged`
            // (research R5).
            if resp.status == 180 && !self.ringing_relayed {
                self.ringing_relayed = true;
                self.lifecycle.advance_to(CallStage::PbxRinging);
                let _ = write_msg(
                    &mut self.control,
                    &ControlMessage::CallRinging {
                        call_id: self.call_id.clone(),
                    },
                );
            }

            // specs/037-p-early-media: the first SDP-bearing provisional is
            // early media (an announcement/ringback the carrier is actually
            // sending, e.g. Jio's ~13.7s `P-Early-Media: sendonly`) — relay
            // it to Agent B's local caller now instead of discarding it
            // until the real `200 OK`. At most one attempt per call
            // (`early_media_sent`); FR-006 makes a failed attempt here
            // best-effort, not a reason to fail the whole call — the
            // attempt just proceeds without early media, exactly as it did
            // before this feature existed.
            //
            // `early_media_sent` is set only once the body actually parses
            // as SDP (code review finding, specs/037-p-early-media): a
            // provisional whose non-empty body *fails* to parse (malformed,
            // or not SDP at all) must not permanently lock this out — a
            // later provisional carrying genuinely usable SDP still needs
            // its shot. The guard exists to stop a *successful* attempt
            // from being redone (e.g. on a retransmit), not to give up
            // after the first unparseable body.
            if !self.early_media_sent && !resp.body.trim().is_empty() {
                match sdp::parse_answer(&resp.body) {
                    Ok(early_answer) => {
                        self.early_media_sent = true;
                        if let Err(e) = self.rtp_socket.connect(early_answer.remote_rtp) {
                            tracing::warn!(call_id = %self.call_id, error = %e, "outbound: early-media RTP connect failed; continuing without it (FR-006)");
                        } else {
                            match spawn_veth_uas_listener(
                                self.veth_local_ip,
                                self.veth_sip_port,
                                self.wideband,
                            ) {
                                Ok(rx) => {
                                    self.early_veth_rx = Some(rx);
                                    self.early_media_codec = Some(early_answer.codec);
                                    let _ = write_msg(
                                        &mut self.control,
                                        &ControlMessage::CallEarlyMedia {
                                            call_id: self.call_id.clone(),
                                        },
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(call_id = %self.call_id, error = %e, "outbound: early-media veth listener spawn failed; continuing without it (FR-006)");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!(call_id = %self.call_id, error = %e, "outbound: provisional body did not parse as SDP; no early media");
                    }
                }
            }
            return OriginationStatus::Pending;
        }

        tracing::info!(call_id = %self.call_id, status = resp.status, reason = %resp.reason, "outbound: final INVITE response");

        if resp.status != 200 {
            // Non-2xx final: ACK reuses the INVITE's own branch/CSeq
            // (RFC 3261 §17.1.1.3), best-effort.
            let ack = crate::ims::call::build_ack(&crate::ims::call::AckParts {
                request_uri: &self.callee_uri,
                route_headers: &self.route_headers,
                via_transport: self.via_transport,
                local_addr: session.local_addr,
                public_uri: &session.origination_identity(),
                to_header: resp.header("To").unwrap_or(&self.callee_uri),
                call_id: &self.call_id,
                from_tag: &self.from_tag,
                cseq: self.invite_cseq,
                branch: &self.branch,
            });
            let _ = session.transport_mut().and_then(|t| t.send(&ack));
            self.fail(&format!("{} {}", resp.status, resp.reason));
            return OriginationStatus::Ended;
        }

        // 200 OK. Everything from here mirrors the old post-final block, in the
        // same order (SDP → RTP → ACK → dialog → veth listener → CallPlaced),
        // so every teardown path keeps its original meaning.
        // The answer is not always in the 2xx. When the carrier sends a
        // *reliable* provisional (RFC 3262), the offer/answer exchange completes
        // there and the `200 OK` arrives with `Content-Length: 0` — which is
        // exactly what Jio does: its `183 Session Progress` carries the SDP and
        // every 200 OK is empty. Parsing only the 2xx failed the call with
        // "SDP answer missing c= connection address" *after* the carrier had
        // already answered it (measured 2026-08-15).
        let answer_body = if resp.body.trim().is_empty() {
            self.provisional_answer.as_deref().unwrap_or(&resp.body)
        } else {
            &resp.body
        };
        let answer = match sdp::parse_answer(answer_body) {
            Ok(a) => a,
            Err(e) => {
                self.fail(&format!("bad SDP answer: {e}"));
                return OriginationStatus::Ended;
            }
        };
        // specs/037-p-early-media: always (re)connect, even when a
        // provisional already connected this socket for early media —
        // `UdpSocket::connect` only updates the kernel's notion of the
        // default peer for this socket, a local, instantaneous operation
        // with no I/O of its own, so reconnecting to the *same* address
        // (the common case: `answer` came from the cached
        // `provisional_answer`, identical bytes) costs nothing and causes
        // no audible gap. Skipping it unconditionally was the actual bug:
        // early media does not require `Require: 100rel` (FR-008), so an
        // early SDP is not always the guaranteed-final answer the way a
        // reliable provisional's is — a `200 OK` carrying its *own*,
        // different SDP (legal per RFC 3264) would have left RTP pointed at
        // a stale address with skipping in place.
        if let Err(e) = self.rtp_socket.connect(answer.remote_rtp) {
            self.fail(&format!("RTP connect failed: {e}"));
            return OriginationStatus::Ended;
        }

        let ack_branch = format!("z9hG4bK{}", random_hex(6));
        let ack = crate::ims::call::build_ack(&crate::ims::call::AckParts {
            request_uri: &self.callee_uri,
            route_headers: &self.route_headers,
            via_transport: self.via_transport,
            local_addr: session.local_addr,
            public_uri: &session.origination_identity(),
            to_header: resp.header("To").unwrap_or(&self.callee_uri),
            call_id: &self.call_id,
            from_tag: &self.from_tag,
            cseq: self.invite_cseq,
            branch: &ack_branch,
        });
        if let Err(e) = session.transport_mut().and_then(|t| t.send(&ack)) {
            self.fail(&format!("ACK send failed: {e}"));
            return OriginationStatus::Ended;
        }
        session.cseq = self.invite_cseq + self.extra_cseq + 1;

        let dialog = DialogInfo::from_uac_response(
            resp,
            self.route_headers.clone(),
            &self.callee_uri,
            &session.origination_identity(),
            &self.from_tag,
            session.cseq,
            session,
        );

        // specs/037-p-early-media: reuse the listener a provisional already
        // spawned, *if* it's still usable. Its `VETH_INVITE_TIMEOUT` clock
        // started back at the provisional, not now — on a carrier like Jio
        // (~13.7s of early media before the real `200 OK`) it can easily
        // have already timed out by the time we get here. Blindly reusing a
        // receiver that already holds a stale failure would hang up a
        // carrier leg the carrier just successfully answered, which FR-006
        // rules out — so a resolved-but-failed (or disconnected) early
        // listener falls back to a *fresh* one, giving Agent B a full new
        // window starting now, exactly as if no early media had ever
        // happened. A still-pending one (the common case: Agent B pairs
        // within milliseconds of `CallEarlyMedia`, so this rarely lingers
        // long enough to matter) is used as-is. An already-succeeded one is
        // forwarded through a fresh one-shot channel so the `AwaitingVeth`
        // poll loop below can treat every case identically.
        //
        // Spawn *before* telling Agent B to call in — same ordering
        // `handle_invite` uses for the inbound direction, so the listener is
        // guaranteed up by the time Agent B's `Call::make` reaches it.
        // specs/037-p-early-media: if the relay is already running (started
        // once the early veth handshake resolved, `tick_pending_origination`),
        // there is nothing left to wait for here — `veth_rx: None` tells
        // `finish_origination` to reuse it rather than spawn or wait for
        // anything. Otherwise, unchanged from before that field existed:
        // reuse an early handshake that's already succeeded, retry fresh if
        // it already failed, or spawn fresh if none was ever attempted.
        let veth_rx = if self.early_relay.is_some() {
            None
        } else {
            Some(match self.early_veth_rx.take() {
                Some(rx) => match rx.try_recv() {
                    Err(mpsc::TryRecvError::Empty) => rx,
                    Ok(Ok(result)) => {
                        let (tx, fresh_rx) = mpsc::channel();
                        let _ = tx.send(Ok(result));
                        fresh_rx
                    }
                    Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                        tracing::warn!(call_id = %self.call_id, "outbound: early-media veth handshake had already failed by the real 200 OK; giving Agent B a fresh window (FR-006)");
                        match spawn_fresh_veth_listener_or_hangup(
                            session,
                            &mut self.control,
                            &dialog,
                            &self.call_id.clone(),
                            self.veth_local_ip,
                            self.veth_sip_port,
                            self.wideband,
                        ) {
                            Some(rx) => rx,
                            None => return OriginationStatus::Ended,
                        }
                    }
                },
                None => match spawn_fresh_veth_listener_or_hangup(
                    session,
                    &mut self.control,
                    &dialog,
                    &self.call_id.clone(),
                    self.veth_local_ip,
                    self.veth_sip_port,
                    self.wideband,
                ) {
                    Some(rx) => rx,
                    None => return OriginationStatus::Ended,
                },
            })
        };

        if let Err(e) = write_msg(
            &mut self.control,
            &ControlMessage::CallPlaced {
                call_id: self.call_id.clone(),
            },
        ) {
            // Same leak as above: the carrier leg is already up. Best-effort
            // even though this very write just failed.
            tracing::warn!(call_id = %self.call_id, error = %e, "outbound: failed to notify Agent B the carrier leg is up");
            let call_id = self.call_id.clone();
            hangup_answered_carrier_leg(
                session,
                &mut self.control,
                &dialog,
                &call_id,
                reason::TRANSPORT_ERROR,
            );
            return OriginationStatus::Ended;
        }

        self.step = OriginationStep::AwaitingVeth {
            dialog,
            veth_rx,
            answer_codec: answer.codec,
        };
        self.deadline = Instant::now() + VETH_INVITE_TIMEOUT;
        OriginationStatus::Pending
    }
}

/// Advance a pending origination on a timer tick (no new carrier response):
/// watch Agent B's control channel for a caller hangup, enforce the current
/// deadline, and — once the carrier has answered — pick up Agent B's veth leg.
/// May take `*pending` (leaving it `None`) when the attempt resolves; returns
/// the bridged `ActiveCall` only on the success path.
pub(super) fn tick_pending_origination(
    pending: &mut Option<PendingOrigination>,
    session: &mut crate::ims::RegisteredSession,
) -> Option<ActiveCall> {
    let mut p = pending.take()?;

    // 1. A caller hangup (or Agent B vanishing) abandons the attempt (FR-003).
    //    A `CallEnded` naming a different call is ignored, never acted on
    //    (FR-010).
    // Already abandoned and only waiting out the CANCEL: do not re-enter the
    // abandon path. See `PendingOrigination::abandoned`.
    let abandoned = if p.abandoned {
        false
    } else {
        match p.ctrl_rx.try_recv() {
            Ok(ControlMessage::CallEnded { call_id, .. }) if call_id == p.call_id => {
                tracing::info!(call_id = %p.call_id, "outbound: caller abandoned the attempt; abandoning the carrier leg");
                true
            }
            Ok(ControlMessage::CallEnded { call_id, .. }) => {
                tracing::warn!(this = %p.call_id, other = %call_id, "ignoring CallEnded for a different call during origination");
                false
            }
            Ok(other) => {
                tracing::debug!(message = ?other, "ignoring control message during origination");
                false
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                tracing::warn!(call_id = %p.call_id, "Agent B control connection dropped during origination; abandoning");
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
        }
    };
    if abandoned {
        p.abandoned = true;
        abandon_origination(&mut p, session);
        // If that left us awaiting the CANCEL's outcome, keep the origination
        // alive so a racing `200` is still ACKed and BYE'd (greptile PR #35);
        // otherwise it is fully resolved and dropped.
        if matches!(p.step, OriginationStep::AwaitingCancel { .. }) {
            *pending = Some(p);
        }
        return None;
    }

    // 1.5. specs/037-p-early-media: if a provisional already has a veth
    // handshake in flight (`early_veth_rx`), check whether it resolved.
    // Signaling alone — Agent B pairing and answering `183`, already done
    // when `CallEarlyMedia` was sent — moves no audio; starting the actual
    // relay here is what makes early media audible. Found live 2026-08-16:
    // without this, pairing/answering succeeded but a real test call
    // reported zero RTP packets in either direction, because the relay this
    // codebase already had (`spawn_relay`/`spawn_transcoding_relay`) was
    // only ever reachable via `finish_origination`, itself only reachable
    // via the real `200 OK` — a call that never reaches `200 OK` (e.g. the
    // carrier plays an announcement then rejects with `480`, exactly Jio's
    // diagnosed behavior) never ran that path at all.
    if p.early_relay.is_none() {
        if let Some(rx) = &p.early_veth_rx {
            match rx.try_recv() {
                Ok(Ok(veth_result)) => {
                    p.early_veth_rx = None;
                    let early_codec = p
                        .early_media_codec
                        .expect("early_media_codec is always set alongside early_veth_rx");
                    match offered_chosen_codec(early_codec) {
                        // Clone both sockets: one clone of each goes into the
                        // relay, the originals stay available in `p` — the
                        // carrier one for the existing unconditional
                        // reconnect at the real `200 OK`, the veth one
                        // (`early_veth_socket`) solely in case that `200 OK`
                        // selects a different codec than this one and the
                        // relay needs rebuilding from scratch (code review
                        // finding — see `early_relay`'s doc comment).
                        Some(chosen) => {
                            match (p.rtp_socket.try_clone(), veth_result.rtp_socket.try_clone()) {
                                (Ok(carrier_sock), Ok(veth_sock_clone)) => {
                                    let stop = Arc::new(AtomicBool::new(false));
                                    let meter = crate::ims::media_stats::MediaMeter::new();
                                    let transcoding = chosen.codec != veth_result.codec.codec;
                                    let started = if transcoding {
                                        crate::ims::transcode::spawn_transcoding_relay(
                                            carrier_sock,
                                            veth_sock_clone,
                                            chosen,
                                            veth_result.codec,
                                            stop.clone(),
                                            &meter,
                                        )
                                        .is_ok()
                                    } else {
                                        spawn_relay(
                                            carrier_sock,
                                            veth_sock_clone,
                                            stop.clone(),
                                            &meter,
                                            chosen.dtmf_payload_type,
                                            veth_result.codec.dtmf_payload_type,
                                        );
                                        true
                                    };
                                    if started {
                                        tracing::info!(call_id = %p.call_id, transcoding, "outbound: early media relay started");
                                        p.early_relay = Some((stop, meter, chosen));
                                        p.early_veth_socket =
                                            Some((veth_result.rtp_socket, veth_result.codec));
                                    } else {
                                        tracing::warn!(call_id = %p.call_id, "outbound: early-media transcoding relay failed to start; continuing without it (FR-006)");
                                    }
                                }
                                (Err(e), _) | (_, Err(e)) => {
                                    tracing::warn!(call_id = %p.call_id, error = %e, "outbound: could not clone an RTP socket for early media; continuing without it (FR-006)");
                                }
                            }
                        }
                        None => {
                            tracing::warn!(call_id = %p.call_id, codec = early_codec.name(), "outbound: carrier's early SDP selected a codec we never offered; continuing without early media (FR-006)");
                        }
                    }
                }
                Ok(Err(e)) => {
                    tracing::debug!(call_id = %p.call_id, error = %e, "outbound: early-media veth handshake failed; the real 200 OK path will retry (FR-006)");
                    p.early_veth_rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    tracing::debug!(call_id = %p.call_id, "outbound: early-media veth listener thread gone without a result; the real 200 OK path will retry (FR-006)");
                    p.early_veth_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    // 2. Read the veth channel without holding a borrow into `p` across a
    // move. `veth_rx: None` means the relay is already running via early
    // media (step 1.5, an earlier tick) — ready immediately, nothing to
    // poll.
    let veth_ready = match &p.step {
        OriginationStep::AwaitingVeth { veth_rx: None, .. } => Some(Ok(None)),
        OriginationStep::AwaitingVeth {
            veth_rx: Some(rx), ..
        } => Some(rx.try_recv().map(Some)),
        _ => None,
    };
    match veth_ready {
        // Agent B's veth leg arrived — bridge the two legs. `None` here
        // means they're already bridged (early media); `finish_origination`
        // just finalizes the signaling around the relay already running.
        Some(Ok(veth_result)) => return finish_origination(p, veth_result, session),
        // Nothing yet: enforce the veth deadline, else keep waiting.
        Some(Err(mpsc::TryRecvError::Empty)) => {
            if Instant::now() >= p.deadline {
                tracing::warn!(call_id = %p.call_id, "outbound: Agent B's veth call never arrived");
                hangup_pending_carrier_leg(&mut p, session, reason::VETH_LEG_FAILED);
            } else {
                *pending = Some(p);
            }
            return None;
        }
        Some(Err(mpsc::TryRecvError::Disconnected)) => {
            tracing::warn!(call_id = %p.call_id, "outbound: veth listener stopped before answering");
            hangup_pending_carrier_leg(&mut p, session, reason::VETH_LEG_FAILED);
            return None;
        }
        None => {}
    }

    // 3. Awaiting the carrier or its CANCEL response — enforce the deadline,
    //    and retry a CANCEL that could not be sent yet.
    let awaiting_cancel = matches!(p.step, OriginationStep::AwaitingCancel { .. });
    let cancel_unsent = matches!(p.step, OriginationStep::AwaitingCancel { sent: false });

    if Instant::now() >= p.deadline {
        if awaiting_cancel {
            // Best-effort exhausted. Either the CANCEL's response never arrived
            // within its window, or (worse) the CANCEL could not be sent at all
            // within the long retry window — in that case the INVITE may linger
            // at the carrier, but the transport being dead this long means the
            // session is being re-established anyway.
            if cancel_unsent {
                tracing::warn!(call_id = %p.call_id, "outbound: could not send CANCEL within the retry window; giving up");
            } else {
                tracing::debug!(call_id = %p.call_id, "outbound: CANCEL response window elapsed");
            }
        } else {
            // AwaitingCarrier timeout — our own give-up. Tell Agent B, then
            // CANCEL and wait out the outcome via `inbound.rx` (AwaitingCancel),
            // so a racing answer is cleaned up rather than leaking.
            tracing::warn!(call_id = %p.call_id, "outbound: no final response from carrier in time");
            p.fail(&format!(
                "{}: no final response from carrier",
                reason::CARRIER_TIMEOUT
            ));
            p.begin_cancel(session);
            *pending = Some(p);
        }
        return None;
    }

    // A CANCEL that failed to send earlier (transport momentarily unavailable)
    // is retried until it goes out; only then is the response window armed, so
    // a still-live INVITE is never dropped as if it had been cancelled
    // (greptile PR #35, round 2).
    if cancel_unsent && p.send_cancel_now(session) {
        p.step = OriginationStep::AwaitingCancel { sent: true };
        p.deadline = Instant::now() + CANCEL_RESPONSE_TIMEOUT;
    }

    *pending = Some(p);
    None
}

/// Abandon an in-flight attempt because the originating caller is gone. Tears
/// the carrier side down as the current step requires: CANCEL a still-pending
/// INVITE (→ `AwaitingCancel`, kept alive so a racing answer is cleaned up), or
/// BYE an already-answered leg. No `CallFailed` is sent — Agent B initiated
/// this and has already reported `CallerAbandoned`.
fn abandon_origination(p: &mut PendingOrigination, session: &mut crate::ims::RegisteredSession) {
    // specs/037-p-early-media: covers the `AwaitingCarrier` case (early
    // media active, real final response never arrived) directly; a
    // no-op if there was never a relay to stop, or if it's about to be
    // covered again by `hangup_pending_carrier_leg` below (idempotent —
    // `stop_early_relay` uses `Option::take`).
    p.stop_early_relay();
    match &p.step {
        OriginationStep::AwaitingCarrier => p.begin_cancel(session),
        OriginationStep::AwaitingVeth { .. } => {
            hangup_pending_carrier_leg(p, session, reason::CALLER_HANGUP);
        }
        // Already cancelling — a duplicate `CallEnded`; nothing more to do.
        OriginationStep::AwaitingCancel { .. } => {}
    }
}

/// BYE an already-answered carrier leg for a pending (not-yet-bridged) attempt,
/// reusing the dialog captured at `200 OK`. Only valid in `AwaitingVeth`.
fn hangup_pending_carrier_leg(
    p: &mut PendingOrigination,
    session: &mut crate::ims::RegisteredSession,
    reason: &str,
) {
    // specs/037-p-early-media: a relay can already be running here (started
    // during early media, before the veth handshake this function is
    // reacting to a failure/timeout/abandonment of) — stop it, or it leaks.
    p.stop_early_relay();
    // Disjoint field borrows: `dialog` reads `p.step`, the BYE writes
    // `p.control`, `call_id` is copied out first — so none of these alias.
    if let OriginationStep::AwaitingVeth { dialog, .. } = &p.step {
        let call_id = p.call_id.clone();
        hangup_answered_carrier_leg(session, &mut p.control, dialog, &call_id, reason);
    }
}

/// The `ChosenCodec` for a codec `sdp::build_offer` actually offers (PCMU or
/// AMR-WB) — `None` for anything else (`AmrNb`, `L16`), which an answer can
/// only select by carrier misbehavior, not a codec we'd ever have agreed to.
/// Shared between `finish_origination`'s codec resolution and
/// `tick_pending_origination`'s early-media relay start (specs/037-p-early-media)
/// so the two don't drift.
fn offered_chosen_codec(negotiated: NegotiatedCodec) -> Option<sdp::ChosenCodec> {
    match negotiated {
        NegotiatedCodec::Pcmu => Some(sdp::ChosenCodec {
            codec: NegotiatedCodec::Pcmu,
            payload_type: 0,
            octet_aligned: false,
            // `build_offer` never offers `telephone-event` on this leg
            // (specs/041 conformance review, RTP-02's sibling gap) — nothing
            // for an answer to have echoed.
            dtmf_payload_type: None,
        }),
        NegotiatedCodec::AmrWb => Some(sdp::ChosenCodec {
            codec: NegotiatedCodec::AmrWb,
            payload_type: 96,
            octet_aligned: true,
            dtmf_payload_type: None,
        }),
        _ => None,
    }
}

/// Bridge a carrier leg (already answered and ACKed) to Agent B's veth leg — the
/// back half of the old `originate_and_bridge`, run once the veth call arrives.
/// Consumes `p`. Returns the `ActiveCall`, or `None` after tearing the carrier
/// leg down if bridging fails.
fn finish_origination(
    p: PendingOrigination,
    veth_result: Option<BridgeResult<VethUasResult>>,
    session: &mut crate::ims::RegisteredSession,
) -> Option<ActiveCall> {
    let PendingOrigination {
        step,
        call_id,
        from_tag,
        destination,
        control,
        ctrl_rx,
        rtp_socket,
        mut lifecycle,
        early_relay,
        early_veth_socket,
        ..
    } = p;
    let mut control = control;
    let OriginationStep::AwaitingVeth {
        dialog,
        answer_codec,
        ..
    } = step
    else {
        // Unreachable: only ever called from the `AwaitingVeth` arm.
        return None;
    };

    // specs/037-p-early-media: `veth_result: None` means the relay is
    // already running — it was started once the early veth handshake
    // resolved (`tick_pending_origination`'s early-relay check), well
    // before this real `200 OK` ever arrived. Reuse it rather than spawn a
    // second one; `rtp_socket` here is the *original* handle, unused since
    // the relay was started from a clone of it — dropped harmlessly at the
    // end of this function, the clone the relay thread owns keeps the
    // underlying socket alive.
    let (stop, meter, carrier_codec_name, transcoding) = match (veth_result, early_relay) {
        (None, Some((stop, meter, early_chosen))) => match offered_chosen_codec(answer_codec) {
            Some(final_chosen) if final_chosen.codec == early_chosen.codec => {
                // The real 200 OK agrees with what the relay was already
                // built for — trust it, exactly as before this check
                // existed.
                let transcoding = early_veth_socket
                    .as_ref()
                    .is_some_and(|(_, vc)| early_chosen.codec != vc.codec);
                (stop, meter, early_chosen.codec.name(), transcoding)
            }
            Some(final_chosen) => {
                // Real 200 OK selected a *different* codec than the early,
                // non-100rel provisional did (code review finding — RFC
                // 3264 doesn't bind them together, so this is legal, not a
                // carrier bug). Reusing the running relay here would feed
                // it RTP payloads its encoder/decoder pair was never built
                // for, corrupting or silently dropping audio rather than
                // erroring. Stop it and rebuild from the retained veth
                // socket instead — `rtp_socket` (the carrier side) is
                // already reconnected to the real answer's address by the
                // unconditional reconnect above this function's call site.
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                let Some((veth_sock, veth_codec)) = early_veth_socket else {
                    tracing::error!(
                        call_id,
                        "outbound: early relay had no retained veth socket to rebuild the relay from"
                    );
                    hangup_answered_carrier_leg(
                        session,
                        &mut control,
                        &dialog,
                        &call_id,
                        reason::TRANSPORT_ERROR,
                    );
                    return None;
                };
                let new_stop = Arc::new(AtomicBool::new(false));
                let new_meter = crate::ims::media_stats::MediaMeter::new();
                let transcoding = final_chosen.codec != veth_codec.codec;
                let relay_result = if transcoding {
                    crate::ims::transcode::spawn_transcoding_relay(
                        rtp_socket,
                        veth_sock,
                        final_chosen,
                        veth_codec,
                        new_stop.clone(),
                        &new_meter,
                    )
                } else {
                    spawn_relay(
                        rtp_socket,
                        veth_sock,
                        new_stop.clone(),
                        &new_meter,
                        final_chosen.dtmf_payload_type,
                        veth_codec.dtmf_payload_type,
                    );
                    Ok(())
                };
                if let Err(e) = relay_result {
                    tracing::error!(call_id, error = %e, "outbound: failed to rebuild the media relay for the real answer's codec");
                    hangup_answered_carrier_leg(
                        session,
                        &mut control,
                        &dialog,
                        &call_id,
                        reason::TRANSPORT_ERROR,
                    );
                    return None;
                }
                tracing::info!(
                    call_id,
                    old_codec = early_chosen.codec.name(),
                    new_codec = final_chosen.codec.name(),
                    "outbound: real 200 OK selected a different codec than early media; rebuilt the relay"
                );
                (new_stop, new_meter, final_chosen.codec.name(), transcoding)
            }
            None => {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                tracing::error!(
                    call_id,
                    codec = answer_codec.name(),
                    "outbound: carrier answered with a codec we never offered"
                );
                hangup_answered_carrier_leg(
                    session,
                    &mut control,
                    &dialog,
                    &call_id,
                    reason::TRANSPORT_ERROR,
                );
                return None;
            }
        },
        (None, None) => {
            // Unreachable: `veth_rx: None` in `AwaitingVeth` is only ever
            // constructed alongside `early_relay: Some(..)`.
            tracing::error!(
                call_id,
                "outbound: AwaitingVeth had no veth result and no early relay"
            );
            hangup_answered_carrier_leg(
                session,
                &mut control,
                &dialog,
                &call_id,
                reason::TRANSPORT_ERROR,
            );
            return None;
        }
        (Some(veth_result), _) => {
            let veth = match veth_result.map_err(|e| e.to_string()) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(call_id, error = %e, "outbound: Agent B's veth call failed");
                    hangup_answered_carrier_leg(
                        session,
                        &mut control,
                        &dialog,
                        &call_id,
                        reason::VETH_LEG_FAILED,
                    );
                    return None;
                }
            };

            // `parse_answer` only returns which codec the answer picked
            // (`NegotiatedCodec`), not a payload type — by RFC 3264, a re-used
            // dynamic payload type on the answer must mean what *our own
            // offer* said it meant, so there is nothing to re-parse.
            // Reconstructs the rest from what `sdp::build_offer` is known to
            // always send.
            let chosen = match offered_chosen_codec(answer_codec) {
                Some(c) => c,
                None => {
                    // Never offered — `sdp::build_offer` only ever lists
                    // PCMU/AMR-WB. Agent B's phone/PBX leg is already
                    // answered by this point, so leaving it stranded on dead
                    // air on top of leaking the carrier leg would compound
                    // the failure.
                    tracing::error!(
                        call_id,
                        codec = answer_codec.name(),
                        "outbound: carrier answered with a codec we never offered"
                    );
                    hangup_answered_carrier_leg(
                        session,
                        &mut control,
                        &dialog,
                        &call_id,
                        reason::TRANSPORT_ERROR,
                    );
                    return None;
                }
            };

            let stop = Arc::new(AtomicBool::new(false));
            let meter = crate::ims::media_stats::MediaMeter::new();
            let transcoding = chosen.codec != veth.codec.codec;
            let relay_result = if transcoding {
                crate::ims::transcode::spawn_transcoding_relay(
                    rtp_socket,
                    veth.rtp_socket,
                    chosen,
                    veth.codec,
                    stop.clone(),
                    &meter,
                )
            } else {
                spawn_relay(
                    rtp_socket,
                    veth.rtp_socket,
                    stop.clone(),
                    &meter,
                    chosen.dtmf_payload_type,
                    veth.codec.dtmf_payload_type,
                );
                Ok(())
            };
            if let Err(e) = relay_result {
                tracing::error!(call_id, error = %e, "outbound: failed to start media relay");
                hangup_answered_carrier_leg(
                    session,
                    &mut control,
                    &dialog,
                    &call_id,
                    reason::TRANSPORT_ERROR,
                );
                return None;
            }
            (stop, meter, chosen.codec.name(), transcoding)
        }
    };

    tracing::info!(
        call_id,
        destination,
        carrier_codec = carrier_codec_name,
        transcoding,
        "outbound: call placed and bridged"
    );

    // `PbxRinging → Bridged`. Force `PbxRinging` first for the rare carrier
    // that answers `200` with no `180` in between (`advance_to` refuses the
    // illegal `Answering → Bridged` jump and would otherwise leave the stage
    // stuck) — research R5.
    lifecycle.advance_to(CallStage::PbxRinging);
    lifecycle.advance_to(CallStage::Bridged);

    Some(ActiveCall {
        control,
        ctrl_rx,
        stop,
        // Our own from_tag doubles as `to_tag`: `handle_bye`'s
        // `build_200_ok_bye` only falls back to it when the incoming request's
        // own To header lacks a tag, which a real in-dialog BYE never does.
        to_tag: from_tag,
        dialog,
        call_id,
        caller: destination,
        answered_at: Utc::now(),
        answered_instant: Instant::now(),
        meter,
        lifecycle,
        // We placed this call ourselves — there is no inbound INVITE of our
        // own to have answered, so any INVITE later naming this dialog is a
        // modification attempt (`InDialogInvite::ReInvite`), never a
        // retransmission. See `CachedInviteAnswer`'s doc comment.
        answered_invite: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr, TcpListener};

    /// A connected loopback pair for `PendingOrigination::control` — the
    /// server half must stay alive for the whole test (an accepted-then-
    /// dropped peer would reset the connection on the next write), so
    /// callers keep both halves in scope even though only the client half
    /// is read from `control`'s field position.
    fn control_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    /// Fields here are never touched by the early-media path under test
    /// (`prack_if_required` returns before reading `session` when the
    /// response carries no `RSeq`) — values are realistic-but-arbitrary,
    /// not meant to resemble a real registration.
    fn test_session() -> crate::ims::RegisteredSession {
        crate::ims::RegisteredSession {
            transport: None,
            realm: "ims.mnc001.mcc001.3gppnetwork.org".to_string(),
            public_uri: "sip:9000000000@ims.mnc001.mcc001.3gppnetwork.org".to_string(),
            local_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5060),
            contact_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5060),
            use_tcp: true,
            cseq: 2,
            gm_state: None,
            xfrm_proto: "esp",
            status: 200,
            reason: "OK".to_string(),
            headers: Vec::new(),
            call_id: "reg-1".to_string(),
            from_tag: "regtag".to_string(),
            pcscf_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5060),
            imei: "000000000000000".to_string(),
        }
    }

    /// Returns the `ControlMessage` sender alongside the `PendingOrigination`
    /// — most callers just let it drop (equivalent to today's behavior,
    /// which never kept it), but `tick_pending_origination` treats a
    /// disconnected `ctrl_rx` as caller-abandonment, so any test that drives
    /// a tick (rather than calling `on_carrier_response` directly) must keep
    /// it alive for the duration.
    fn test_pending(
        call_id: &str,
        control: TcpStream,
    ) -> (PendingOrigination, mpsc::Sender<ControlMessage>) {
        let (ctrl_tx, ctrl_rx) = mpsc::channel();
        let rtp_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut lifecycle = BridgedCall::new(call_id.to_string(), "9000000001".to_string(), None);
        lifecycle.advance_to(CallStage::Answering);
        let p = PendingOrigination {
            step: OriginationStep::AwaitingCarrier,
            call_id: call_id.to_string(),
            from_tag: "ftag".to_string(),
            branch: "z9hG4bKtest".to_string(),
            invite_cseq: 1,
            extra_cseq: 0,
            pracked_rseq: None,
            provisional_answer: None,
            early_veth_rx: None,
            early_media_sent: false,
            early_media_codec: None,
            early_relay: None,
            early_veth_socket: None,
            callee_uri: "sip:9000000001@example.test".to_string(),
            route_headers: Vec::new(),
            via_transport: "TCP",
            destination: "9000000001".to_string(),
            control,
            ctrl_rx,
            rtp_socket,
            veth_local_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            veth_sip_port: 0,
            wideband: false,
            deadline: Instant::now() + Duration::from_secs(30),
            any_response_seen: false,
            ringing_relayed: false,
            abandoned: false,
            lifecycle,
        };
        (p, ctrl_tx)
    }

    fn provisional_183_with_sdp(call_id: &str) -> SipResponse {
        SipResponse {
            status: 183,
            reason: "Session Progress".to_string(),
            headers: vec![
                ("Call-ID".to_string(), call_id.to_string()),
                ("CSeq".to_string(), "1 INVITE".to_string()),
            ],
            body: "v=0\r\nc=IN IP4 127.0.0.1\r\nm=audio 40000 RTP/AVP 0\r\n".to_string(),
        }
    }

    /// specs/037-p-early-media, US1 (T006): the first SDP-bearing
    /// provisional connects the carrier RTP socket and spawns the veth
    /// listener; a retransmit of that same provisional must not redo either
    /// — reconnecting the socket a second time is exactly the kind of
    /// re-establishment SC-005's zero-gap handoff rules out.
    #[test]
    fn first_sdp_bearing_provisional_triggers_early_media_once() {
        let call_id = "out-early-1";
        let (control, _server) = control_pair();
        let mut session = test_session();
        let (mut p, _ctrl_tx) = test_pending(call_id, control);

        let resp1 = provisional_183_with_sdp(call_id);
        let status1 = p.on_carrier_response(&resp1, &mut session);
        assert!(matches!(status1, OriginationStatus::Pending));
        assert!(
            p.early_media_sent,
            "the first SDP-bearing provisional should attempt early media"
        );
        assert!(
            p.early_veth_rx.is_some(),
            "the veth listener should be spawned from the first provisional"
        );
        let peer_after_first = p.rtp_socket.peer_addr().unwrap();
        assert_eq!(
            peer_after_first,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 40000)
        );

        // A retransmit of the exact same provisional (RSeq is absent here
        // regardless, so this is indistinguishable from a genuine resend at
        // the transport level) must not reconnect the socket or spawn a
        // second listener.
        let resp2 = provisional_183_with_sdp(call_id);
        let status2 = p.on_carrier_response(&resp2, &mut session);
        assert!(matches!(status2, OriginationStatus::Pending));
        assert_eq!(
            p.rtp_socket.peer_addr().unwrap(),
            peer_after_first,
            "a retransmitted provisional must not reconnect the RTP socket"
        );
    }

    /// A provisional with no SDP body (e.g. a plain `100 Trying`/`180
    /// Ringing`) must not trigger early media at all — the no-early-media
    /// path (spec.md FR-002) stays exactly as it was.
    #[test]
    fn provisional_without_sdp_does_not_trigger_early_media() {
        let call_id = "out-early-2";
        let (control, _server) = control_pair();
        let mut session = test_session();
        let (mut p, _ctrl_tx) = test_pending(call_id, control);

        let resp = SipResponse {
            status: 180,
            reason: "Ringing".to_string(),
            headers: vec![
                ("Call-ID".to_string(), call_id.to_string()),
                ("CSeq".to_string(), "1 INVITE".to_string()),
            ],
            body: String::new(),
        };

        let status = p.on_carrier_response(&resp, &mut session);
        assert!(matches!(status, OriginationStatus::Pending));
        assert!(!p.early_media_sent);
        assert!(p.early_veth_rx.is_none());
        assert!(
            p.rtp_socket.peer_addr().is_err(),
            "an unconnected UDP socket has no peer"
        );
    }

    /// Code review finding (specs/037-p-early-media): the early-media veth
    /// listener's `VETH_INVITE_TIMEOUT` clock starts at the provisional, not
    /// at the real `200 OK` — on a carrier with a long early-media window
    /// (Jio: ~13.7s) the listener can have already timed out by the time
    /// the real answer arrives. Blindly reusing that stale, already-failed
    /// receiver would hang up a carrier leg the carrier just successfully
    /// answered — a regression against FR-006 ("this feature must not
    /// reduce the reliability of outbound call setup"). The fix falls back
    /// to a fresh listener when the reused one has already resolved to a
    /// failure; this proves that fallback actually fires rather than
    /// trusting the stale result.
    #[test]
    fn stale_early_veth_failure_falls_back_to_a_fresh_listener_at_200_ok() {
        let call_id = "out-early-3";
        let (control, _server) = control_pair();
        let mut session = test_session();
        // A real (if unreachable) UDP "connection" so the 200 OK path's ACK
        // send succeeds and this test actually reaches the veth-listener
        // decision under test, rather than failing earlier at the ACK step.
        let dummy_peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let transport =
            crate::ims::sip_client::SipTransport::connect(dummy_peer.local_addr().unwrap(), false)
                .unwrap();
        session.transport = Some(transport);

        let (mut p, _ctrl_tx) = test_pending(call_id, control);

        // Simulate an early-media veth listener that already failed (timed
        // out) by the time the real 200 OK arrives.
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(Err(crate::error::BridgeError::Ims(
            "veth INVITE timed out".to_string(),
        )));
        p.early_veth_rx = Some(rx);

        let resp = SipResponse {
            status: 200,
            reason: "OK".to_string(),
            headers: vec![
                ("Call-ID".to_string(), call_id.to_string()),
                ("CSeq".to_string(), "1 INVITE".to_string()),
                (
                    "To".to_string(),
                    "<sip:9000000001@example.test>;tag=totag".to_string(),
                ),
            ],
            body: "v=0\r\nc=IN IP4 127.0.0.1\r\nm=audio 40000 RTP/AVP 0\r\n".to_string(),
        };

        let status = p.on_carrier_response(&resp, &mut session);
        // Must still be in flight, not immediately hung up as
        // VETH_LEG_FAILED because of the stale result.
        assert!(matches!(status, OriginationStatus::Pending));
        let OriginationStep::AwaitingVeth { veth_rx, .. } = &p.step else {
            panic!("expected AwaitingVeth, got a different step");
        };
        // The critical assertion: `on_carrier_response` alone returning
        // `Pending` is not enough to prove the fix, because the *old*,
        // buggy code also reached `AwaitingVeth` here — it just carried the
        // stale, already-failed receiver forward, and the failure only
        // surfaced one tick later in `tick_pending_origination`. Proving
        // the fix means proving the receiver in `AwaitingVeth` is a fresh,
        // still-pending one, not the stale one: `try_recv` on a genuinely
        // fresh listener (nothing sent yet) must be `Empty`, whereas the
        // stale receiver would immediately yield the buffered `Err`.
        let veth_rx = veth_rx
            .as_ref()
            .expect("no early relay was running in this test, so veth_rx must be Some");
        assert!(
            matches!(veth_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "expected a fresh, still-pending veth listener, not the stale failed one"
        );
    }

    /// Regression test for the root cause behind "signaling paired and
    /// answered `183`, but a real test call reported zero RTP packets"
    /// (specs/037-p-early-media, found live 2026-08-16): pairing/answering
    /// alone moves no audio — the actual relay must start once the veth
    /// handshake resolves, not wait for the real `200 OK`. Exercises
    /// `tick_pending_origination` directly (not `on_carrier_response`) since
    /// that's where the fix lives.
    #[test]
    fn early_relay_starts_as_soon_as_the_veth_handshake_resolves() {
        let call_id = "out-early-4";
        let (control, _server) = control_pair();
        let mut session = test_session();
        let (mut p, _ctrl_tx) = test_pending(call_id, control);

        // State right after `CallEarlyMedia` fired: RTP already connected,
        // codec known, veth listener "spawned" — substituted here with a
        // channel this test controls directly rather than a real
        // `spawn_veth_uas_listener` thread.
        let carrier_peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        p.rtp_socket
            .connect(carrier_peer.local_addr().unwrap())
            .unwrap();
        p.early_media_codec = Some(NegotiatedCodec::Pcmu);
        let (veth_tx, veth_rx) = mpsc::channel();
        p.early_veth_rx = Some(veth_rx);

        // Agent B's veth leg "arrives": a real, connected UDP socket
        // standing in for the veth-side RTP endpoint.
        let veth_peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let veth_rtp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        veth_rtp.connect(veth_peer.local_addr().unwrap()).unwrap();
        veth_tx
            .send(Ok(VethUasResult {
                rtp_socket: veth_rtp,
                codec: sdp::ChosenCodec {
                    codec: NegotiatedCodec::Pcmu,
                    payload_type: 0,
                    octet_aligned: false,
                    dtmf_payload_type: None,
                },
            }))
            .unwrap();

        let mut pending = Some(p);
        let result = tick_pending_origination(&mut pending, &mut session);
        assert!(
            result.is_none(),
            "the attempt isn't answered yet — no ActiveCall should come out of this tick"
        );
        let mut p =
            pending.expect("the attempt should still be pending, now with the relay running");
        assert!(
            p.early_relay.is_some(),
            "the relay should have started as soon as the veth handshake resolved, \
             not waited for the real 200 OK"
        );
        assert!(
            p.early_veth_rx.is_none(),
            "the resolved receiver should have been cleared, not left to be polled again"
        );
        assert!(
            matches!(p.step, OriginationStep::AwaitingCarrier),
            "still waiting for the real final response — early media doesn't answer the call"
        );

        // Prove real bytes actually move — send one packet each way through
        // the now-running relay and confirm it arrives.
        carrier_peer
            .send_to(b"hello-from-carrier", p.rtp_socket.local_addr().unwrap())
            .unwrap();
        veth_peer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buf = [0u8; 64];
        let n = veth_peer.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello-from-carrier");

        // Clean up: stop the relay thread this test started.
        p.stop_early_relay();
    }

    /// Code review finding (specs/037-p-early-media): a provisional whose
    /// non-empty body fails to parse as SDP used to set `early_media_sent`
    /// unconditionally, permanently locking out early media for the rest of
    /// the attempt — even if a *later* provisional carried perfectly good
    /// SDP. Proves the fix: the flag stays clear after a bad body, and a
    /// subsequent good one still triggers early media normally.
    #[test]
    fn unparseable_provisional_body_does_not_lock_out_a_later_valid_one() {
        let call_id = "out-early-5";
        let (control, _server) = control_pair();
        let mut session = test_session();
        let (mut p, _ctrl_tx) = test_pending(call_id, control);

        let bad_resp = SipResponse {
            status: 183,
            reason: "Session Progress".to_string(),
            headers: vec![
                ("Call-ID".to_string(), call_id.to_string()),
                ("CSeq".to_string(), "1 INVITE".to_string()),
            ],
            body: "not valid sdp at all".to_string(),
        };
        let status1 = p.on_carrier_response(&bad_resp, &mut session);
        assert!(matches!(status1, OriginationStatus::Pending));
        assert!(
            !p.early_media_sent,
            "an unparseable body must not lock out a later valid one"
        );
        assert!(p.early_veth_rx.is_none());

        let good_resp = provisional_183_with_sdp(call_id);
        let status2 = p.on_carrier_response(&good_resp, &mut session);
        assert!(matches!(status2, OriginationStatus::Pending));
        assert!(
            p.early_media_sent,
            "the later, valid provisional should have triggered early media"
        );
        assert!(p.early_veth_rx.is_some());
    }

    /// Code review finding (specs/037-p-early-media): RFC 3264 doesn't bind
    /// a non-`100rel` provisional's SDP to the real `200 OK`'s — a carrier
    /// can legally answer with a different codec than its early media used.
    /// Blindly reusing the already-running early relay in that case would
    /// feed it RTP payloads its encoder/decoder pair was never built for.
    /// Proves the fix: the real `200 OK` selecting a different codec stops
    /// the early relay and builds a fresh one, rather than trusting it.
    #[test]
    fn mismatched_final_codec_rebuilds_the_relay_instead_of_reusing_it() {
        let call_id = "out-early-6";
        let (control, _server) = control_pair();
        let mut session = test_session();
        let dummy_peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let transport =
            crate::ims::sip_client::SipTransport::connect(dummy_peer.local_addr().unwrap(), false)
                .unwrap();
        session.transport = Some(transport);
        let (mut p, _ctrl_tx) = test_pending(call_id, control);

        // Early media negotiates PCMU (matching the veth leg's own PCMU,
        // so a plain passthrough relay — no transcoding needed to prove
        // this specific bug).
        let carrier_peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        p.rtp_socket
            .connect(carrier_peer.local_addr().unwrap())
            .unwrap();
        p.early_media_codec = Some(NegotiatedCodec::Pcmu);
        let (veth_tx, veth_rx) = mpsc::channel();
        p.early_veth_rx = Some(veth_rx);
        let veth_peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let veth_rtp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        veth_rtp.connect(veth_peer.local_addr().unwrap()).unwrap();
        veth_tx
            .send(Ok(VethUasResult {
                rtp_socket: veth_rtp,
                codec: sdp::ChosenCodec {
                    codec: NegotiatedCodec::Pcmu,
                    payload_type: 0,
                    octet_aligned: false,
                    dtmf_payload_type: None,
                },
            }))
            .unwrap();

        // Tick 1: the veth handshake resolves, the early (PCMU) relay starts.
        let mut pending = Some(p);
        assert!(tick_pending_origination(&mut pending, &mut session).is_none());
        let mut p = pending.expect("still pending after starting the early relay");
        let (early_stop, _early_meter, early_chosen) = p
            .early_relay
            .clone()
            .expect("the early relay should have started");
        assert_eq!(early_chosen.codec, NegotiatedCodec::Pcmu);
        assert!(
            !early_stop.load(std::sync::atomic::Ordering::Relaxed),
            "the early relay should still be running at this point"
        );

        // The real 200 OK arrives, selecting AMR-WB instead — a different
        // codec than the early relay was built for.
        let final_resp = SipResponse {
            status: 200,
            reason: "OK".to_string(),
            headers: vec![
                ("Call-ID".to_string(), call_id.to_string()),
                ("CSeq".to_string(), "1 INVITE".to_string()),
                (
                    "To".to_string(),
                    "<sip:9000000001@example.test>;tag=totag".to_string(),
                ),
            ],
            body: "v=0\r\nc=IN IP4 127.0.0.1\r\nm=audio 40001 RTP/AVP 96\r\n".to_string(),
        };
        let status = p.on_carrier_response(&final_resp, &mut session);
        assert!(matches!(status, OriginationStatus::Pending));

        // Tick 2: `veth_rx: None` (already relaying) resolves immediately —
        // `finish_origination` runs now.
        let mut pending = Some(p);
        let outcome = tick_pending_origination(&mut pending, &mut session);

        // The one invariant this test can check in every environment: the
        // stale PCMU relay must never be *reused* once a mismatch is
        // detected. Whether a fresh AMR-WB relay can actually be *built*
        // afterward depends on AMR-WB codec support being linked in
        // (`amr-linked`, not available in this test environment — its
        // encoder/decoder construction fails gracefully with a `BridgeResult`
        // error, not a panic, so this path exercises the same graceful
        // failure a real deployment without AMR-WB support would hit). Both
        // outcomes are correct here as long as the old relay was stopped:
        // an `ActiveCall` running a genuinely different relay, or a clean
        // hangup because the replacement couldn't be built.
        assert!(
            early_stop.load(std::sync::atomic::Ordering::Relaxed),
            "the mismatched-codec early relay must have been stopped, not reused"
        );
        if let Some(active_call) = outcome {
            assert!(
                !Arc::ptr_eq(&early_stop, &active_call.stop),
                "if the call finalized, it must be running a freshly built relay, not the old PCMU one"
            );
        }
    }
}
