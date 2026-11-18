//! A carrier's in-dialog re-INVITE: a session refresh, a hold or a resume.
//!
//! RFC 4028 lets either refresh transport be used, and a carrier that refreshes
//! with re-INVITE rather than `UPDATE` sends one every half session interval.
//! This bridge used to decline every re-INVITE `488` — which a carrier treats
//! as a failed refresh. Measured on T2
//! (2026-11-15): `200 OK` to an outbound call set `Session-Expires:
//! 1800;refresher=uas`; 900 s in, the carrier sent a re-INVITE with
//! `Session-Expires: 1800;refresher=uac`; three were declined `488` about 33 s
//! apart, and the call was then torn down with `BYE` and `Reason:
//! Q.850;cause=113;text="Session Timer Expired"`, about 17 minutes in.
//!
//! That re-INVITE's SDP listed the carrier's full codec set (EVS, AMR-WB in
//! bandwidth-efficient framing at a new payload type, AMR) — none of it the
//! format the call was actually using — but kept the **same `o=` line** as the
//! carrier's answer at call setup. RFC 3264 §8: "If the version in the origin
//! line does not increment, the SDP MUST be identical to the SDP with that
//! version number. The answerer MUST be prepared to receive an offer that
//! contains SDP with a version that has not changed; this is effectively a
//! no-op. However, the answerer MUST generate a valid answer (which MAY be the
//! same as the previous SDP from the answerer ...)" — where the "previous SDP"
//! is the last one sent to the other party, offer or answer. The carrier's body
//! is not identical, so its offer is non-compliant; RFC 6337 §5.2.5 leaves the
//! handling of such an SDP to the implementation. This bridge takes the
//! unchanged version at its word: the same origin line, the same media address,
//! the same direction is a no-op refresh, answered `200 OK` with the SDP it
//! already sent, and the media path is never touched. Strictly, that answer
//! shares no format with the offer it answers (RFC 3264 §6); following the
//! offer instead would mean switching the carrier leg's payload type and AMR
//! framing mid-call in every media relay, for a refresh that the carrier itself
//! labels as no change.
//!
//! An **offerless** re-INVITE is answered the same way, except that the SDP in
//! the `200 OK` is now this bridge's offer (RFC 3261 §13.3.1.4) — the previous
//! SDP, unchanged and at the same version (RFC 3264 §8) — and the carrier's
//! answer arrives in the `ACK`.
//! That answer is checked: it must keep the carrier's media address and the
//! audio format the call is already using, because the media path does not
//! follow a change of either. The format is the payload type *and* what the
//! carrier declares for it, codec and AMR framing (`octet-align`): the number
//! alone does not show that the relay can still read the carrier's audio. Its
//! direction may change, as an answer's may, and the relay follows it (see
//! below). An answer that fails the check means the refresh failed
//! — there is no response to an `ACK` to decline it with — so the call is ended
//! with `BYE`, as RFC 3261 §13.2.2.4 has a UAC do with an unacceptable offer in
//! a `2xx`, rather than left with media neither side agreed on. An `ACK` that
//! brings no answer this bridge can read (no body, or one that is not SDP) is
//! a different case: nothing in it says the media changed, and the offer it
//! answers was the SDP already in force, so the call carries on with the media
//! in use. Ending a working call on a guess would be the worse mistake.
//!
//! An `ACK` that never arrives is different again. Every `200 OK` to a
//! re-INVITE, with an offer or without, is owed one, and RFC 3261 §14.2 has a
//! UAS that "never receives an ACK" end the dialog with `BYE`: the carrier
//! either never got the `200 OK` or has abandoned the request, and the refresh
//! may just have replaced or removed the session timer that would otherwise
//! have caught that. The wait is bounded by [`REFRESH_ACK_TIMEOUT`] and checked
//! from the dispatch loop, never as a blocking wait.
//!
//! # Hold, resume, and other modifications
//!
//! An offer with a **new** `o=` version is a modification (RFC 3264 §8). The
//! one carriers send on a call that is up is a hold or a resume (§8.4): the
//! far party is put on hold with `sendonly` (or `inactive`), and taken off it
//! with `sendrecv`. Measured on T2 (2026-11-18, an outbound VoWiFi call): the
//! SBC sent a hold at version 89 with `a=sendonly`, and the resume at 90
//! without a direction, both from the same address and port. Neither listed
//! the format the call was using: both were the SBC's full codec list, with
//! AMR-WB at payload type 102 **bandwidth-efficient**, where setup had
//! agreed 96 octet-aligned, and `telephone-event/16000` at 103 instead of
//! 105. Both were declined `488`; the call carried on unheld, and the
//! carrier's next refresh came back to 96 octet-aligned.
//!
//! Such an offer is now answered `200 OK` when this side can follow it:
//!
//! - the carrier's media address is unchanged, and its `o=` line names the
//!   same session, only at another version;
//! - the offer still lists the codec the call is running (AMR-WB, AMR or
//!   PCMU), under any payload type number and AMR framing: the first entry
//!   for it in the offer's order of preference that the relay can follow;
//! - the running media relay can carry on with those labels
//!   (`ims::carrier_leg`): the transcoding relay can follow any of them, the
//!   PCMU pass-through only a new `telephone-event` payload type, and an AMR
//!   call forwarded without transcoding (specs/120) none, so a modification
//!   on such a call is still declined.
//!
//! The answer is the next version of the SDP this side last sent (the same
//! `o=` line, its version plus one), naming that codec under the offer's
//! payload type and `fmtp`, the offer's `telephone-event` at the codec's
//! rate, and the answering direction: `recvonly` to `sendonly`, `inactive`
//! to `inactive`, `sendrecv` to `sendrecv` (RFC 3264 §6.1). Once the `200
//! OK` is sent, the relay sends with those labels (§8.3.2) and sends nothing
//! toward the carrier while this side's answer is `recvonly` or `inactive`.
//! What the carrier sends during a hold (music, an announcement) still
//! reaches the telephone leg. The codec never changes, so the relay is never
//! rebuilt, and Agent B is not told: the hold is the carrier leg's, and the
//! telephone leg stays as it was. The new SDPs on both sides become the
//! baseline the next refresh is compared against.
//!
//! An offer at the **same** version that changes the direction or the media
//! address contradicts itself (§8) and is still declined `488`, as is any
//! modification this side cannot follow. A declined re-INVITE leaves the
//! session as it was (RFC 3261 §14.1).

use super::registration_refresh;
use super::session_refresh::{self, PeerRefreshTerms, SessionIntervalTooSmall};
use crate::ims::carrier_leg::{CarrierLabels, CarrierLeg, CarrierLegState};
use crate::ims::sdp::{self, MediaDirection};
use crate::ims::sip_client::SipRequest;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long the `ACK` of a refresh re-INVITE this bridge answered `200 OK` may
/// take before the call is ended (RFC 3261 §14.2): 64×T1, §13.3.1.4's bound on
/// a `2xx` going unacknowledged, with the T1 of 2 s that TS 24.229 table 7.7.1
/// sets for IMS access. That is 128 s, the value `registration_refresh` and
/// `reg_subscription` use for their own 64×T1 timers. RFC 3261's default T1
/// would give 32 s, less than this access allows a transaction.
/// `inbound::OFFERLESS_ACK_TIMEOUT` is far shorter because it is a blocking
/// wait before a call exists; this is a deadline on a call that is up,
/// checked without blocking.
pub(super) const REFRESH_ACK_TIMEOUT: Duration = registration_refresh::TRANSACTION_TIMEOUT;

/// What a carrier SDP declares for one audio payload type, as far as the media
/// path depends on it: the codec, and for AMR whether it is framed
/// octet-aligned or bandwidth-efficient (RFC 4867 `octet-align`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DeclaredFormat {
    codec: sdp::NegotiatedCodec,
    octet_aligned: bool,
}

/// What `sdp` declares for `payload_type`: `None` when it does not list it
/// with a codec this bridge knows.
fn declared_format(sdp: &sdp::SdpOffer, payload_type: u8) -> Option<DeclaredFormat> {
    let entry = sdp
        .offered
        .iter()
        .find(|entry| entry.payload_type == payload_type)?;
    Some(DeclaredFormat {
        codec: entry.codec,
        octet_aligned: entry.is_octet_aligned(),
    })
}

