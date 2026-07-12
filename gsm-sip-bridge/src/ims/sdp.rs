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
    AmrWb,
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
}
