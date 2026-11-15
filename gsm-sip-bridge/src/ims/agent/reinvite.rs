//! A carrier's in-dialog re-INVITE that only refreshes the session.
//!
//! RFC 4028 lets either refresh transport be used, and a carrier that refreshes
//! with re-INVITE rather than `UPDATE` sends one every half session interval.
//! This bridge does not renegotiate media mid-call, and used to decline every
//! re-INVITE `488` — which a carrier treats as a failed refresh. Measured on T2
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
//! still `sendrecv` is a no-op refresh, answered `200 OK` with the SDP it
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
//! That answer is checked: it must keep the carrier's media address, `sendrecv`,
//! and the payload type the call is already using, because the media path does
//! not follow a change. An answer that fails the check means the refresh failed
//! — there is no response to an `ACK` to decline it with — so the call is ended
//! with `BYE`, as RFC 3261 §13.2.2.4 has a UAC do with an unacceptable offer in
//! a `2xx`, rather than left with media neither side agreed on.
//!
//! Anything else — a new version, a moved address, a hold — is still a real
//! modification this bridge does not implement, and is still declined `488`.

use super::session_refresh::{self, PeerRefreshTerms};
use crate::ims::sdp::{self, MediaDirection};
use crate::ims::sip_client::SipRequest;
use std::net::SocketAddr;
use std::time::Instant;

/// The session description both sides agreed on when the call was set up —
/// what a session-refresh re-INVITE is compared against, and what this
/// bridge answers it with.
#[derive(Debug, Clone)]
pub(super) struct EstablishedMedia {
    /// The SDP this bridge last sent the carrier: its offer, on a call it
    /// placed; its answer, on one it answered (the offerless path's offer
    /// included). Resent verbatim — same `o=` version — on a refresh.
    local_sdp: String,
    /// The carrier's `o=` line from its latest SDP.
    remote_origin: String,
    /// Where the carrier's media for this call comes from.
    remote_rtp: SocketAddr,
    /// The carrier-leg audio payload type the media path is running with.
    payload_type: u8,
    /// The `CSeq` number of an offerless refresh re-INVITE this bridge
    /// answered with an offer, while the `ACK` carrying the carrier's answer
    /// is still due.
    awaiting_answer: Option<u32>,
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
    /// A real modification, or nothing to compare it with: decline `488`, as
    /// before. The string says why, for the log.
    Decline(&'static str),
}

impl EstablishedMedia {
    /// `local_sdp` is what this bridge sent, `remote_sdp` what the carrier
    /// sent, and `payload_type` the carrier-leg format the answer selected.
    /// `None` when the carrier's SDP has no `o=` line — then no later offer
    /// can be shown to be unchanged, and every re-INVITE stays declined.
    pub(super) fn new(
        local_sdp: String,
        remote_sdp: &str,
        remote_rtp: SocketAddr,
        payload_type: u8,
    ) -> Option<Self> {
        Some(Self {
            remote_origin: normalized(sdp::origin_line(remote_sdp)?),
            local_sdp,
            remote_rtp,
            payload_type,
            awaiting_answer: None,
        })
    }

    pub(super) fn local_sdp(&self) -> &str {
        &self.local_sdp
    }

    /// Whether `offer` (the re-INVITE's SDP, if it carried one) leaves this
    /// call's session description unchanged.
    pub(super) fn classify(&self, offer: Option<&str>) -> ReinviteVerdict {
        let Some(offer) = offer else {
            return ReinviteVerdict::OfferlessRefresh;
        };
        if sdp::origin_line(offer).map(normalized).as_deref() != Some(self.remote_origin.as_str()) {
            return ReinviteVerdict::Decline("the offer has a new o= origin (a media change)");
        }
        match self.check_media(offer) {
            Ok(()) => ReinviteVerdict::Refresh,
            Err(why) => ReinviteVerdict::Decline(why),
        }
    }

    /// The carrier's media address and direction in `sdp` are the ones the
    /// call is running with.
    fn check_media(&self, body: &str) -> Result<(), &'static str> {
        let parsed = sdp::parse_offer(body).map_err(|_| "the SDP could not be parsed")?;
        if parsed.remote_rtp != self.remote_rtp {
            return Err("the SDP moves the carrier's media address");
        }
        if parsed.direction != MediaDirection::SendRecv {
            return Err("the SDP changes the media direction (hold)");
        }
        Ok(())
    }