/// The `ACK` still owed for a refresh re-INVITE answered `200 OK`.
#[derive(Debug, Clone, Copy)]
struct AwaitedAck {
    /// The re-INVITE's `CSeq` number, which its `ACK` repeats.
    cseq: u32,
    /// The re-INVITE was offerless, so the `ACK` carries the carrier's answer
    /// to the offer in the `200 OK`.
    brings_answer: bool,
    /// When the wait for it is over.
    deadline: Instant,
}

/// The session description both sides last agreed on — at setup, or at the
/// latest modification this bridge accepted — which a carrier re-INVITE is
/// compared against, and the running relay's handle on the carrier leg.
#[derive(Debug, Clone)]
pub(super) struct EstablishedMedia {
    /// The SDP this bridge last sent the carrier: its offer, on a call it
    /// placed; its answer, on one it answered (the offerless path's offer
    /// included), or to the latest modification it accepted. Resent verbatim
    /// — same `o=` version — on a refresh.
    local_sdp: String,
    /// The direction `local_sdp` states.
    local_direction: MediaDirection,
    /// The carrier's `o=` line from its latest SDP.
    remote_origin: String,
    /// The direction the carrier's latest SDP states.
    remote_direction: MediaDirection,
    /// Where the carrier's media for this call comes from.
    remote_rtp: SocketAddr,
    /// The carrier-leg audio payload type the media path is running with.
    payload_type: u8,
    /// What the carrier's SDP declared for that payload type, at setup or at
    /// the latest accepted modification. The baseline is the carrier's own
    /// declaration, not this bridge's reading of it, so an answer that
    /// repeats that SDP is always unchanged, and one that keeps the number
    /// but declares another codec or framing never is. `None` when that SDP
    /// declared nothing recognisable.
    remote_format: Option<DeclaredFormat>,
    /// The running media relay's carrier leg, through which an accepted
    /// modification relabels or holds it. `None` when the relay cannot
    /// follow one (an AMR call forwarded without transcoding, specs/120):
    /// every modification is then declined.
    carrier_leg: Option<Arc<CarrierLeg>>,
    /// The `ACK` of the latest refresh re-INVITE this bridge answered `200
    /// OK`, while it is still due.
    awaiting_ack: Option<AwaitedAck>,
}

/// What to do with a re-INVITE naming the active call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReinviteVerdict {
    /// The session description is unchanged: answer `200 OK` with the SDP
    /// already sent.
    Refresh,
    /// No offer: answer `200 OK` offering the SDP already sent, and check the
    /// carrier's answer when the `ACK` brings it.
    OfferlessRefresh,
    /// A modification this side can follow — a hold or a resume, possibly
    /// relabelling the running codec: answer `200 OK` with
    /// [`Modification::answer`], then [`EstablishedMedia::apply`] it.
    Modify(Modification),
    /// A modification this side cannot follow, or nothing to compare it
    /// with: decline `488`, as before. The string says why, for the log.
    Decline(&'static str),
}

/// An accepted modification: the answer to send, and the state both sides are
/// in once it is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Modification {
    answer: String,
    local_direction: MediaDirection,
    remote_origin: String,
    remote_direction: MediaDirection,
    remote_format: Option<DeclaredFormat>,
    leg: CarrierLegState,
}

impl Modification {
    /// The SDP answer for the `200 OK`.
    pub(super) fn answer(&self) -> &str {
        &self.answer
    }

    /// What the relay does on the carrier leg once the answer is sent.
    pub(super) fn leg(&self) -> CarrierLegState {
        self.leg
    }

    /// The direction the answer states.
    pub(super) fn local_direction(&self) -> MediaDirection {
        self.local_direction
    }
}

/// Whether `answer` is a direction an answer may state for a stream offered
/// with `offer` (RFC 3264 §6.1).
fn answers(offer: MediaDirection, answer: MediaDirection) -> bool {
    use MediaDirection::{Inactive, RecvOnly, SendOnly, SendRecv};
    match offer {
        SendRecv => true,
        SendOnly => matches!(answer, RecvOnly | Inactive),
        RecvOnly => matches!(answer, SendOnly | Inactive),
        Inactive => answer == Inactive,
    }
}

/// Whether two `o=` lines (already [`normalized`]) name the same session —
/// username, session id and address — whatever their versions (RFC 3264 §8:
/// a modification keeps the line "identical ... except that the version in
/// the origin field MUST increment").
fn same_session(a: &str, b: &str) -> bool {
    fn without_version(origin: &str) -> Option<[&str; 5]> {
        let fields: Vec<&str> = origin.split(' ').collect();
        (fields.len() == 6).then(|| [fields[0], fields[1], fields[3], fields[4], fields[5]])
    }
    without_version(a).is_some_and(|a| Some(a) == without_version(b))
}

impl EstablishedMedia {
    /// `local_sdp` is what this bridge sent, `remote_sdp` what the carrier
    /// sent, and `payload_type` the carrier-leg format the answer selected.
    /// `None` when the carrier's SDP has no `o=` line — then no later offer
    /// can be shown to be unchanged, and every re-INVITE stays declined.
    /// No modification can be followed until the relay's carrier leg is
    /// attached ([`Self::with_carrier_leg`]).
    pub(super) fn new(
        local_sdp: String,
        remote_sdp: &str,
        remote_rtp: SocketAddr,
        payload_type: u8,
    ) -> Option<Self> {
        let remote = sdp::parse_offer(remote_sdp).ok();
        Some(Self {
            remote_origin: normalized(sdp::origin_line(remote_sdp)?),
            remote_direction: remote.as_ref().map(|r| r.direction).unwrap_or_default(),
            remote_format: remote
                .as_ref()
                .and_then(|parsed| declared_format(parsed, payload_type)),
            local_direction: sdp::parse_offer(&local_sdp)
                .map(|ours| ours.direction)
                .unwrap_or_default(),
            local_sdp,
            remote_rtp,
            payload_type,
            carrier_leg: None,
            awaiting_ack: None,
        })
    }

    /// The running relay's carrier leg, so that a modification can be
    /// followed.
    pub(super) fn with_carrier_leg(mut self, leg: Arc<CarrierLeg>) -> Self {
        self.carrier_leg = Some(leg);
        self
    }

    pub(super) fn local_sdp(&self) -> &str {
        &self.local_sdp
    }

    /// What to do with a re-INVITE whose SDP is `offer` (`None` when it
    /// carried none).
    pub(super) fn classify(&self, offer: Option<&str>) -> ReinviteVerdict {
        let Some(offer) = offer else {
            return ReinviteVerdict::OfferlessRefresh;
        };
        let origin = sdp::origin_line(offer).map(normalized);
        if origin.as_deref() == Some(self.remote_origin.as_str()) {
            return match self.check_unchanged(offer) {
                Ok(()) => ReinviteVerdict::Refresh,
                Err(why) => ReinviteVerdict::Decline(why),
            };
        }
        match self.plan_modification(origin, offer) {
            Ok(modification) => ReinviteVerdict::Modify(modification),
            Err(why) => ReinviteVerdict::Decline(why),
        }
    }

