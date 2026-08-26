//! A bridged call's state, and every way one can end.
//!
//! Split out of `agent::mod` because a call outlives the message that created
//! it: [`ActiveCall`] is handed between the inbound and outbound paths and
//! then torn down by any of four different triggers (carrier `BYE`, a PBX-side
//! `CallEnded`, Agent B's control connection dropping, or a lost network
//! attachment). Collecting them here is what makes it checkable that all four
//! report the call the same way.

use super::observability;
use crate::ims::lifecycle::BridgedCall;
use crate::ims::session::respond;
use crate::ims::session::Inbound;
use crate::ims::sip_client::{
    build_200_ok_bye, build_bye, random_hex, ByeRequest, SipRequest, SipSink,
};
use crate::vowifi::control::{read_msg, reason, write_msg, ControlMessage};
use chrono::Utc;
use std::io::BufReader;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// Answers "is the network attachment still up?" during a call, so a call whose
/// attachment genuinely dies mid-call can be ended with the cause stated,
/// distinct from the caller hanging up (FR-011).
///
/// Returns `true` while attached. LTE-only — the cellular path reads `CEREG`;
/// the Wi-Fi path passes `None`, because its ePDG tunnel is charon's to watch
/// and a lost tunnel already surfaces as the control connection dropping.
///
/// It is consulted only *during* a call and only after the media has stalled,
/// so it costs no modem traffic on a healthy call, and confirming genuine loss
/// before ending a call is what keeps a transient silence from being mistaken
/// for a dropped attachment.
pub(crate) type AttachmentHook = dyn Fn() -> bool + Send + Sync;

/// How long the carrier leg may carry no audio before the attachment is
/// checked. A real conversation with DTX still sends comfort-noise frames, so a
/// full stall this long is already abnormal; the check then decides whether it
/// is silence or a genuinely dead attachment.
const MEDIA_STALL_BEFORE_ATTACHMENT_CHECK: Duration = Duration::from_secs(6);

/// Consecutive attachment checks that must report "down" before a call is ended
/// for attachment loss. More than one so a single glitched `CEREG` read cannot
/// tear down a live call.
const ATTACHMENT_LOSS_CONFIRMATIONS: u32 = 2;

/// Minimum gap between attachment probes once the media has stalled — so a
/// stalled call is confirmed dead over a few seconds, not hammered at the
/// dispatch loop's fast poll rate.
const ATTACHMENT_PROBE_INTERVAL: Duration = Duration::from_secs(2);

/// Holds what's needed to tear a bridged call down again once a `BYE`
/// arrives — the control connection Agent B expects `CallEnded` on, and the
/// flag that stops the background RTP relay threads.
pub(super) struct ActiveCall {
    pub(super) control: TcpStream,
    /// Agent B's side of the control channel. Kept alive for the whole call so
    /// the dispatch loop hears about a hangup that starts on the *PBX* side —
    /// without it, only a carrier-originated `BYE` could ever end a call, and
    /// hanging up the SIP extension would leave the caller on a dead line.
    pub(super) ctrl_rx: mpsc::Receiver<ControlMessage>,
    pub(super) stop: Arc<AtomicBool>,
    pub(super) call_id: String,
    pub(super) to_tag: String,
    /// What's needed to hang up on the carrier ourselves, captured from the
    /// INVITE while we still had it.
    pub(super) dialog: DialogInfo,
    /// Observability bookkeeping (specs/014-vowifi-metrics-restore): who
    /// called and when the call was answered, needed at hangup time to
    /// report `CallCompleted`/write the history row.
    pub(super) caller: String,
    pub(super) answered_at: chrono::DateTime<Utc>,
    pub(super) answered_instant: Instant,
    /// Per-direction packet counts on the carrier leg, read at teardown for the
    /// FR-017 one-way-audio verdict.
    pub(super) meter: crate::ims::media_stats::MediaMeter,
    /// The transport-agnostic lifecycle record for this call (`ims::lifecycle`).
    /// A live `ActiveCall` only exists once the call actually bridged, so this
    /// is created already advanced to `CallStage::Bridged`; the dispatch loop
    /// attributes its ending through it so end-cause and success are decided by
    /// one model, not restated at each teardown site.
    pub(super) lifecycle: BridgedCall,
    /// What's needed to resend the exact `200 OK` that answered this call's
    /// INVITE, if a retransmission of it arrives (RFC 3261 §17.2.1;
    /// specs/042-dialog-transaction-identity). `None` for a call this side
    /// placed itself (UAC role, `origination.rs`) — an outbound-placed call
    /// never had an inbound INVITE of its own to answer, so any INVITE later
    /// naming that dialog is a modification attempt, never a retransmission.
    pub(super) answered_invite: Option<CachedInviteAnswer>,
}

