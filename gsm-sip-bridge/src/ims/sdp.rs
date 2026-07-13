//! Minimal SDP (RFC 4566) build/parse — just enough for a two-codec
//! (PCMU, AMR-WB) audio offer/answer, not a general-purpose SDP library.

use crate::error::{BridgeError, BridgeResult};
use std::net::{IpAddr, SocketAddr};

const PCMU_PAYLOAD_TYPE: u8 = 0;
/// Dynamic payload type (RFC 3551 §6: 96-127 range) chosen for AMR-WB —
/// arbitrary but must match between the `a=rtpmap`/`a=fmtp` lines here and
/// whatever `parse_answer` compares the answer's payload type against.
const AMR_WB_PAYLOAD_TYPE: u8 = 96;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegotiatedCodec {
    Pcmu,
    /// AMR narrowband (`AMR/8000`). Offered by carriers on mobile-terminating
    /// calls where the originating leg is narrowband — Airtel was observed
    /// offering it *alone*, with no AMR-WB and no PCMU, so it is not optional
    /// if inbound calls are to be answerable in general.
    AmrNb,
    AmrWb,
}

/// The codec `build_answer` selected, with everything the media path needs to
/// actually speak it: the offer's payload-type number (dynamic for both AMR
/// flavours — never assume 96) and its RTP framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChosenCodec {
    pub codec: NegotiatedCodec,
    pub payload_type: u8,
    /// True for RFC 4867 octet-aligned framing, false for bandwidth-efficient
    /// (bit-packed). Not a preference we get to make — it is declared by the
    /// offer's `a=fmtp` for this payload type. Meaningless for PCMU.
    pub octet_aligned: bool,
}

fn ip_addrtype(ip: IpAddr) -> &'static str {
    if ip.is_ipv6() {
        "IP6"
    } else {
        "IP4"
    }
}

/// Build an SDP offer, `session_id` as the `o=` origin id (any
/// stable-enough number; a random one is fine, this isn't a re-INVITE that
/// needs monotonic versioning). Always offers PCMU (payload type 0, no
/// negotiation needed, universally supported); additionally offers AMR-WB
/// (dynamic payload type 96, `octet-align=1` — RFC 4867's *default* is the
/// bit-packed "bandwidth-efficient" mode, which this client doesn't
/// implement, so this must be explicit) when `offer_amr_wb` is true — the
/// caller's job to only pass `true` when a real AMR-WB codec is actually
/// linked in (see `amr_safe::is_available()`), since offering a codec we
/// can't actually encode/decode would be worse than not offering it.
pub fn build_offer(local_ip: IpAddr, rtp_port: u16, session_id: u64, offer_amr_wb: bool) -> String {
    let addrtype = ip_addrtype(local_ip);
    let payload_types = if offer_amr_wb {
        format!("{PCMU_PAYLOAD_TYPE} {AMR_WB_PAYLOAD_TYPE}")
    } else {
        PCMU_PAYLOAD_TYPE.to_string()
    };

    let mut sdp = format!(
        "v=0\r\n\
         o=- {session_id} {session_id} IN {addrtype} {local_ip}\r\n\
         s=gsm-sip-bridge test call\r\n\
         c=IN {addrtype} {local_ip}\r\n\
         t=0 0\r\n\
         m=audio {rtp_port} RTP/AVP {payload_types}\r\n\
         a=rtpmap:{PCMU_PAYLOAD_TYPE} PCMU/8000\r\n",
    );
    if offer_amr_wb {
        sdp.push_str(&format!(
            "a=rtpmap:{AMR_WB_PAYLOAD_TYPE} AMR-WB/16000\r\n\
             a=fmtp:{AMR_WB_PAYLOAD_TYPE} octet-align=1\r\n",
        ));
    }
    sdp.push_str("a=sendrecv\r\n");
    sdp
}

pub struct SdpAnswer {
    pub remote_rtp: SocketAddr,
    pub codec: NegotiatedCodec,
}