    /// The carrier's media address and direction in an offer at the same
    /// version are the ones the call is running with.
    fn check_unchanged(&self, offer: &str) -> Result<(), &'static str> {
        let parsed = self.check_address(offer)?;
        if parsed.direction != self.remote_direction {
            return Err("the SDP changes the media direction but not its o= version");
        }
        Ok(())
    }

    /// `body` parses, and keeps the carrier's media address.
    fn check_address(&self, body: &str) -> Result<sdp::SdpOffer, &'static str> {
        let parsed = sdp::parse_offer(body).map_err(|_| "the SDP could not be parsed")?;
        if parsed.remote_rtp != self.remote_rtp {
            return Err("the SDP moves the carrier's media address");
        }
        Ok(parsed)
    }

    /// The answer to `offer`, a new version of the carrier's SDP (its origin
    /// line, normalized, is `origin`), if this side can follow it. See the
    /// module documentation for what that takes.
    fn plan_modification(
        &self,
        origin: Option<String>,
        offer: &str,
    ) -> Result<Modification, &'static str> {
        let Some(leg) = &self.carrier_leg else {
            return Err("the call's media relay cannot follow a session modification");
        };
        let origin = origin.ok_or("the offer has no o= line")?;
        if !same_session(&origin, &self.remote_origin) {
            return Err("the offer's o= line names another session");
        }
        let parsed = self.check_address(offer)?;
        if sdp::precondition_verdict(&parsed) == sdp::PreconditionVerdict::Decline {
            return Err("the offer's QoS preconditions cannot be confirmed");
        }
        let running = leg.state().labels;
        // The carrier's preference wins (RFC 3264 §5.1: an offer lists its
        // formats most preferred first): of the entries for the running
        // codec, the first one the relay can carry on with. On T2's resume
        // (2026-11-21) that is 96 octet-aligned, the format setup agreed,
        // listed before the 112 bandwidth-efficient one.
        let mut of_codec = parsed
            .offered
            .iter()
            .filter(|e| e.codec == leg.codec())
            .peekable();
        if of_codec.peek().is_none() {
            return Err("the offer no longer lists the codec the call is running");
        }
        let entry = of_codec
            .find(|e| {
                leg.can_follow(CarrierLabels {
                    payload_type: e.payload_type,
                    octet_aligned: e.is_octet_aligned(),
                    ..running
                })
            })
            .ok_or("the call's media relay cannot follow the offer's payload type or framing")?;
        let (answer, chosen) = sdp::build_reanswer(&self.local_sdp, entry, &parsed)
            .map_err(|_| "this side's previous SDP cannot be given a new version")?;
        let labels = CarrierLabels::of(&chosen);
        if !leg.can_follow(labels) {
            return Err("the call's media relay cannot follow the offer's payload type or framing");
        }
        let local_direction = parsed.direction.answered();
        // The audio number the carrier may still be sending under: the one
        // this relabel leaves, or else the one an earlier relabel left. It
        // stays readable as that audio only while this offer gives the number
        // no other meaning (see `CarrierLegState::previous_audio`).
        let previous_audio = if labels.payload_type == running.payload_type {
            leg.state().previous_audio
        } else {
            Some((running.payload_type, running.octet_aligned))
        }
        .filter(|&(payload_type, octet_aligned)| {
            !sdp::audio_payload_types(offer).contains(&payload_type)
                || declared_format(&parsed, payload_type)
                    == Some(DeclaredFormat {
                        codec: leg.codec(),
                        octet_aligned,
                    })
        });
        Ok(Modification {
            answer,
            local_direction,
            remote_origin: origin,
            remote_direction: parsed.direction,
            remote_format: declared_format(&parsed, chosen.payload_type),
            leg: CarrierLegState {
                labels,
                sending: local_direction.sends() && parsed.direction.receives(),
                previous_audio,
            },
        })
    }

    /// Makes `modification`, whose answer has just been sent, the session
    /// description in force: the relay switches to its labels and direction,
    /// and the next re-INVITE is compared against both new SDPs.
    pub(super) fn apply(&mut self, modification: &Modification) {
        if let Some(leg) = &self.carrier_leg {
            if !leg.set(modification.leg) {
                // `plan_modification` checked this against the same, unchanged
                // leg, so it cannot happen; said loudly if it ever does.
                tracing::error!(
                    labels = ?modification.leg.labels,
                    "the media relay refused the labels of a modification already answered"
                );
            }
        }
        self.local_sdp.clone_from(&modification.answer);
        self.local_direction = modification.local_direction;
        self.remote_origin.clone_from(&modification.remote_origin);
        self.remote_direction = modification.remote_direction;
        self.payload_type = modification.leg.labels.payload_type;
        self.remote_format = modification.remote_format;
    }

    /// Record that a refresh re-INVITE with `CSeq` number `cseq` was answered
    /// `200 OK` at `now`: its `ACK` is due within [`REFRESH_ACK_TIMEOUT`], and
    /// `brings_answer` when the re-INVITE was offerless. A later re-INVITE
    /// replaces it; a retransmission of the same one does not extend the
    /// wait. A request with no readable `CSeq` has no `ACK` that could be
    /// matched to it, so none is awaited.
    pub(super) fn await_ack(&mut self, cseq: Option<u32>, brings_answer: bool, now: Instant) {
        let Some(cseq) = cseq else {
            self.awaiting_ack = None;
            return;
        };
        if self.awaiting_ack.is_some_and(|ack| ack.cseq == cseq) {
            return;
        }
        self.awaiting_ack = Some(AwaitedAck {
            cseq,
            brings_answer,
            deadline: now + REFRESH_ACK_TIMEOUT,
        });
    }

    /// Settles the awaited `ACK` if `cseq` is its `CSeq` number: `Some` with
    /// whether it brings the carrier's answer (an offerless refresh), `None`
    /// for any other `ACK`.
    pub(super) fn take_ack(&mut self, cseq: Option<u32>) -> Option<bool> {
        let ack = self.awaiting_ack.filter(|ack| Some(ack.cseq) == cseq)?;
        self.awaiting_ack = None;
        Some(ack.brings_answer)
    }

    /// Whether the `ACK` of a refresh re-INVITE answered `200 OK` is still
    /// missing at `now`, past its deadline.
    pub(super) fn ack_overdue(&self, now: Instant) -> bool {
        self.awaiting_ack.is_some_and(|ack| now >= ack.deadline)
    }

    /// Takes the carrier's `answer` from the `ACK` completing an offerless
    /// refresh. `Ok` when the call's media carries on as it is (and the
    /// carrier's origin is updated to the answer's); `Err` with why when the
    /// answer cannot be followed.
    ///
    /// The payload type number alone is not the format: a carrier that keeps
    /// the number but drops or flips `octet-align`, or maps it to another
    /// codec, sends audio the relay can no longer read (code review finding,
    /// 2026-11-16). So what the answer declares for the payload type must
    /// equal what the carrier's SDP declared for it at setup.
    ///
    /// The direction may change, as any answer's may (RFC 3264 §6.1): a
    /// carrier answering this side's `sendrecv` with `sendonly` or
    /// `inactive` puts the call on hold, and one answering a held call's
    /// `recvonly` with `inactive` rather than `sendonly` keeps it held. The
    /// relay follows it when it can; one that cannot (`carrier_leg` is
    /// `None`) fails the refresh, as every changed direction did before.
    pub(super) fn take_answer(&mut self, answer: &str) -> Result<(), &'static str> {
        let parsed = self.check_address(answer)?;
        if sdp::first_audio_payload_type(answer) != Some(self.payload_type) {
            return Err("the answer selects a different payload type");
        }
        if declared_format(&parsed, self.payload_type) != self.remote_format {
            return Err("the answer changes the codec or AMR framing of the payload type in use");
        }
        if parsed.direction != self.remote_direction {
            if !answers(self.local_direction, parsed.direction) {
                return Err("the answer's media direction does not answer the offer's");
            }
            let Some(leg) = &self.carrier_leg else {
                return Err(
                    "the answer changes the media direction, which the relay cannot follow",
                );
            };
            leg.set(CarrierLegState {
                sending: self.local_direction.sends() && parsed.direction.receives(),
                ..leg.state()
            });
            self.remote_direction = parsed.direction;
        }
        if let Some(origin) = sdp::origin_line(answer) {
            self.remote_origin = normalized(origin);
        }
        Ok(())
    }
}