/// See [`ActiveCall::answered_invite`].
pub(super) struct CachedInviteAnswer {
    /// The raw `CSeq` header value of the INVITE this side answered (e.g.
    /// `"1 INVITE"`). RFC 3261 §12.2.2 requires every subsequent in-dialog
    /// request to carry a strictly higher CSeq, so an exact match can only be
    /// a retransmission of this same transaction, never a fresh re-INVITE.
    pub(super) invite_cseq: String,
    pub(super) contact: String,
    pub(super) answer_sdp: String,
}

/// How an inbound `INVITE` naming the call already active on this line
/// relates to the answer already given for it (specs/042-dialog-transaction-identity,
/// MT-01/MT-02).
pub(super) enum InDialogInvite {
    /// Same Call-ID, identical CSeq to the INVITE already answered.
    RetransmittedOriginal,
    /// Same Call-ID, anything else — a genuine re-INVITE, or any inbound
    /// INVITE naming a call this side placed itself (which never has a
    /// cached answer to retransmit).
    ReInvite,
}

/// Classifies an inbound INVITE that has already been confirmed to name the
/// active call's `Call-ID` — see [`InDialogInvite`].
pub(super) fn classify_in_dialog_invite(
    req: &SipRequest,
    answered_invite: Option<&CachedInviteAnswer>,
) -> InDialogInvite {
    match answered_invite {
        Some(cached) if req.header("CSeq") == Some(cached.invite_cseq.as_str()) => {
            InDialogInvite::RetransmittedOriginal
        }
        _ => InDialogInvite::ReInvite,
    }
}

/// The dialog state needed to send an in-dialog request (a `BYE`) on a call we
/// answered as a UAS. See `sip_client::ByeRequest` for the role reversal.
pub(super) struct DialogInfo {
    /// The caller's `Contact` URI — where in-dialog requests must be sent.
    pub(super) remote_target: String,
    /// `Record-Route` from the INVITE, reversed.
    pub(super) route_headers: Vec<String>,
    /// Our `From` on outgoing in-dialog requests: the INVITE's `To` plus our tag.
    pub(super) from: String,
    /// Our `To`: the INVITE's `From`, tag included.
    pub(super) to: String,
    pub(super) local_addr: SocketAddr,
    pub(super) use_tcp: bool,
    /// Our own CSeq counter for this dialog. We answered the INVITE, so the
    /// caller's CSeq space is theirs; ours starts fresh.
    pub(super) cseq: u32,
}

impl DialogInfo {
    pub(super) fn from_invite(
        invite: &SipRequest,
        to_tag: &str,
        session: &crate::ims::RegisteredSession,
    ) -> Self {
        // Fall back to the Request-URI if the caller sent no Contact — a BYE to
        // the wrong target is still better than never hanging up at all.
        let remote_target = invite
            .header("Contact")
            .and_then(|c| {
                let start = c.find('<')? + 1;
                let end = c[start..].find('>')? + start;
                Some(c[start..end].to_string())
            })
            .unwrap_or_else(|| invite.request_uri.clone());

        let route_headers: Vec<String> = invite
            .headers_all("Record-Route")
            .iter()
            .rev()
            .map(|v| format!("Route: {v}"))
            .collect();

        let from = match invite.header("To") {
            Some(to) if to.contains(";tag=") => to.to_string(),
            Some(to) => format!("{to};tag={to_tag}"),
            None => format!("<sip:{}>;tag={to_tag}", session.public_uri),
        };
        let to = invite.header("From").unwrap_or_default().to_string();

        Self {
            remote_target,
            route_headers,
            from,
            to,
            local_addr: session.local_addr,
            use_tcp: session.use_tcp,
            cseq: 1,
        }
    }