/// Parse an SDP answer body down to just what's needed to send/receive
/// RTP: the connection address (`c=`), the `m=audio` port, and which codec
/// the answer selected (identified by comparing its payload type against
/// the ones we offered — RFC 3264 requires the answer's payload type on a
/// re-used dynamic number to mean what the offer said it meant, so this
/// doesn't need to re-parse the answer's own `a=rtpmap`).
pub fn parse_answer(body: &str) -> BridgeResult<SdpAnswer> {
    let mut conn_ip: Option<IpAddr> = None;
    let mut rtp_port: Option<u16> = None;
    let mut payload_type: Option<u8> = None;

    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("c=IN ") {
            // "IP4 1.2.3.4" or "IP6 2001:db8::1"
            let addr_str = rest.split_whitespace().nth(1);
            if let Some(addr_str) = addr_str {
                conn_ip = addr_str.parse().ok();
            }
        } else if let Some(rest) = line.strip_prefix("m=audio ") {
            // "<port> RTP/AVP <pt> [<pt> ...]" — take the first payload type.
            let mut fields = rest.split_whitespace();
            rtp_port = fields.next().and_then(|p| p.parse().ok());
            payload_type = fields.nth(1).and_then(|pt| pt.parse().ok());
        }
    }

    let conn_ip = conn_ip
        .ok_or_else(|| BridgeError::Ims("SDP answer missing c= connection address".into()))?;
    let rtp_port =
        rtp_port.ok_or_else(|| BridgeError::Ims("SDP answer missing m=audio port".into()))?;
    let payload_type = payload_type
        .ok_or_else(|| BridgeError::Ims("SDP answer's m=audio line has no payload type".into()))?;

    let codec = match payload_type {
        PCMU_PAYLOAD_TYPE => NegotiatedCodec::Pcmu,
        AMR_WB_PAYLOAD_TYPE => NegotiatedCodec::AmrWb,
        other => {
            return Err(BridgeError::Ims(format!(
                "SDP answer selected an unoffered/unsupported payload type: {other}"
            )))
        }
    };

    Ok(SdpAnswer {
        remote_rtp: SocketAddr::new(conn_ip, rtp_port),
        codec,
    })
}

/// One codec offered in an inbound SDP offer, in the order its payload type
/// appeared on the `m=audio` line — the payload type is whatever number the
/// offerer chose (unlike `build_offer`'s own fixed `PCMU_PAYLOAD_TYPE`/
/// `AMR_WB_PAYLOAD_TYPE`, an inbound offer's dynamic payload type for AMR-WB
/// isn't guaranteed to be 96), which `build_answer` must echo back verbatim
/// per RFC 3264 §6.1 (the answer reuses the offer's own payload type
/// numbers, it doesn't renumber them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferedCodec {
    pub payload_type: u8,
    pub codec: NegotiatedCodec,
    /// The offer's own `a=fmtp` parameters for this payload type, verbatim
    /// (empty when it had none).
    ///
    /// These must be *echoed*, not invented: AMR's `octet-align` is a
    /// declarative parameter (RFC 4867 §8.1) — the answerer may not flip it,
    /// it only states which framing the sender uses for that payload type. A
    /// carrier commonly offers AMR twice, once bandwidth-efficient and once
    /// octet-aligned, on two different payload types; answering the
    /// bandwidth-efficient one with `octet-align=1` is self-contradictory and
    /// gets the call torn down immediately (observed on Airtel: BYE ~250ms
    /// after our 200 OK).
    pub fmtp: String,
}

impl OfferedCodec {
    /// Whether this payload type is framed octet-aligned (RFC 4867 §4.4)
    /// rather than bandwidth-efficient (§4.3). Both are supported — see
    /// `ims::amr_rtp` — so this selects which framing the media path must use,
    /// and is never something we get to choose for ourselves.
    pub fn is_octet_aligned(&self) -> bool {
        self.fmtp
            .split(';')
            .map(|p| p.trim().replace(' ', ""))
            .any(|p| p == "octet-align=1")
    }
}

pub struct SdpOffer {
    pub remote_rtp: SocketAddr,
    /// Recognized codecs from the offer, in `m=audio` payload-type order.
    /// Payload types on the `m=audio` line with no matching `a=rtpmap` (or
    /// naming an unrecognized codec) are silently omitted rather than
    /// rejected outright — an offer can list codecs we don't support
    /// alongside ones we do, and that isn't itself an error.
    pub offered: Vec<OfferedCodec>,
}