/// An `o=` line with runs of whitespace collapsed, so a re-serialisation that
/// only changes spacing still compares equal.
fn normalized(origin: &str) -> String {
    origin.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The number in a request's `CSeq` header (`"1 INVITE"` → `1`).
pub(super) fn cseq_number(req: &SipRequest) -> Option<u32> {
    req.header("CSeq")?.split_whitespace().next()?.parse().ok()
}

/// The extra headers the session-timer terms put on a `200 OK`.
pub(super) type TimerHeaders = Vec<(&'static str, String)>;

/// The `Session-Expires` terms for answering a refresh re-INVITE `req`, and
/// the extra headers they put on the `200 OK`. `Err` when the request asks for
/// an interval below the minimum, which is refused `422` instead.
pub(super) fn refresh_terms(
    req: &SipRequest,
    now: Instant,
) -> Result<(PeerRefreshTerms, TimerHeaders), SessionIntervalTooSmall> {
    let carrier_supports_timer = req
        .headers_all("Supported")
        .iter()
        .flat_map(|value| value.split(','))
        .any(|tag| tag.trim().eq_ignore_ascii_case("timer"));
    let terms = session_refresh::terms_for_peer_refresh(
        req.header("Session-Expires"),
        carrier_supports_timer,
        now,
    )?;
    let mut headers = Vec::new();
    if let Some(value) = &terms.session_expires {
        headers.push(("Session-Expires", value.clone()));
    }
    if terms.require_timer {
        headers.push(("Require", "timer".to_string()));
    }
    Ok((terms, headers))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::ims::carrier_leg::Relabel;

    /// The carrier's answer at setup, shaped like T2's (addresses are
    /// documentation ranges).
    pub(in crate::ims::agent) const SETUP_ANSWER: &str = "v=0\r\n\
        o=- 71848567 71848567 IN IP4 198.51.100.36\r\n\
        s=SBC call\r\n\
        c=IN IP4 198.51.100.36\r\n\
        t=0 0\r\n\
        m=audio 14592 RTP/AVP 96\r\n\
        a=rtpmap:96 AMR-WB/16000\r\n\
        a=fmtp:96 octet-align=1\r\n\
        a=ptime:20\r\n";

    /// The refresh re-INVITE's offer, shaped like T2's: the full codec list,
    /// none of it the format in use, but the same `o=` line.
    pub(in crate::ims::agent) const REFRESH_OFFER: &str = "v=0\r\n\
        o=- 71848567 71848567 IN IP4 198.51.100.36\r\n\
        s=SBC call\r\n\
        c=IN IP4 198.51.100.36\r\n\
        t=0 0\r\n\
        m=audio 14592 RTP/AVP 97 106 102 101 103\r\n\
        a=rtpmap:97 telephone-event/8000\r\n\
        a=ptime:20\r\n\
        a=rtpmap:106 EVS/16000\r\n\
        a=fmtp:106 br=9.6-128;bw=swb;mode-change-capability=2;cmr=0;dtx=1;ch-aw-recv=3;max-red=0\r\n\
        a=rtpmap:102 AMR-WB/16000\r\n\
        a=fmtp:102 mode-change-capability=2;max-red=0\r\n\
        a=rtpmap:101 AMR/8000\r\n\
        a=fmtp:101 mode-change-capability=2;max-red=0\r\n\
        a=rtpmap:103 telephone-event/16000\r\n\
        a=fmtp:103 0-15\r\n";

    pub(in crate::ims::agent) const OUR_OFFER: &str = "v=0\r\n\
        o=- 2357052722 2357052722 IN IP4 192.0.2.67\r\n\
        s=gsm-sip-bridge test call\r\n\
        c=IN IP4 192.0.2.67\r\n\
        t=0 0\r\n\
        m=audio 45772 RTP/AVP 96 0\r\n\
        a=rtpmap:96 AMR-WB/16000\r\n\
        a=fmtp:96 octet-align=1\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=sendrecv\r\n";

    pub(in crate::ims::agent) fn established() -> EstablishedMedia {
        EstablishedMedia::new(
            OUR_OFFER.to_string(),
            SETUP_ANSWER,
            "198.51.100.36:14592".parse().unwrap(),
            96,
        )
        .unwrap()
    }

    #[test]
    fn the_live_refresh_offer_is_a_refresh() {
        assert_eq!(
            established().classify(Some(REFRESH_OFFER)),
            ReinviteVerdict::Refresh
        );
    }

    #[test]
    fn whitespace_alone_does_not_change_the_origin() {
        let offer = REFRESH_OFFER.replace("o=- 71848567", "o=-  71848567");
        assert_eq!(
            established().classify(Some(&offer)),
            ReinviteVerdict::Refresh
        );
    }

    /// A call whose relay cannot follow a modification (an AMR call
    /// forwarded without transcoding has no carrier leg handle) declines
    /// every new version, as every call did before.
    #[test]
    fn a_new_sdp_version_is_declined_when_the_relay_cannot_follow_it() {
        let offer = REFRESH_OFFER.replace("71848567 71848567", "71848567 71848568");
        assert_eq!(
            established().classify(Some(&offer)),
            ReinviteVerdict::Decline("the call's media relay cannot follow a session modification")
        );
    }

    #[test]
    fn a_moved_media_address_is_a_modification_even_at_the_same_version() {
        let offer = REFRESH_OFFER.replace("m=audio 14592", "m=audio 14600");
        assert!(matches!(
            established().classify(Some(&offer)),
            ReinviteVerdict::Decline(_)
        ));
    }

    #[test]
    fn a_hold_is_a_modification_even_at_the_same_version() {
        for direction in ["sendonly", "recvonly", "inactive"] {
            let offer = format!("{REFRESH_OFFER}a={direction}\r\n");
            assert!(
                matches!(
                    established().classify(Some(&offer)),
                    ReinviteVerdict::Decline(_)
                ),
                "{direction}"
            );
        }
    }

    #[test]
    fn an_offerless_re_invite_is_answered_with_an_offer() {
        assert_eq!(
            established().classify(None),
            ReinviteVerdict::OfferlessRefresh
        );
    }

    #[test]
    fn only_the_awaited_ack_settles_a_refresh_and_only_once() {
        let now = Instant::now();
        let mut media = established();
        assert_eq!(media.take_ack(Some(2)), None, "nothing awaited yet");
        media.await_ack(Some(2), true, now);
        assert_eq!(media.take_ack(Some(3)), None);
        assert_eq!(media.take_ack(None), None);
        assert_eq!(media.take_ack(Some(2)), Some(true));
        assert_eq!(media.take_ack(Some(2)), None, "settled once");
        // A refresh with an offer is owed an ACK too, with no answer in it.
        media.await_ack(Some(3), false, now);
        assert_eq!(media.take_ack(Some(3)), Some(false));
    }

    /// Code review finding, 2026-11-16: the wait for the ACK had no end.
    #[test]
    fn a_missing_ack_is_overdue_after_the_bounded_wait() {
        let now = Instant::now();
        // 64×T1 with the IMS T1 of 2 s (TS 24.229 table 7.7.1).
        assert_eq!(REFRESH_ACK_TIMEOUT, Duration::from_secs(128));
        let just_before = now + REFRESH_ACK_TIMEOUT - Duration::from_millis(1);
        for brings_answer in [true, false] {
            let mut media = established();
            assert!(!media.ack_overdue(now + REFRESH_ACK_TIMEOUT), "none due");
            media.await_ack(Some(2), brings_answer, now);
            assert!(!media.ack_overdue(just_before));
            assert!(media.ack_overdue(now + REFRESH_ACK_TIMEOUT));
            // The ACK arriving settles it: nothing is overdue afterwards.
            assert_eq!(media.take_ack(Some(2)), Some(brings_answer));
            assert!(!media.ack_overdue(now + REFRESH_ACK_TIMEOUT));
        }
    }

    #[test]
    fn a_retransmitted_re_invite_does_not_extend_the_wait_but_a_new_one_restarts_it() {
        let now = Instant::now();
        let later = now + Duration::from_secs(20);
        let mut media = established();
        media.await_ack(Some(2), true, now);

        media.await_ack(Some(2), true, later);
        assert!(media.ack_overdue(now + REFRESH_ACK_TIMEOUT));

        media.await_ack(Some(3), false, later);
        assert!(!media.ack_overdue(now + REFRESH_ACK_TIMEOUT));
        assert!(media.ack_overdue(later + REFRESH_ACK_TIMEOUT));
        assert_eq!(media.take_ack(Some(2)), None, "superseded");
    }

    #[test]
    fn a_re_invite_with_no_readable_cseq_awaits_no_ack() {
        let now = Instant::now();
        let mut media = established();
        media.await_ack(Some(2), true, now);
        media.await_ack(None, true, now);
        assert!(!media.ack_overdue(now + REFRESH_ACK_TIMEOUT));
    }

    #[test]
    fn an_ack_answer_keeping_the_media_completes_the_refresh() {
        let mut media = established();
        assert_eq!(media.take_answer(SETUP_ANSWER), Ok(()));
    }

    #[test]
    fn a_new_origin_in_an_acceptable_answer_becomes_the_baseline() {
        let mut media = established();
        let answer = SETUP_ANSWER.replace("71848567 71848567", "71848567 71848568");
        assert_eq!(media.take_answer(&answer), Ok(()));
        // The next refresh offer is judged against the answer's origin.
        let offer = REFRESH_OFFER.replace("71848567 71848567", "71848567 71848568");
        assert_eq!(media.classify(Some(&offer)), ReinviteVerdict::Refresh);
    }

    #[test]
    fn an_ack_answer_the_media_path_cannot_follow_fails_the_refresh() {
        let other_payload_type = SETUP_ANSWER
            .replace("RTP/AVP 96", "RTP/AVP 0")
            .replace("a=rtpmap:96 AMR-WB/16000", "a=rtpmap:0 PCMU/8000");
        let moved = SETUP_ANSWER.replace("m=audio 14592", "m=audio 14600");
        let held = format!("{SETUP_ANSWER}a=inactive\r\n");
        for answer in [other_payload_type.as_str(), moved.as_str(), held.as_str()] {
            let mut media = established();
            assert!(media.take_answer(answer).is_err(), "{answer:?}");
        }
    }

    /// Code review finding, 2026-11-16: the payload type number was all that
    /// was compared, so an answer that kept PT 96 and changed what it means
    /// was accepted, and the relay went on with framing the carrier had left.
    #[test]
    fn an_ack_answer_keeping_the_payload_type_but_not_its_format_fails_the_refresh() {
        let no_framing = SETUP_ANSWER.replace("a=fmtp:96 octet-align=1\r\n", "");
        let bandwidth_efficient = SETUP_ANSWER.replace("octet-align=1", "octet-align=0");
        let other_fmtp = SETUP_ANSWER.replace("octet-align=1", "mode-change-capability=2");
        let narrowband = SETUP_ANSWER.replace("AMR-WB/16000", "AMR/8000");
        let unknown_codec = SETUP_ANSWER.replace("AMR-WB/16000", "EVS/16000");
        for answer in [
            &no_framing,
            &bandwidth_efficient,
            &other_fmtp,
            &narrowband,
            &unknown_codec,
        ] {
            assert_eq!(sdp::first_audio_payload_type(answer), Some(96));
            assert_eq!(
                established().take_answer(answer),
                Err("the answer changes the codec or AMR framing of the payload type in use"),
                "{answer}"
            );
        }
    }

    #[test]
    fn an_ack_answer_restating_the_same_format_differently_completes_the_refresh() {
        let more_parameters =
            SETUP_ANSWER.replace("octet-align=1", "mode-change-capability=2; octet-align=1");
        let lower_case = SETUP_ANSWER.replace("AMR-WB/16000", "amr-wb/16000");
        for answer in [&more_parameters, &lower_case] {
            assert_eq!(established().take_answer(answer), Ok(()), "{answer}");
        }
    }

    /// The baseline is what the carrier declared at setup, not what this
    /// bridge would have liked it to declare: a carrier that never stated the
    /// framing and still does not has changed nothing.
    #[test]
    fn an_ack_answer_repeating_a_setup_sdp_that_declared_no_framing_is_unchanged() {
        let no_framing = SETUP_ANSWER.replace("a=fmtp:96 octet-align=1\r\n", "");
        let no_rtpmap = no_framing.replace("a=rtpmap:96 AMR-WB/16000\r\n", "");
        for setup in [&no_framing, &no_rtpmap] {
            let established = || {
                EstablishedMedia::new(
                    OUR_OFFER.to_string(),
                    setup,
                    "198.51.100.36:14592".parse().unwrap(),
                    96,
                )
                .unwrap()
            };
            assert_eq!(established().take_answer(setup), Ok(()), "{setup}");
            // Starting to state a framing mid-call is a change like any other.
            assert!(established().take_answer(SETUP_ANSWER).is_err(), "{setup}");
        }
    }

    /// On a call this bridge answered, the carrier's setup SDP is its offer,
    /// which lists several payload types: the baseline is the one in use.
    #[test]
    fn the_format_baseline_is_the_setup_offers_entry_for_the_payload_type_in_use() {
        let setup_offer = "v=0\r\n\
            o=- 71848567 71848567 IN IP4 198.51.100.36\r\n\
            s=SBC call\r\n\
            c=IN IP4 198.51.100.36\r\n\
            t=0 0\r\n\
            m=audio 14592 RTP/AVP 102 96\r\n\
            a=rtpmap:102 AMR-WB/16000\r\n\
            a=fmtp:102 mode-change-capability=2\r\n\
            a=rtpmap:96 AMR-WB/16000\r\n\
            a=fmtp:96 octet-align=1\r\n";
        let mut media = EstablishedMedia::new(
            OUR_OFFER.to_string(),
            setup_offer,
            "198.51.100.36:14592".parse().unwrap(),
            96,
        )
        .unwrap();
        assert_eq!(media.take_answer(SETUP_ANSWER), Ok(()));
        let bandwidth_efficient = SETUP_ANSWER.replace("octet-align=1", "octet-align=0");
        assert!(media.take_answer(&bandwidth_efficient).is_err());
    }

    #[test]
    fn a_carrier_sdp_without_an_origin_records_nothing_to_compare() {
        let no_origin = SETUP_ANSWER.replace("o=- 71848567 71848567 IN IP4 198.51.100.36\r\n", "");
        assert!(EstablishedMedia::new(
            OUR_OFFER.to_string(),
            &no_origin,
            "198.51.100.36:14592".parse().unwrap(),
            96,
        )
        .is_none());
    }

    // ---- Hold and resume, shaped like T2's on 2026-11-18 ----

    /// This bridge's offer on the live outbound call (synthetic session id,
    /// documentation address).
    pub(in crate::ims::agent) const LIVE_OUR_OFFER: &str = "v=0\r\n\
        o=- 2357052722 2357052722 IN IP4 192.0.2.67\r\n\
        s=gsm-sip-bridge test call\r\n\
        c=IN IP4 192.0.2.67\r\n\
        t=0 0\r\n\
        m=audio 36185 RTP/AVP 96 0 105 100\r\n\
        a=rtpmap:96 AMR-WB/16000\r\n\
        a=fmtp:96 octet-align=1\r\n\
        a=rtpmap:0 PCMU/8000\r\n\
        a=rtpmap:105 telephone-event/16000\r\n\
        a=fmtp:105 0-15\r\n\
        a=rtpmap:100 telephone-event/8000\r\n\
        a=fmtp:100 0-15\r\n\
        a=sendrecv\r\n";

    /// The SBC's answer at setup: AMR-WB octet-aligned at 96.
    pub(in crate::ims::agent) const LIVE_SETUP_ANSWER: &str = "v=0\r\n\
        o=- 73193688 73193688 IN IP4 198.51.100.48\r\n\
        s=SBC call\r\n\
        c=IN IP4 198.51.100.48\r\n\
        t=0 0\r\n\
        m=audio 33694 RTP/AVP 96 105\r\n\
        a=rtpmap:96 AMR-WB/16000\r\n\
        a=fmtp:96 octet-align=1\r\n\
        a=ptime:20\r\n\
        a=rtpmap:105 telephone-event/16000\r\n\
        a=fmtp:105 0-15\r\n";

    /// The SBC's hold: a new version, `sendonly`, the same address, and its
    /// full codec list, with AMR-WB only at 102 bandwidth-efficient.
    pub(in crate::ims::agent) const LIVE_HOLD_OFFER: &str = "v=0\r\n\
        o=- 73193688 73193689 IN IP4 198.51.100.48\r\n\
        s=SBC call\r\n\
        c=IN IP4 198.51.100.48\r\n\
        t=0 0\r\n\
        m=audio 33694 RTP/AVP 100 106 102 101 103\r\n\
        a=rtpmap:100 telephone-event/8000\r\n\
        a=ptime:20\r\n\
        a=sendonly\r\n\
        a=rtpmap:106 EVS/16000\r\n\
        a=fmtp:106 br=9.6-128;bw=swb;mode-change-capability=2;cmr=0;dtx=1;ch-aw-recv=3;max-red=0\r\n\
        a=rtpmap:102 AMR-WB/16000\r\n\
        a=fmtp:102 mode-change-capability=2;max-red=0\r\n\
        a=rtpmap:101 AMR/8000\r\n\
        a=fmtp:101 mode-change-capability=2;max-red=0\r\n\
        a=rtpmap:103 telephone-event/16000\r\n\
        a=fmtp:103 0-15\r\n";

    /// The SBC's resume, three minutes later: the next version, no direction.
    pub(in crate::ims::agent) fn live_resume_offer() -> String {
        LIVE_HOLD_OFFER
            .replace("73193688 73193689", "73193688 73193690")
            .replace("a=sendonly\r\n", "")
    }

    /// The live call's media as set up, its relay's carrier leg labelled as
    /// the setup answer agreed, able to follow labels as `relabel` says.
    pub(in crate::ims::agent) fn live_established(
        relabel: Relabel,
    ) -> (EstablishedMedia, Arc<CarrierLeg>) {
        let leg = CarrierLeg::new(
            &sdp::ChosenCodec {
                codec: sdp::NegotiatedCodec::AmrWb,
                payload_type: 96,
                octet_aligned: true,
                dtmf_payload_type: Some(105),
                dtmf_clock_rate: Some(16000),
            },
            relabel,
        );
        let media = EstablishedMedia::new(
            LIVE_OUR_OFFER.to_string(),
            LIVE_SETUP_ANSWER,
            "198.51.100.48:33694".parse().unwrap(),
            96,
        )
        .unwrap()
        .with_carrier_leg(leg.clone());
        (media, leg)
    }

    fn modification(verdict: ReinviteVerdict) -> Modification {
        match verdict {
            ReinviteVerdict::Modify(modification) => modification,
            other => panic!("expected a modification, got {other:?}"),
        }
    }

    const HELD_AT_102: CarrierLegState = CarrierLegState {
        labels: CarrierLabels {
            payload_type: 102,
            octet_aligned: false,
            dtmf_payload_type: Some(103),
            dtmf_clock_rate: Some(16000),
        },
        sending: false,
        // The hold leaves 96 unlisted, so the carrier's late AMR-WB under it
        // is still read.
        previous_audio: Some((96, true)),
    };

    /// The live hold is answered `recvonly`, keeping AMR-WB under the only
    /// payload type the offer lists it at, in the offer's framing, as the
    /// next version of this side's SDP.
    #[test]
    fn the_live_hold_is_answered_recvonly_with_amr_wb_at_its_new_payload_type() {
        let (media, _leg) = live_established(Relabel::Any);
        let hold = modification(media.classify(Some(LIVE_HOLD_OFFER)));
        let answer = hold.answer();
        for line in [
            "o=- 2357052722 2357052723 IN IP4 192.0.2.67\r\n",
            "s=gsm-sip-bridge test call\r\n",
            "c=IN IP4 192.0.2.67\r\n",
            "m=audio 36185 RTP/AVP 102 103\r\n",
            "a=rtpmap:102 AMR-WB/16000\r\n",
            "a=fmtp:102 mode-change-capability=2;max-red=0\r\n",
            "a=rtpmap:103 telephone-event/16000\r\n",
            "a=recvonly\r\n",
        ] {
            assert!(answer.contains(line), "{line:?} missing from\n{answer}");
        }
        for absent in ["octet-align", "a=sendrecv", "EVS", "AMR/8000", ":96 "] {
            assert!(!answer.contains(absent), "{absent:?} in\n{answer}");
        }
        assert_eq!(hold.leg(), HELD_AT_102);
        assert_eq!(hold.local_direction(), MediaDirection::RecvOnly);
    }

    /// Nothing changes until the answer has been sent and `apply` is called;
    /// then the relay holds and relabels, and the answer is this side's SDP.
    #[test]
    fn applying_the_live_hold_holds_and_relabels_the_relay() {
        let (mut media, leg) = live_established(Relabel::Any);
        let before = leg.state();
        let hold = modification(media.classify(Some(LIVE_HOLD_OFFER)));
        assert_eq!(leg.state(), before, "classifying changes nothing");
        media.apply(&hold);
        assert_eq!(leg.state(), HELD_AT_102);
        assert_eq!(media.local_sdp(), hold.answer());
    }

    /// The live resume after the hold: `sendrecv` again, still 102, the
    /// version after the hold's answer, and the relay sends again.
    #[test]
    fn the_live_resume_after_the_hold_sends_again() {
        let (mut media, leg) = live_established(Relabel::Any);
        media.apply(&modification(media.classify(Some(LIVE_HOLD_OFFER))));
        let resume = modification(media.classify(Some(&live_resume_offer())));
        let answer = resume.answer();
        for line in [
            "o=- 2357052722 2357052724 IN IP4 192.0.2.67\r\n",
            "m=audio 36185 RTP/AVP 102 103\r\n",
            "a=sendrecv\r\n",
        ] {
            assert!(answer.contains(line), "{line:?} missing from\n{answer}");
        }
        media.apply(&resume);
        assert_eq!(
            leg.state(),
            CarrierLegState {
                sending: true,
                ..HELD_AT_102
            }
        );
    }

    /// The resume alone (as if the hold had been lost) still moves the call
    /// to the format the carrier now offers, sending.
    #[test]
    fn the_live_resume_alone_relabels_without_holding() {
        let (media, _leg) = live_established(Relabel::Any);
        let resume = modification(media.classify(Some(&live_resume_offer())));
        assert_eq!(
            resume.leg(),
            CarrierLegState {
                sending: true,
                ..HELD_AT_102
            }
        );
        assert!(resume.answer().contains("a=sendrecv\r\n"));
    }

    /// A retransmission of the hold (its `200 OK` lost) has the origin the
    /// hold made the baseline: a refresh, answered with the hold's answer.
    #[test]
    fn a_retransmitted_hold_is_answered_with_the_same_answer() {
        let (mut media, leg) = live_established(Relabel::Any);
        let hold = modification(media.classify(Some(LIVE_HOLD_OFFER)));
        media.apply(&hold);
        assert_eq!(
            media.classify(Some(LIVE_HOLD_OFFER)),
            ReinviteVerdict::Refresh
        );
        assert_eq!(media.local_sdp(), hold.answer());
        assert_eq!(leg.state(), HELD_AT_102);
    }

    /// A refresh during the hold restates the hold: the carrier's same-version
    /// `sendonly` offer is a refresh, and an offerless refresh's answer may
    /// be `sendonly` or `inactive` (RFC 3264 §6.1 for this side's `recvonly`
    /// offer), but not `sendrecv`, which would answer something not offered.
    #[test]
    fn a_session_refresh_during_the_hold_keeps_it() {
        let (mut media, leg) = live_established(Relabel::Any);
        let hold = modification(media.classify(Some(LIVE_HOLD_OFFER)));
        media.apply(&hold);
        let answer_at = |version: &str, direction: &str| {
            format!(
                "v=0\r\n\
                 o=- 73193688 {version} IN IP4 198.51.100.48\r\n\
                 s=SBC call\r\n\
                 c=IN IP4 198.51.100.48\r\n\
                 t=0 0\r\n\
                 m=audio 33694 RTP/AVP 102 103\r\n\
                 a=rtpmap:102 AMR-WB/16000\r\n\
                 a=fmtp:102 mode-change-capability=2;max-red=0\r\n\
                 a=rtpmap:103 telephone-event/16000\r\n\
                 {direction}"
            )
        };
        assert_eq!(media.classify(None), ReinviteVerdict::OfferlessRefresh);
        assert_eq!(
            media.take_answer(&answer_at("73193691", "a=sendonly\r\n")),
            Ok(())
        );
        assert_eq!(leg.state(), HELD_AT_102);
        assert_eq!(
            media.take_answer(&answer_at("73193692", "a=inactive\r\n")),
            Ok(())
        );
        assert_eq!(leg.state(), HELD_AT_102);
        assert_eq!(
            media.take_answer(&answer_at("73193693", "")),
            Err("the answer's media direction does not answer the offer's")
        );
        assert_eq!(leg.state(), HELD_AT_102);
    }

    /// A carrier may also hold by answering this side's `sendrecv` offer in
    /// an offerless refresh with `sendonly` or `inactive`; the relay follows
    /// it, and a later `sendrecv` answer resumes.
    #[test]
    fn an_offerless_refresh_answer_can_hold_and_resume() {
        let (mut media, leg) = live_established(Relabel::Any);
        let held =
            LIVE_SETUP_ANSWER.replace("73193688 73193688", "73193688 73193689") + "a=inactive\r\n";
        assert_eq!(media.take_answer(&held), Ok(()));
        assert!(!leg.state().sending);
        let resumed = LIVE_SETUP_ANSWER.replace("73193688 73193688", "73193688 73193690");
        assert_eq!(media.take_answer(&resumed), Ok(()));
        assert!(leg.state().sending);
        assert_eq!(leg.state().labels.payload_type, 96, "labels unchanged");
    }

    /// A hold that still lists the running format keeps it: no relabel.
    #[test]
    fn a_hold_listing_the_running_format_keeps_it() {
        let (media, _leg) = live_established(Relabel::Any);
        let offer =
            LIVE_SETUP_ANSWER.replace("73193688 73193688", "73193688 73193689") + "a=sendonly\r\n";
        let hold = modification(media.classify(Some(&offer)));
        assert_eq!(
            hold.leg(),
            CarrierLegState {
                labels: CarrierLabels {
                    payload_type: 96,
                    octet_aligned: true,
                    dtmf_payload_type: Some(105),
                    dtmf_clock_rate: Some(16000),
                },
                sending: false,
                previous_audio: None,
            }
        );
        assert!(hold.answer().contains("m=audio 36185 RTP/AVP 96 105\r\n"));
        assert!(hold.answer().contains("a=fmtp:96 octet-align=1\r\n"));
    }

    /// Of several entries for the running codec, the carrier's first wins,
    /// whatever its framing: here 102 bandwidth-efficient, listed before 104
    /// octet-aligned, the framing the call is running.
    #[test]
    fn the_carriers_first_entry_for_the_codec_wins() {
        let (media, _leg) = live_established(Relabel::Any);
        let offer = LIVE_HOLD_OFFER.replace(
            "a=rtpmap:103 telephone-event/16000\r\n",
            "a=rtpmap:103 telephone-event/16000\r\na=rtpmap:104 AMR-WB/16000\r\na=fmtp:104 octet-align=1\r\n",
        )
        .replace("102 101 103", "102 101 103 104");
        let hold = modification(media.classify(Some(&offer)));
        assert_eq!(hold.leg().labels.payload_type, 102);
        assert!(!hold.leg().labels.octet_aligned);
    }

    /// T2's resume on 2026-11-21, after a hold at 102 bandwidth-efficient:
    /// AMR-WB first at 96 octet-aligned, the format setup agreed, and only
    /// later at 112 bandwidth-efficient. The call goes back to 96, and 102,
    /// unlisted, is the old number now.
    #[test]
    fn the_live_resume_goes_back_to_the_carriers_preferred_framing() {
        let (mut media, _leg) = live_established(Relabel::Any);
        media.apply(&modification(media.classify(Some(LIVE_HOLD_OFFER))));
        let resume = live_resume_offer()
            .replace(
                "RTP/AVP 100 106 102 101 103",
                "RTP/AVP 96 105 106 112 101 103",
            )
            .replace(
                "a=rtpmap:100 telephone-event/8000\r\n",
                "a=rtpmap:96 AMR-WB/16000/1\r\n\
                 a=fmtp:96 octet-align=1;mode-change-capability=2;max-red=0\r\n\
                 a=rtpmap:105 telephone-event/16000\r\na=fmtp:105 0-15\r\n",
            )
            .replace("a=rtpmap:102 AMR-WB", "a=rtpmap:112 AMR-WB")
            .replace("a=fmtp:102 ", "a=fmtp:112 ")
            .replace(
                "a=rtpmap:103 telephone-event/16000\r\na=fmtp:103 0-15\r\n",
                "",
            );
        let resumed = modification(media.classify(Some(&resume)));
        let answer = resumed.answer();
        for line in [
            "m=audio 36185 RTP/AVP 96 105\r\n",
            "a=fmtp:96 octet-align=1;mode-change-capability=2;max-red=0\r\n",
            "a=sendrecv\r\n",
        ] {
            assert!(answer.contains(line), "{line:?} missing from\n{answer}");
        }
        assert_eq!(
            resumed.leg(),
            CarrierLegState {
                labels: CarrierLabels {
                    payload_type: 96,
                    octet_aligned: true,
                    dtmf_payload_type: Some(105),
                    dtmf_clock_rate: Some(16000),
                },
                sending: true,
                previous_audio: Some((102, false)),
            }
        );
    }

    /// A relay that can only keep its audio labels (the PCMU pass-through)
    /// skips the carrier's first entry for one it can follow.
    #[test]
    fn an_entry_the_relay_cannot_follow_is_passed_over_for_one_it_can() {
        let (media, _leg) = live_established(Relabel::DtmfOnly);
        let offer = LIVE_HOLD_OFFER
            .replace("102 101 103\r\n", "102 101 103 96\r\n")
            .replace(
                "a=fmtp:103 0-15\r\n",
                "a=fmtp:103 0-15\r\na=rtpmap:96 AMR-WB/16000\r\na=fmtp:96 octet-align=1\r\n",
            );
        let hold = modification(media.classify(Some(&offer)));
        assert_eq!(hold.leg().labels.payload_type, 96);
        assert!(hold.leg().labels.octet_aligned);
        assert!(!hold.leg().sending);
    }

    /// T2's hold on 2026-11-21: AMR-WB only at 102, with no `a=fmtp`, and 96,
    /// the number the call ran AMR-WB under, now `telephone-event/8000`.
    fn rebinding_hold_offer() -> String {
        LIVE_HOLD_OFFER
            .replace(
                "m=audio 33694 RTP/AVP 100 106 102 101 103\r\n",
                "m=audio 33694 RTP/AVP 96 106 102 101 103\r\n",
            )
            .replace(
                "a=rtpmap:100 telephone-event/8000",
                "a=rtpmap:96 telephone-event/8000",
            )
            .replace("a=fmtp:102 mode-change-capability=2;max-red=0\r\n", "")
    }

    /// The hold is answered with no empty `a=fmtp` for the codec it listed
    /// without one, and 96 is no longer read as AMR-WB: the offer gave it to
    /// `telephone-event`.
    #[test]
    fn a_hold_that_rebinds_the_old_payload_type_retires_it() {
        let (mut media, leg) = live_established(Relabel::Any);
        let hold = modification(media.classify(Some(&rebinding_hold_offer())));
        let answer = hold.answer();
        assert!(answer.contains("a=rtpmap:102 AMR-WB/16000\r\n"), "{answer}");
        assert!(!answer.contains("a=fmtp:102"), "no empty fmtp in\n{answer}");
        assert_eq!(
            hold.leg(),
            CarrierLegState {
                previous_audio: None,
                ..HELD_AT_102
            }
        );
        media.apply(&hold);
        assert_eq!(leg.state().previous_audio, None);
    }

    /// A number the offer gives to a codec this bridge does not know (EVS)
    /// is retired just the same, and so is one restated with the other AMR
    /// framing.
    #[test]
    fn an_old_payload_type_given_another_format_is_retired() {
        let (media, _leg) = live_established(Relabel::Any);
        for rtpmap in ["EVS/16000", "AMR-WB/16000"] {
            let offer = LIVE_HOLD_OFFER
                .replace("102 101 103\r\n", "102 101 103 96\r\n")
                .replace(
                    "a=fmtp:103 0-15\r\n",
                    &format!("a=fmtp:103 0-15\r\na=rtpmap:96 {rtpmap}\r\n"),
                );
            let hold = modification(media.classify(Some(&offer)));
            assert_eq!(hold.leg().labels.payload_type, 102, "{rtpmap}");
            assert_eq!(hold.leg().previous_audio, None, "{rtpmap}");
        }
    }

    /// A later offer that keeps the running labels keeps the old number
    /// too, while it either leaves it out or restates it as the same audio,
    /// framed the same; giving it to anything else retires it then.
    #[test]
    fn a_later_offer_keeps_the_old_payload_type_while_it_means_the_same_audio() {
        let (mut media, _leg) = live_established(Relabel::Any);
        media.apply(&modification(media.classify(Some(LIVE_HOLD_OFFER))));
        let resume_listing_96 = |rtpmap: &str| {
            live_resume_offer()
                .replace("102 101 103\r\n", "102 101 103 96\r\n")
                .replace(
                    "a=fmtp:103 0-15\r\n",
                    &format!("a=fmtp:103 0-15\r\na=rtpmap:96 {rtpmap}\r\n"),
                )
        };
        let previous = |offer: &str| {
            let resume = modification(media.classify(Some(offer)));
            assert_eq!(resume.leg().labels, HELD_AT_102.labels);
            resume.leg().previous_audio
        };
        assert_eq!(previous(&live_resume_offer()), Some((96, true)));
        assert_eq!(
            previous(&resume_listing_96(
                "AMR-WB/16000\r\na=fmtp:96 octet-align=1"
            )),
            Some((96, true))
        );
        assert_eq!(previous(&resume_listing_96("telephone-event/8000")), None);
    }

    /// The live resume after a rebinding hold (2026-11-21) moves AMR-WB on
    /// again, from 102 to 112: 102, unlisted, is the old number now.
    #[test]
    fn a_resume_after_a_rebinding_hold_keeps_the_number_it_leaves() {
        let (mut media, _leg) = live_established(Relabel::Any);
        media.apply(&modification(media.classify(Some(&rebinding_hold_offer()))));
        let resume = rebinding_hold_offer()
            .replace("73193688 73193689", "73193688 73193690")
            .replace("a=sendonly\r\n", "")
            .replace("RTP/AVP 96 106 102 101 103", "RTP/AVP 96 106 112 101 103")
            .replace("a=rtpmap:102 AMR-WB", "a=rtpmap:112 AMR-WB");
        let resumed = modification(media.classify(Some(&resume)));
        assert_eq!(resumed.leg().labels.payload_type, 112);
        assert_eq!(resumed.leg().previous_audio, Some((102, false)));
        assert!(resumed.leg().sending);
    }

    /// Every hold direction is answered as RFC 3264 §6.1 has it, and the
    /// relay sends only when the carrier receives.
    #[test]
    fn each_offered_direction_gets_its_answer() {
        let (media, _leg) = live_established(Relabel::Any);
        for (offered, answered, sending) in [
            ("a=sendonly\r\n", "a=recvonly\r\n", false),
            ("a=inactive\r\n", "a=inactive\r\n", false),
            ("a=recvonly\r\n", "a=sendonly\r\n", true),
            ("a=sendrecv\r\n", "a=sendrecv\r\n", true),
        ] {
            let offer = LIVE_HOLD_OFFER.replace("a=sendonly\r\n", offered);
            let m = modification(media.classify(Some(&offer)));
            assert!(m.answer().contains(answered), "{offered}: {}", m.answer());
            assert_eq!(m.leg().sending, sending, "{offered}");
        }
    }

    /// What this side cannot follow is still declined, leaving the call as
    /// it was.
    #[test]
    fn a_modification_this_side_cannot_follow_is_declined() {
        let moved = LIVE_HOLD_OFFER.replace("m=audio 33694", "m=audio 33700");
        let another_session = LIVE_HOLD_OFFER.replace("o=- 73193688 ", "o=- 73190000 ");
        let no_origin =
            LIVE_HOLD_OFFER.replace("o=- 73193688 73193689 IN IP4 198.51.100.48\r\n", "");
        let no_amr_wb = LIVE_HOLD_OFFER
            .replace("100 106 102 101 103", "100 106 101 103")
            .replace("a=rtpmap:102 AMR-WB/16000\r\n", "");
        let preconditions =
            format!("{LIVE_HOLD_OFFER}a=curr:qos e2e none\r\na=des:qos mandatory e2e sendrecv\r\n");
        for (offer, why) in [
            (moved.as_str(), "the SDP moves the carrier's media address"),
            (
                another_session.as_str(),
                "the offer's o= line names another session",
            ),
            (no_origin.as_str(), "the offer has no o= line"),
            (
                no_amr_wb.as_str(),
                "the offer no longer lists the codec the call is running",
            ),
            (
                preconditions.as_str(),
                "the offer's QoS preconditions cannot be confirmed",
            ),
        ] {
            let (media, leg) = live_established(Relabel::Any);
            let before = leg.state();
            assert_eq!(
                media.classify(Some(offer)),
                ReinviteVerdict::Decline(why),
                "{offer}"
            );
            assert_eq!(leg.state(), before);
        }
    }

    /// The PCMU pass-through relay forwards audio unchanged, so a hold that
    /// moves PCMU to another payload type is declined, while one that only
    /// moves `telephone-event` is followed.
    #[test]
    fn the_pass_through_relay_follows_only_a_dtmf_relabel() {
        let setup = "v=0\r\n\
            o=- 500 500 IN IP4 198.51.100.48\r\n\
            s=SBC call\r\n\
            c=IN IP4 198.51.100.48\r\n\
            t=0 0\r\n\
            m=audio 33694 RTP/AVP 0 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n";
        let ours = LIVE_OUR_OFFER;
        let leg = CarrierLeg::new(
            &sdp::ChosenCodec {
                codec: sdp::NegotiatedCodec::Pcmu,
                payload_type: 0,
                octet_aligned: false,
                dtmf_payload_type: Some(101),
                dtmf_clock_rate: Some(8000),
            },
            Relabel::DtmfOnly,
        );
        let media = EstablishedMedia::new(
            ours.to_string(),
            setup,
            "198.51.100.48:33694".parse().unwrap(),
            0,
        )
        .unwrap()
        .with_carrier_leg(leg);

        let new_dtmf = setup
            .replace("500 500", "500 501")
            .replace("0 101", "0 100")
            .replace("rtpmap:101", "rtpmap:100")
            + "a=sendonly\r\n";
        let hold = modification(media.classify(Some(&new_dtmf)));
        assert_eq!(hold.leg().labels.telephone_event(), Some((100, 8000)));
        assert!(!hold.leg().sending);

        let moved_pcmu = setup
            .replace("500 500", "500 501")
            .replace("RTP/AVP 0 101", "RTP/AVP 98 101")
            .replace("rtpmap:0 PCMU", "rtpmap:98 PCMU");
        assert_eq!(
            media.classify(Some(&moved_pcmu)),
            ReinviteVerdict::Decline(
                "the call's media relay cannot follow the offer's payload type or framing"
            )
        );
    }

    #[test]
    fn origins_name_the_same_session_whatever_their_versions() {
        let a = "o=- 73193688 73193689 IN IP4 198.51.100.48";
        assert!(same_session(
            a,
            "o=- 73193688 73193688 IN IP4 198.51.100.48"
        ));
        assert!(!same_session(
            a,
            "o=- 73193687 73193689 IN IP4 198.51.100.48"
        ));
        assert!(!same_session(
            a,
            "o=- 73193688 73193689 IN IP4 198.51.100.49"
        ));
        assert!(!same_session(a, "o=- 73193688 73193689 IN IP4"));
    }

    #[test]
    fn answer_directions_follow_rfc_3264() {
        use MediaDirection::{Inactive, RecvOnly, SendOnly, SendRecv};
        let all = [SendRecv, SendOnly, RecvOnly, Inactive];
        assert!(all.iter().all(|d| answers(SendRecv, *d)));
        assert_eq!(
            all.map(|d| answers(SendOnly, d)),
            [false, false, true, true]
        );
        assert_eq!(
            all.map(|d| answers(RecvOnly, d)),
            [false, true, false, true]
        );
        assert_eq!(
            all.map(|d| answers(Inactive, d)),
            [false, false, false, true]
        );
    }
}