    /// The UAC-role counterpart to [`from_invite`](Self::from_invite) —
    /// specs/025-outbound-calling, research.md R-010: we *sent* the INVITE
    /// this dialog started from, so unlike `from_invite`, `from`/`to` come
    /// from what we sent/received rather than the reverse, and `route_headers`
    /// reuses the same Service-Route set the INVITE itself was routed with
    /// (the same simplification `ims::call::run_call` already makes for its
    /// own BYE, rather than recomputing a dialog route set from
    /// `Record-Route` — `SipResponse` does not even expose repeated headers
    /// the way `SipRequest::headers_all` does, since nothing needed it before
    /// this).
    pub(super) fn from_uac_response(
        resp: &crate::ims::sip_client::SipResponse,
        route_headers: Vec<String>,
        callee_uri: &str,
        public_uri: &str,
        from_tag: &str,
        next_cseq: u32,
        session: &crate::ims::RegisteredSession,
    ) -> Self {
        // The far end's Contact is where in-dialog requests belong (RFC 3261
        // §12.1.2); no Contact on the 200 OK is malformed but not fatal — the
        // original callee URI is still a request the network already proved
        // it could route once.
        let remote_target = resp
            .header("Contact")
            .and_then(|c| {
                let start = c.find('<')? + 1;
                let end = c[start..].find('>')? + start;
                Some(c[start..end].to_string())
            })
            // `callee_uri` is a bare `user@host`, so the scheme has to go back
            // on: a `Contact`-less response produced `BYE +91...@ims... SIP/2.0`
            // with no `sip:` at all, which Jio refused
            // `400 Bad Request - P - 16004` (observed 2026-08-15, when the
            // dialog was mistakenly built from a PRACK's response — those carry
            // no Contact).
            .unwrap_or_else(|| format!("sip:{callee_uri}"));

        let to = resp
            .header("To")
            .map(str::to_string)
            .unwrap_or_else(|| format!("<sip:{callee_uri}>"));
        let from = format!("<sip:{public_uri}>;tag={from_tag}");

        Self {
            remote_target,
            route_headers,
            from,
            to,
            local_addr: session.local_addr,
            use_tcp: session.use_tcp,
            cseq: next_cseq,
        }
    }

    /// The `BYE` that ends this dialog. Every teardown path builds it the same
    /// way, with a fresh branch — it was written out four times before.
    pub(super) fn build_bye_for(&self, call_id: &str) -> String {
        build_bye(&ByeRequest {
            request_uri: &self.remote_target,
            route_headers: &self.route_headers,
            via_transport: if self.use_tcp { "TCP" } else { "UDP" },
            local_addr: self.local_addr,
            from: &self.from,
            to: &self.to,
            call_id,
            cseq: self.cseq,
            branch: &format!("z9hG4bK{}", random_hex(6)),
        })
    }
}

/// Reads Agent B's control messages on a thread, so the caller can wait on
/// them with a timeout while also servicing the carrier's SIP signaling —
/// without a partially-read line ever corrupting the newline-JSON framing,
/// which is what polling the socket with a read timeout would risk.
pub(super) fn spawn_control_reader(stream: TcpStream) -> mpsc::Receiver<ControlMessage> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        loop {
            match read_msg(&mut reader) {
                Ok(msg) => {
                    if tx.send(msg).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "Agent B control connection reader stopped");
                    return;
                }
            }
        }
    });
    rx
}

