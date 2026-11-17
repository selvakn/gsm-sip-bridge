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
//! and the audio format the call is already using, because the media path does
//! not follow a change. The format is the payload type *and* what the carrier
//! declares for it, codec and AMR framing (`octet-align`): the number alone
//! does not show that the relay can still read the carrier's audio. An answer that fails the check means the refresh failed
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
//! Anything else — a new version, a moved address, a hold — is still a real
//! modification this bridge does not implement, and is still declined `488`.

use super::registration_refresh;
use super::session_refresh::{self, PeerRefreshTerms, SessionIntervalTooSmall};
use crate::ims::sdp::{self, MediaDirection};
use crate::ims::sip_client::SipRequest;
use std::net::SocketAddr;
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
    /// What the carrier's SDP at setup declared for that payload type. The
    /// baseline is the carrier's own declaration, not this bridge's reading
    /// of it, so an answer that repeats the setup SDP is always unchanged,
    /// and one that keeps the number but declares another codec or framing
    /// never is. `None` when the setup SDP declared nothing recognisable.
    remote_format: Option<DeclaredFormat>,
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
            remote_format: sdp::parse_offer(remote_sdp)
                .ok()
                .and_then(|parsed| declared_format(&parsed, payload_type)),
            local_sdp,
            remote_rtp,
            payload_type,
            awaiting_ack: None,
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
            Ok(_) => ReinviteVerdict::Refresh,
            Err(why) => ReinviteVerdict::Decline(why),
        }
    }

    /// The carrier's media address and direction in `sdp` are the ones the
    /// call is running with.
    fn check_media(&self, body: &str) -> Result<sdp::SdpOffer, &'static str> {
        let parsed = sdp::parse_offer(body).map_err(|_| "the SDP could not be parsed")?;
        if parsed.remote_rtp != self.remote_rtp {
            return Err("the SDP moves the carrier's media address");
        }
        if parsed.direction != MediaDirection::SendRecv {
            return Err("the SDP changes the media direction (hold)");
        }
        Ok(parsed)
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
    pub(super) fn take_answer(&mut self, answer: &str) -> Result<(), &'static str> {
        let parsed = self.check_media(answer)?;
        if sdp::first_audio_payload_type(answer) != Some(self.payload_type) {
            return Err("the answer selects a different payload type");
        }
        if declared_format(&parsed, self.payload_type) != self.remote_format {
            return Err("the answer changes the codec or AMR framing of the payload type in use");
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
}