/// Parse an inbound SDP offer (the inverse of `build_offer`): the connection
/// address, the `m=audio` port, and which of the listed payload types are
/// codecs this client recognizes (by matching each payload type's
/// `a=rtpmap:<pt> <name>/<rate>` line against PCMU/8000 and AMR-WB/16000).
pub fn parse_offer(body: &str) -> BridgeResult<SdpOffer> {
    let mut conn_ip: Option<IpAddr> = None;
    let mut rtp_port: Option<u16> = None;
    let mut listed_pts: Vec<u8> = Vec::new();
    let mut rtpmap: std::collections::HashMap<u8, (String, u32)> = std::collections::HashMap::new();
    let mut fmtp: std::collections::HashMap<u8, String> = std::collections::HashMap::new();

    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("c=IN ") {
            let addr_str = rest.split_whitespace().nth(1);
            if let Some(addr_str) = addr_str {
                conn_ip = addr_str.parse().ok();
            }
        } else if let Some(rest) = line.strip_prefix("m=audio ") {
            let mut fields = rest.split_whitespace();
            rtp_port = fields.next().and_then(|p| p.parse().ok());
            // Skip the "RTP/AVP" token, then collect every payload type.
            listed_pts = fields.skip(1).filter_map(|pt| pt.parse().ok()).collect();
        } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            // "<pt> <name>/<rate>[/<params>]"
            let mut parts = rest.splitn(2, ' ');
            let Some(pt) = parts.next().and_then(|p| p.parse::<u8>().ok()) else {
                continue;
            };
            let Some(name_rate) = parts.next() else {
                continue;
            };
            let mut nr = name_rate.splitn(2, '/');
            let (Some(name), Some(rate_str)) = (nr.next(), nr.next()) else {
                continue;
            };
            let Some(rate) = rate_str.split('/').next().and_then(|r| r.parse().ok()) else {
                continue;
            };
            rtpmap.insert(pt, (name.to_ascii_uppercase(), rate));
        } else if let Some(rest) = line.strip_prefix("a=fmtp:") {
            // "<pt> <params>"
            let mut parts = rest.splitn(2, ' ');
            let Some(pt) = parts.next().and_then(|p| p.parse::<u8>().ok()) else {
                continue;
            };
            if let Some(params) = parts.next() {
                fmtp.insert(pt, params.trim().to_string());
            }
        }
    }

    let conn_ip = conn_ip
        .ok_or_else(|| BridgeError::Ims("SDP offer missing c= connection address".into()))?;
    let rtp_port =
        rtp_port.ok_or_else(|| BridgeError::Ims("SDP offer missing m=audio port".into()))?;
    if listed_pts.is_empty() {
        return Err(BridgeError::Ims(
            "SDP offer's m=audio line lists no payload types".into(),
        ));
    }

    let mut offered = Vec::new();
    for pt in listed_pts {
        let codec = if pt == PCMU_PAYLOAD_TYPE {
            // PCMU's payload type is statically assigned (RFC 3551 §6) —
            // recognized even without an explicit a=rtpmap line, same as a
            // real UA would.
            Some(NegotiatedCodec::Pcmu)
        } else if let Some((name, rate)) = rtpmap.get(&pt) {
            match (name.as_str(), *rate) {
                ("PCMU", 8000) => Some(NegotiatedCodec::Pcmu),
                ("AMR", 8000) => Some(NegotiatedCodec::AmrNb),
                ("AMR-WB", 16000) => Some(NegotiatedCodec::AmrWb),
                _ => None,
            }
        } else {
            None
        };
        if let Some(codec) = codec {
            offered.push(OfferedCodec {
                payload_type: pt,
                codec,
                fmtp: fmtp.get(&pt).cloned().unwrap_or_default(),
            });
        }
    }

    Ok(SdpOffer {
        remote_rtp: SocketAddr::new(conn_ip, rtp_port),
        offered,
    })
}