/// Reports an answered call ending — `CallCompleted{Answered}`, the history
/// row, and `active_calls` back to 0 — shared by every path that can end an
/// `ActiveCall` (carrier `BYE`, PBX-originated `CallEnded`, Agent B's
/// control connection dropping mid-call).
pub(super) fn report_answered_call_ended(
    obs: &observability::AgentObservability,
    call: &ActiveCall,
) {
    let verdict = call
        .meter
        .verdict(crate::ims::media_stats::DEFAULT_ONE_WAY_THRESHOLD_PERCENT);
    tracing::info!(
        call_id = %call.call_id,
        media = verdict.as_str(),
        carrier_rx = call.meter.carrier_rx(),
        pbx_rx = call.meter.pbx_rx(),
        // The lifecycle model's own account of the call: who ended it and the
        // status it derives from the same media verdict. Logged so the model
        // that drives admission and teardown is auditable against the metric
        // reported just below (`ims::lifecycle`).
        ended_by = call.lifecycle.ended_by.map(|e| e.as_str()).unwrap_or("unknown"),
        outcome = call.lifecycle.call_status(verdict.is_success()).as_str(),
        "call media verdict"
    );
    if !verdict.is_success() {
        tracing::warn!(
            call_id = %call.call_id,
            media = verdict.as_str(),
            "answered call did not carry audio both ways: {}",
            verdict.diagnosis()
        );
    }
    obs.report_call_answered_and_ended(
        &call.caller,
        call.answered_at,
        call.answered_instant.elapsed().as_secs_f64(),
        verdict,
    );
    obs.set_active_calls(0);
}

/// Watches a call's carrier leg for a genuinely lost attachment (FR-011).
///
/// The signal is two-stage on purpose. Downlink packets stalling is cheap to
/// notice and happens first, but on its own it cannot tell a dropped attachment
/// from a caller who simply went quiet. So a stall only *arms* the check; the
/// authoritative answer — "is the modem still attached?" — is asked over the AT
/// port, and only after a stall has persisted, so a healthy call never touches
/// the modem at all. Loss is declared only after it is confirmed more than once,
/// so a single glitched read cannot tear down a live call.
#[derive(Default)]
pub(super) struct AttachmentWatch {
    carrier_rx_mark: u64,
    media_stalled_since: Option<Instant>,
    last_probe: Option<Instant>,
    down_count: u32,
}

impl AttachmentWatch {
    /// Feeds the current downlink packet count and, once the carrier leg has
    /// been silent long enough, probes `check`. Returns `true` only when the
    /// attachment is confirmed lost.
    pub(super) fn attachment_lost(&mut self, carrier_rx: u64, check: &AttachmentHook) -> bool {
        if carrier_rx > self.carrier_rx_mark {
            // Audio is still arriving from the carrier — healthy; reset.
            self.carrier_rx_mark = carrier_rx;
            self.media_stalled_since = None;
            self.last_probe = None;
            self.down_count = 0;
            return false;
        }
        // The carrier leg is silent. Wait out the stall window before spending
        // an AT round-trip on it.
        let stalled_since = *self.media_stalled_since.get_or_insert_with(Instant::now);
        if stalled_since.elapsed() < MEDIA_STALL_BEFORE_ATTACHMENT_CHECK {
            return false;
        }
        if let Some(last) = self.last_probe {
            if last.elapsed() < ATTACHMENT_PROBE_INTERVAL {
                return false;
            }
        }
        self.last_probe = Some(Instant::now());
        if check() {
            // Attached: the silence is the caller, not a lost attachment. Rearm
            // the stall window rather than re-probing on every tick.
            self.media_stalled_since = Some(Instant::now());
            self.down_count = 0;
            false
        } else {
            self.down_count += 1;
            self.down_count >= ATTACHMENT_LOSS_CONFIRMATIONS
        }
    }
}