    /// Record that an offerless refresh re-INVITE with `CSeq` number `cseq`
    /// was answered with an offer; its `ACK` brings the answer. A later
    /// re-INVITE replaces it.
    pub(super) fn await_answer(&mut self, cseq: Option<u32>) {
        self.awaiting_answer = cseq;
    }

    /// Clear any outstanding offerless refresh — a later re-INVITE with an
    /// offer supersedes it.
    pub(super) fn clear_awaiting_answer(&mut self) {
        self.awaiting_answer = None;
    }

    /// Whether an `ACK` with `CSeq` number `cseq` is the one completing an
    /// offerless refresh.
    pub(super) fn completes_refresh(&self, cseq: Option<u32>) -> bool {
        self.awaiting_answer.is_some() && self.awaiting_answer == cseq
    }

    /// Takes the carrier's `answer` from the `ACK` completing an offerless
    /// refresh. `Ok` when the call's media carries on as it is (and the
    /// carrier's origin is updated to the answer's); `Err` with why when the
    /// answer cannot be followed.
    pub(super) fn take_answer(&mut self, answer: Option<&str>) -> Result<(), &'static str> {
        self.awaiting_answer = None;
        let answer = answer.ok_or("the ACK carried no SDP answer")?;
        self.check_media(answer)?;
        if sdp::first_audio_payload_type(answer) != Some(self.payload_type) {
            return Err("the answer selects a different payload type");
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

/// The `Session-Expires` terms for answering a refresh re-INVITE `req`, and
/// the extra headers they put on the `200 OK`.
pub(super) fn refresh_terms(
    req: &SipRequest,
    now: Instant,
) -> (PeerRefreshTerms, Vec<(&'static str, String)>) {
    let carrier_supports_timer = req
        .headers_all("Supported")
        .iter()
        .flat_map(|value| value.split(','))
        .any(|tag| tag.trim().eq_ignore_ascii_case("timer"));
    let terms = session_refresh::terms_for_peer_refresh(
        req.header("Session-Expires"),
        carrier_supports_timer,
        now,
    );
    let mut headers = Vec::new();
    if let Some(value) = &terms.session_expires {
        headers.push(("Session-Expires", value.clone()));
    }
    if terms.require_timer {
        headers.push(("Require", "timer".to_string()));
    }
    (terms, headers)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

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

    #[test]
    fn a_new_sdp_version_is_a_modification() {
        let offer = REFRESH_OFFER.replace("71848567 71848567", "71848567 71848568");
        assert!(matches!(
            established().classify(Some(&offer)),
            ReinviteVerdict::Decline(_)
        ));
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
    fn only_the_awaited_ack_completes_an_offerless_refresh() {
        let mut media = established();
        assert!(!media.completes_refresh(Some(2)), "nothing awaited yet");
        media.await_answer(Some(2));
        assert!(media.completes_refresh(Some(2)));
        assert!(!media.completes_refresh(Some(3)));
        assert!(!media.completes_refresh(None));
        media.clear_awaiting_answer();
        assert!(!media.completes_refresh(Some(2)));
    }

    #[test]
    fn an_ack_answer_keeping_the_media_completes_the_refresh() {
        let mut media = established();
        media.await_answer(Some(2));
        assert_eq!(media.take_answer(Some(SETUP_ANSWER)), Ok(()));
        assert!(!media.completes_refresh(Some(2)), "settled once");
    }

    #[test]
    fn a_new_origin_in_an_acceptable_answer_becomes_the_baseline() {
        let mut media = established();
        let answer = SETUP_ANSWER.replace("71848567 71848567", "71848567 71848568");
        media.await_answer(Some(2));
        assert_eq!(media.take_answer(Some(&answer)), Ok(()));
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
        for answer in [
            None,
            Some(other_payload_type.as_str()),
            Some(moved.as_str()),
            Some(held.as_str()),
        ] {
            let mut media = established();
            media.await_answer(Some(2));
            assert!(media.take_answer(answer).is_err(), "{answer:?}");
        }
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
}