/// Build an SDP answer to `offer`, choosing exactly one codec: PCMU if the
/// offer included it (matches this project's Airtel-observed behavior and
/// avoids a 16k<->8k transcode on the bridge — see
/// `specs/011-vowifi-sip-bridge/research.md` item 3), otherwise AMR-WB if
/// the offer included it and `amr_available` is true (the caller's job to
/// pass `amr_safe::is_available()`), otherwise an error — an offer with no
/// codec we can both decode and that PJSIP's 8 kHz media path can carry
/// isn't answerable. Returns the SDP body and the codec it selected (so the
/// caller doesn't have to re-parse its own answer to know which one won).
/// Which codec of `offer` we'd answer with, if any — the single source of
/// truth for that decision, so a caller deciding *whether* to accept a call
/// (`ims::agent`) can't drift out of sync with `build_answer`, which decides
/// what to actually answer.
///
/// Preference order, best first:
///
/// 1. PCMU — relays straight through to Agent B's PJSIP leg, no transcode.
/// 2. AMR-WB — wideband, so transcoding to PCMU only loses the top half of the
///    band.
/// 3. AMR-NB — same 8kHz rate as PCMU; last resort, but a carrier may offer
///    nothing else (Airtel does, on some calls).
///
/// Within an AMR flavour, octet-aligned framing is preferred over
/// bandwidth-efficient purely because it's the simpler path; both are
/// supported (`ims::amr_rtp`). Crucially the framing is *read from the offer*,
/// never asserted — `octet-align` is declarative (RFC 4867 §8.1), so
/// answering a bandwidth-efficient payload type with `octet-align=1` is a
/// contradiction rather than a negotiation, and gets the call torn down.
pub fn select_codec(offer: &SdpOffer, amr_available: bool) -> Option<&OfferedCodec> {
    let pick = |codec: NegotiatedCodec| -> Option<&OfferedCodec> {
        if !amr_available && codec != NegotiatedCodec::Pcmu {
            return None;
        }
        let of_codec = || offer.offered.iter().filter(|c| c.codec == codec);
        of_codec()
            .find(|c| c.is_octet_aligned())
            .or_else(|| of_codec().next())
    };

    pick(NegotiatedCodec::Pcmu)
        .or_else(|| pick(NegotiatedCodec::AmrWb))
        .or_else(|| pick(NegotiatedCodec::AmrNb))
}