/// Ends a call because the network attachment was lost mid-call (FR-011).
///
/// The same coordinated teardown as a carrier `BYE` — stop the relay, tell
/// Agent B over the control channel so it drops the PBX leg — plus a
/// best-effort `BYE` toward the carrier. That `BYE` will usually not arrive
/// (the attachment it would travel over is the thing that died), but sending it
/// costs nothing and closes the dialog on any path that survived.
pub(super) fn end_call_attachment_lost(
    session: &mut crate::ims::RegisteredSession,
    mut call: ActiveCall,
) {
    call.stop.store(true, Ordering::Relaxed);
    if let Err(e) = write_msg(
        &mut call.control,
        &ControlMessage::CallEnded {
            call_id: call.call_id.clone(),
            reason: reason::ATTACHMENT_LOST.to_string(),
        },
    ) {
        tracing::warn!(call_id = %call.call_id, error = %e, "failed to notify Agent B of the attachment-loss teardown");
    }
    let bye = call.dialog.build_bye_for(&call.call_id);
    let _ = session.transport_mut().and_then(|t| t.send(&bye));
    tracing::info!(call_id = %call.call_id, reason = reason::ATTACHMENT_LOST, "call ended");
}

/// Tells the carrier the call is over after the PBX side hangs up first.
///
/// The mirror image of `handle_bye` (which handles the carrier hanging up on
/// us); between them, a hangup from either end tears the whole bridge down.
/// The BYE goes out on the registered client transport, like every other
/// request we originate — it is routed by the dialog's route set, not by which
/// connection the INVITE happened to arrive on.
///
/// The client transport can die silently mid-call — a NAT or the P-CSCF
/// itself dropping an idle TCP connection, since no SIP traffic crosses this
/// leg for the whole call duration (media is a separate RTP path; see
/// `RegisteredSession::reconnect_transport`) — so the first `send` failing
/// does not mean the carrier leg is unreachable, only that this particular
/// socket is dead. One reconnect-and-retry recovers that case; if the retry
/// also fails, the carrier leg is left stuck up (rare: reconnect only fails
/// if the underlying network attachment itself is down, in which case the
/// carrier's own side will eventually time the call out).
pub(super) fn hangup_carrier(
    session: &mut crate::ims::RegisteredSession,
    inbound: &Inbound,
    call: ActiveCall,
    reason: &str,
) {
    call.stop.store(true, Ordering::Relaxed);
    let bye = call.dialog.build_bye_for(&call.call_id);
    match session.transport_mut().and_then(|t| t.send(&bye)) {
        Ok(()) => {
            tracing::info!(call_id = %call.call_id, reason, "PBX hung up; sent BYE to the carrier");
            return;
        }
        Err(e) => {
            tracing::warn!(call_id = %call.call_id, error = %e, "failed to BYE the carrier after a PBX hangup; reconnecting to retry");
        }
    }
    if let Err(e) = session.reconnect_transport() {
        tracing::warn!(call_id = %call.call_id, error = %e, "could not reconnect the carrier transport; carrier leg may be left up until the network times it out");
        return;
    }
    match session.transport_mut().and_then(|t| t.send(&bye)) {
        Ok(()) => {
            tracing::info!(call_id = %call.call_id, reason, "PBX hung up; sent BYE to the carrier after reconnecting");
        }
        Err(e) => {
            tracing::warn!(call_id = %call.call_id, error = %e, "failed to BYE the carrier even after reconnecting; carrier leg may be left up until the network times it out");
        }
    }
    if let Err(e) = crate::ims::session::restart_client_reader(session, inbound) {
        tracing::warn!(call_id = %call.call_id, error = %e, "failed to restart the Gm client reader after a mid-call transport reconnect");
    }
}

/// A minimal `ActiveCall` for `agent::mod`'s dialog-identity tests
/// (specs/042-dialog-transaction-identity) — only `call_id`, `to_tag` and
/// `dialog.to` (the caller's original `From`) are ever inspected by what's
/// under test; the rest exist only because the struct requires them.
#[cfg(test)]
pub(super) fn test_active_call(call_id: &str, to_tag: &str, caller_from: &str) -> ActiveCall {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let control = TcpStream::connect(addr).unwrap();
    let (_tx, ctrl_rx) = mpsc::channel();
    ActiveCall {
        control,
        ctrl_rx,
        stop: Arc::new(AtomicBool::new(false)),
        call_id: call_id.to_string(),
        to_tag: to_tag.to_string(),
        dialog: DialogInfo {
            remote_target: "sip:caller@example.net".to_string(),
            route_headers: Vec::new(),
            from: String::new(),
            to: caller_from.to_string(),
            local_addr: addr,
            use_tcp: true,
            cseq: 1,
        },
        caller: "+919000000000".to_string(),
        answered_at: Utc::now(),
        answered_instant: Instant::now(),
        meter: crate::ims::media_stats::MediaMeter::new(),
        lifecycle: BridgedCall::new(call_id.to_string(), "+919000000000".to_string(), None),
        answered_invite: None,
    }
}