pub fn build_answer(
    local_ip: IpAddr,
    rtp_port: u16,
    session_id: u64,
    offer: &SdpOffer,
    amr_available: bool,
) -> BridgeResult<(String, ChosenCodec)> {
    // Preference order, best first:
    //   1. PCMU        — relays straight through to Agent B's PJSIP leg, no
    //                    transcode at all.
    //   2. AMR-WB      — wideband, so transcoding to PCMU only loses the
    //                    top half of the band.
    //   3. AMR-NB      — same 8kHz rate as PCMU; last resort, but a carrier
    //                    may offer nothing else.
    // Within each AMR flavour, octet-aligned framing is preferred over
    // bandwidth-efficient purely because it's the simpler path; both are
    // supported (see `ims::amr_rtp`). Crucially the framing is *read from the
    // offer*, never asserted — `octet-align` is declarative (RFC 4867 §8.1),
    // so answering a bandwidth-efficient payload type with `octet-align=1`
    // is a contradiction, not a negotiation, and gets the call torn down.
    let chosen = select_codec(offer, amr_available).ok_or_else(|| {
        BridgeError::Ims("SDP offer has no codec this client can answer with".into())
    })?;

    let addrtype = ip_addrtype(local_ip);
    // Echo the offer's own parameters for this payload type verbatim rather
    // than asserting our own — they describe how the *offerer* frames what it
    // sends, which is not ours to change.
    let rtpmap_line = match chosen.codec {
        NegotiatedCodec::Pcmu => format!("a=rtpmap:{} PCMU/8000\r\n", chosen.payload_type),
        NegotiatedCodec::AmrNb => format!(
            "a=rtpmap:{pt} AMR/8000\r\na=fmtp:{pt} {fmtp}\r\n",
            pt = chosen.payload_type,
            fmtp = chosen.fmtp,
        ),
        NegotiatedCodec::AmrWb => format!(
            "a=rtpmap:{pt} AMR-WB/16000\r\na=fmtp:{pt} {fmtp}\r\n",
            pt = chosen.payload_type,
            fmtp = chosen.fmtp,
        ),
    };

    let sdp = format!(
        "v=0\r\n\
         o=- {session_id} {session_id} IN {addrtype} {local_ip}\r\n\
         s=gsm-sip-bridge vowifi bridge\r\n\
         c=IN {addrtype} {local_ip}\r\n\
         t=0 0\r\n\
         m=audio {rtp_port} RTP/AVP {pt}\r\n\
         {rtpmap_line}\
         a=ptime:20\r\n\
         a=sendrecv\r\n",
        pt = chosen.payload_type,
    );

    Ok((
        sdp,
        ChosenCodec {
            codec: chosen.codec,
            payload_type: chosen.payload_type,
            octet_aligned: chosen.is_octet_aligned(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_offer_includes_pcmu_only_when_amr_wb_not_offered() {
        let sdp = build_offer("2402:8100::1".parse().unwrap(), 40000, 12345, false);
        assert!(sdp.contains("m=audio 40000 RTP/AVP 0\r\n"));
        assert!(sdp.contains("a=rtpmap:0 PCMU/8000"));
        assert!(!sdp.contains("AMR-WB"));
        assert!(sdp.contains("c=IN IP6 2402:8100::1"));
    }

    #[test]
    fn build_offer_includes_both_codecs_when_amr_wb_offered() {
        let sdp = build_offer("1.2.3.4".parse().unwrap(), 40000, 12345, true);
        assert!(sdp.contains("m=audio 40000 RTP/AVP 0 96\r\n"));
        assert!(sdp.contains("a=rtpmap:0 PCMU/8000"));
        assert!(sdp.contains("a=rtpmap:96 AMR-WB/16000"));
        assert!(sdp.contains("a=fmtp:96 octet-align=1"));
    }

    #[test]
    fn parse_answer_extracts_remote_rtp_and_recognizes_pcmu() {
        let body = "v=0\r\n\
                     o=- 1 1 IN IP4 5.6.7.8\r\n\
                     s=-\r\n\
                     c=IN IP4 5.6.7.8\r\n\
                     t=0 0\r\n\
                     m=audio 50000 RTP/AVP 0\r\n\
                     a=rtpmap:0 PCMU/8000\r\n";
        let answer = parse_answer(body).unwrap();
        assert_eq!(answer.remote_rtp, "5.6.7.8:50000".parse().unwrap());
        assert_eq!(answer.codec, NegotiatedCodec::Pcmu);
    }

    #[test]
    fn parse_answer_recognizes_amr_wb() {
        let body = "v=0\r\n\
                     c=IN IP4 5.6.7.8\r\n\
                     t=0 0\r\n\
                     m=audio 50000 RTP/AVP 96\r\n\
                     a=rtpmap:96 AMR-WB/16000\r\n\
                     a=fmtp:96 octet-align=1\r\n";
        let answer = parse_answer(body).unwrap();
        assert_eq!(answer.codec, NegotiatedCodec::AmrWb);
    }

    #[test]
    fn parse_answer_rejects_unrecognized_payload_type() {
        let body = "v=0\r\nc=IN IP4 5.6.7.8\r\nm=audio 50000 RTP/AVP 8\r\n";
        assert!(parse_answer(body).is_err());
    }

    #[test]
    fn parse_answer_rejects_missing_connection_line() {
        let body = "v=0\r\nm=audio 50000 RTP/AVP 0\r\n";
        assert!(parse_answer(body).is_err());
    }

    /// A realistic Airtel-shaped inbound INVITE offer: PCMU plus AMR-WB,
    /// PCMU listed first (matches how build_offer itself orders payload
    /// types, and how real VoWiFi/VoLTE offers were observed in
    /// ims::call's captured traces).
    const AIRTEL_LIKE_OFFER: &str = "v=0\r\n\
         o=- 1 1 IN IP4 10.0.0.5\r\n\
         s=-\r\n\
         c=IN IP4 10.0.0.5\r\n\
         t=0 0\r\n\
         m=audio 49170 RTP/AVP 0 96\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=rtpmap:96 AMR-WB/16000\r\n\
         a=fmtp:96 octet-align=1\r\n\
         a=sendrecv\r\n";

    #[test]
    fn parse_offer_extracts_remote_rtp_and_both_codecs_in_order() {
        let offer = parse_offer(AIRTEL_LIKE_OFFER).unwrap();
        assert_eq!(offer.remote_rtp, "10.0.0.5:49170".parse().unwrap());
        assert_eq!(offer.offered.len(), 2);
        assert_eq!(offer.offered[0].payload_type, 0);
        assert_eq!(offer.offered[0].codec, NegotiatedCodec::Pcmu);
        assert_eq!(offer.offered[1].payload_type, 96);
        assert_eq!(offer.offered[1].codec, NegotiatedCodec::AmrWb);
    }

    #[test]
    fn parse_offer_recognizes_pcmu_without_explicit_rtpmap() {
        // PCMU (payload type 0) is a statically assigned RFC 3551 type — a
        // real UA doesn't have to send a=rtpmap:0 for it.
        let body = "v=0\r\nc=IN IP4 10.0.0.5\r\nm=audio 49170 RTP/AVP 0\r\n";
        let offer = parse_offer(body).unwrap();
        assert_eq!(
            offer.offered,
            vec![OfferedCodec {
                payload_type: 0,
                codec: NegotiatedCodec::Pcmu,
                fmtp: String::new(),
            }]
        );
    }

    /// The real Airtel mobile-terminating offer: AMR-WB on *two* payload
    /// types, 104 bandwidth-efficient and 110 octet-aligned. We must answer
    /// on 110 — answering 104 with `octet-align=1` contradicts the offer and
    /// got the call BYE'd ~250ms after our 200 OK on a live call.
    #[test]
    fn build_answer_picks_the_octet_aligned_amr_wb_payload_type() {
        let body = "v=0\r\nc=IN IP6 2401:4900:c4:4062::14\r\n\
                     m=audio 5482 RTP/AVP 104 110 102\r\n\
                     a=rtpmap:104 AMR-WB/16000\r\n\
                     a=fmtp:104 mode-set=0,1,2,3; mode-change-capability=2; max-red=0\r\n\
                     a=rtpmap:110 AMR-WB/16000\r\n\
                     a=fmtp:110 octet-align=1; mode-set=0,1,2,3; mode-change-capability=2; max-red=0\r\n\
                     a=rtpmap:102 AMR/8000\r\n";
        let offer = parse_offer(body).unwrap();

        let (sdp, codec) =
            build_answer("2401:4900:1::2".parse().unwrap(), 40000, 1, &offer, true).unwrap();
        assert_eq!(codec.codec, NegotiatedCodec::AmrWb);
        assert!(
            sdp.contains("m=audio 40000 RTP/AVP 110\r\n"),
            "must answer on the octet-aligned payload type, got:\n{sdp}"
        );
        // The offer's own parameters, echoed rather than invented.
        assert!(sdp.contains(
            "a=fmtp:110 octet-align=1; mode-set=0,1,2,3; mode-change-capability=2; max-red=0\r\n"
        ));
        assert!(
            !sdp.contains("104"),
            "must not answer on the bandwidth-efficient type"
        );
    }

    /// An AMR-WB offer with no `octet-align=1` is bandwidth-efficient. That is
    /// answerable (`ims::amr_rtp` frames both), but the answer must *not*
    /// claim octet-alignment, and the media path must be told which framing it
    /// is committed to.
    #[test]
    fn build_answer_accepts_bandwidth_efficient_amr_without_claiming_octet_align() {
        let body = "v=0\r\nc=IN IP6 2401:4900:c4:4062::14\r\n\
                     m=audio 5482 RTP/AVP 104\r\n\
                     a=rtpmap:104 AMR-WB/16000\r\n\
                     a=fmtp:104 mode-set=0,1,2,3; max-red=0\r\n";
        let offer = parse_offer(body).unwrap();
        let (sdp, chosen) =
            build_answer("2401:4900:1::2".parse().unwrap(), 40000, 1, &offer, true).unwrap();
        assert_eq!(chosen.codec, NegotiatedCodec::AmrWb);
        assert_eq!(chosen.payload_type, 104);
        assert!(!chosen.octet_aligned, "offer never declared octet-align");
        assert!(
            !sdp.contains("octet-align"),
            "answer must not assert a framing the offer didn't declare:\n{sdp}"
        );
    }

    /// The real Airtel narrowband-only offer: `AMR/8000` and nothing else, no
    /// PCMU, no AMR-WB, and bandwidth-efficient on every payload type. This is
    /// the offer that was being declined outright.
    #[test]
    fn build_answer_handles_a_narrowband_only_bandwidth_efficient_offer() {
        let body = "v=0\r\nc=IN IP6 2401:4900:c4:4062::14\r\n\
                     m=audio 30870 RTP/AVP 108 100 116\r\n\
                     a=rtpmap:108 AMR/8000\r\n\
                     a=fmtp:108 mode-set=0,2,4,7; mode-change-period=2; max-red=0\r\n\
                     a=rtpmap:100 AMR/8000\r\n\
                     a=fmtp:100 max-red=0\r\n\
                     a=rtpmap:116 telephone-event/8000\r\n";
        let offer = parse_offer(body).unwrap();
        let (sdp, chosen) =
            build_answer("2401:4900:1::2".parse().unwrap(), 40000, 1, &offer, true).unwrap();
        assert_eq!(chosen.codec, NegotiatedCodec::AmrNb);
        assert_eq!(chosen.payload_type, 108, "first listed AMR-NB payload type");
        assert!(!chosen.octet_aligned);
        assert!(sdp.contains("m=audio 40000 RTP/AVP 108\r\n"));
        assert!(sdp.contains("a=rtpmap:108 AMR/8000\r\n"));
        // The offer's own parameters, echoed.
        assert!(sdp.contains("a=fmtp:108 mode-set=0,2,4,7; mode-change-period=2; max-red=0\r\n"));
    }

    /// Without a linked AMR codec there is genuinely nothing to answer such an
    /// offer with — decline rather than answer with a codec we can't encode.
    #[test]
    fn build_answer_declines_an_amr_only_offer_when_amr_is_not_linked() {
        let body = "v=0\r\nc=IN IP6 2401:4900:c4:4062::14\r\n\
                     m=audio 30870 RTP/AVP 108\r\n\
                     a=rtpmap:108 AMR/8000\r\n";
        let offer = parse_offer(body).unwrap();
        assert!(build_answer("2401:4900:1::2".parse().unwrap(), 40000, 1, &offer, false).is_err());
    }

    #[test]
    fn parse_offer_omits_unrecognized_codecs_without_erroring() {
        // GSM/EFR (payload type 3) alongside PCMU — should just skip the
        // one we don't recognize rather than failing the whole offer.
        let body = "v=0\r\nc=IN IP4 10.0.0.5\r\nm=audio 49170 RTP/AVP 0 3\r\n\
                     a=rtpmap:3 GSM/8000\r\n";
        let offer = parse_offer(body).unwrap();
        assert_eq!(offer.offered.len(), 1);
        assert_eq!(offer.offered[0].codec, NegotiatedCodec::Pcmu);
    }

    #[test]
    fn parse_offer_rejects_missing_connection_line() {
        let body = "v=0\r\nm=audio 50000 RTP/AVP 0\r\n";
        assert!(parse_offer(body).is_err());
    }

    #[test]
    fn build_answer_prefers_pcmu_when_offered() {
        let offer = parse_offer(AIRTEL_LIKE_OFFER).unwrap();
        let (sdp, codec) =
            build_answer("1.2.3.4".parse().unwrap(), 40000, 999, &offer, true).unwrap();
        assert_eq!(codec.codec, NegotiatedCodec::Pcmu);
        assert!(sdp.contains("m=audio 40000 RTP/AVP 0\r\n"));
        assert!(sdp.contains("a=rtpmap:0 PCMU/8000"));
        assert!(!sdp.contains("AMR-WB"));
    }

    #[test]
    fn build_answer_falls_back_to_amr_wb_when_pcmu_absent_and_amr_available() {
        // `octet-align=1` is required for the offer to be answerable at all —
        // it is the only AMR-WB framing this client can produce or consume.
        let body = "v=0\r\nc=IN IP4 10.0.0.5\r\nm=audio 49170 RTP/AVP 97\r\n\
                     a=rtpmap:97 AMR-WB/16000\r\na=fmtp:97 octet-align=1\r\n";
        let offer = parse_offer(body).unwrap();
        let (sdp, codec) =
            build_answer("1.2.3.4".parse().unwrap(), 40000, 999, &offer, true).unwrap();
        assert_eq!(codec.codec, NegotiatedCodec::AmrWb);
        // Echoes the offer's own payload type (97), not the hardcoded 96.
        assert!(sdp.contains("m=audio 40000 RTP/AVP 97\r\n"));
        assert!(sdp.contains("a=rtpmap:97 AMR-WB/16000"));
    }

    #[test]
    fn build_answer_errors_when_amr_wb_only_offer_and_amr_unavailable() {
        let body = "v=0\r\nc=IN IP4 10.0.0.5\r\nm=audio 49170 RTP/AVP 96\r\n\
                     a=rtpmap:96 AMR-WB/16000\r\n";
        let offer = parse_offer(body).unwrap();
        let result = build_answer("1.2.3.4".parse().unwrap(), 40000, 999, &offer, false);
        assert!(result.is_err());
    }

    #[test]
    fn build_answer_errors_when_offer_has_no_recognized_codec() {
        let body = "v=0\r\nc=IN IP4 10.0.0.5\r\nm=audio 49170 RTP/AVP 3\r\n\
                     a=rtpmap:3 GSM/8000\r\n";
        let offer = parse_offer(body).unwrap();
        let result = build_answer("1.2.3.4".parse().unwrap(), 40000, 999, &offer, true);
        assert!(result.is_err());
    }
}