pub(super) fn handle_bye(sink: &SipSink, req: &SipRequest, mut call: ActiveCall) {
    call.stop.store(true, Ordering::Relaxed);
    if let Err(e) = write_msg(
        &mut call.control,
        &ControlMessage::CallEnded {
            call_id: call.call_id.clone(),
            reason: reason::CALLER_HANGUP.to_string(),
        },
    ) {
        tracing::warn!(call_id = %call.call_id, error = %e, "failed to notify Agent B of hangup");
    }
    respond(sink, "200 OK (BYE)", &build_200_ok_bye(req, &call.to_tag));
    tracing::info!(call_id = %call.call_id, "call ended");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_with_flowing_audio_never_probes_the_attachment() {
        // The load-bearing safety property of FR-011's watch: while audio keeps
        // arriving from the carrier, it must never touch the modem — and so can
        // never mistake a healthy call for a dropped attachment. If this holds,
        // a live call cannot be torn down by the watch.
        let mut w = AttachmentWatch::default();
        let probed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probed_c = probed.clone();
        let check = move || {
            probed_c.store(true, Ordering::Relaxed);
            false // would report "down" — but must never be consulted here
        };
        for rx in 1..=1000 {
            assert!(
                !w.attachment_lost(rx, &check),
                "a call with flowing audio must never be declared lost"
            );
        }
        assert!(
            !probed.load(Ordering::Relaxed),
            "a healthy call must never probe the modem"
        );
    }

    #[test]
    fn a_call_that_never_carried_downlink_does_not_immediately_declare_loss() {
        // A brand-new call sits at carrier_rx=0 for its first ticks before media
        // ramps up; the watch must not fire during that window on the strength
        // of the stall alone — the stall only *arms* the modem probe, which has
        // not even been reached yet here.
        let mut w = AttachmentWatch::default();
        let check = || false;
        assert!(!w.attachment_lost(0, &check));
        assert!(!w.attachment_lost(0, &check));
    }

    fn invite_with_cseq(cseq: &str) -> SipRequest {
        let raw = format!(
            "INVITE sip:x SIP/2.0\r\nCall-ID: c\r\nCSeq: {cseq} INVITE\r\n\
             Content-Length: 0\r\n\r\n"
        );
        SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
    }

    fn cached_answer(invite_cseq: &str) -> CachedInviteAnswer {
        CachedInviteAnswer {
            invite_cseq: invite_cseq.to_string(),
            contact: "<sip:bridge@10.0.0.1>".to_string(),
            answer_sdp: "v=0".to_string(),
        }
    }

    #[test]
    fn classify_in_dialog_invite_recognizes_an_identical_cseq_as_a_retransmission() {
        let cached = cached_answer("1 INVITE");
        let req = invite_with_cseq("1");
        assert!(matches!(
            classify_in_dialog_invite(&req, Some(&cached)),
            InDialogInvite::RetransmittedOriginal
        ));
    }

    #[test]
    fn classify_in_dialog_invite_treats_a_higher_cseq_as_a_re_invite() {
        let cached = cached_answer("1 INVITE");
        let req = invite_with_cseq("2");
        assert!(matches!(
            classify_in_dialog_invite(&req, Some(&cached)),
            InDialogInvite::ReInvite
        ));
    }

    #[test]
    fn classify_in_dialog_invite_treats_no_cached_answer_as_a_re_invite() {
        // A call this side placed itself (UAC role) has no cached answer —
        // any INVITE later naming it must never be mistaken for a retransmission.
        let req = invite_with_cseq("1");
        assert!(matches!(
            classify_in_dialog_invite(&req, None),
            InDialogInvite::ReInvite
        ));
    }
}
