//! SDP offer/answer for siptest's own payload types. `ims::sdp` cannot be
//! reused: `build_offer` only emits PCMU/AMR-NB/AMR-WB/L16, and `parse_answer`
//! hard-rejects any payload type other than 0 and 96 — G.722's PT 9 would
//! error out (research.md R5).

use std::net::IpAddr;

use crate::error::{SipTestError, SipTestResult};
use crate::media::codec::CodecProfile;

pub fn build_offer(
    local_ip: IpAddr,
    rtp_port: u16,
    session_id: u64,
    codec: CodecProfile,
) -> String {
    let addrtype = if local_ip.is_ipv6() { "IP6" } else { "IP4" };
    format!(
        "v=0\r\n\
         o=- {sid} {sid} IN {addrtype} {ip}\r\n\
         s=siptest\r\n\
         c=IN {addrtype} {ip}\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP {pt} 101\r\n\
         a=rtpmap:{pt} {rtpmap}\r\n\
         a=rtpmap:101 telephone-event/8000\r\n\
         a=fmtp:101 0-15\r\n\
         a=ptime:20\r\n\
         a=sendrecv\r\n",
        sid = session_id,
        addrtype = addrtype,
        ip = local_ip,
        port = rtp_port,
        pt = codec.pt,
        rtpmap = codec.rtpmap,
    )
}

#[derive(Debug, Clone)]
pub struct SdpAnswer {
    pub remote_rtp: std::net::SocketAddr,
    pub payload_type: u8,
}

/// Parses an SDP answer down to the connection address, the `m=audio` port,
/// and the selected payload type. Explicit failures for `c=0.0.0.0`,
/// `m=audio 0`, and `a=inactive` — an answer meaning "no media" must be
/// reported as a failure, not silently accepted as if it were ordinary.
pub fn parse_answer(body: &str) -> SipTestResult<SdpAnswer> {
    let mut conn_ip: Option<IpAddr> = None;
    let mut rtp_port: Option<u16> = None;
    let mut payload_type: Option<u8> = None;
    let mut inactive = false;

    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("c=IN ") {
            if let Some(addr) = rest
                .split_whitespace()
                .nth(1)
                .and_then(|a| a.parse::<IpAddr>().ok())
            {
                conn_ip = Some(addr);
            }
        } else if let Some(rest) = line.strip_prefix("m=audio ") {
            let mut fields = rest.split_whitespace();
            rtp_port = fields.next().and_then(|p| p.parse().ok());
            payload_type = fields.nth(1).and_then(|pt| pt.parse().ok());
        } else if line == "a=inactive" {
            inactive = true;
        }
    }

    if inactive {
        return Err(SipTestError::Config("SDP answer marked a=inactive".into()));
    }
    let conn_ip =
        conn_ip.ok_or_else(|| SipTestError::Config("SDP answer missing c= address".into()))?;
    if conn_ip.is_unspecified() {
        return Err(SipTestError::Config(
            "SDP answer's c= address is 0.0.0.0".into(),
        ));
    }
    let rtp_port =
        rtp_port.ok_or_else(|| SipTestError::Config("SDP answer missing m=audio port".into()))?;
    if rtp_port == 0 {
        return Err(SipTestError::Config(
            "SDP answer's m=audio port is 0".into(),
        ));
    }
    let payload_type = payload_type
        .ok_or_else(|| SipTestError::Config("SDP answer has no payload type".into()))?;

    Ok(SdpAnswer {
        remote_rtp: std::net::SocketAddr::new(conn_ip, rtp_port),
        payload_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::codec::PCMU;

    #[test]
    fn offer_lists_pcmu_and_telephone_event() {
        let offer = build_offer("192.168.15.10".parse().unwrap(), 40000, 1, PCMU);
        assert!(offer.contains("m=audio 40000 RTP/AVP 0 101"));
        assert!(offer.contains("a=rtpmap:0 PCMU/8000"));
        assert!(offer.contains("a=rtpmap:101 telephone-event/8000"));
    }

    #[test]
    fn answer_selecting_pcmu_parses() {
        let body = "v=0\r\no=- 1 1 IN IP4 192.168.15.10\r\ns=-\r\nc=IN IP4 192.168.15.10\r\nt=0 0\r\nm=audio 41000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
        let answer = parse_answer(body).unwrap();
        assert_eq!(answer.payload_type, 0);
        assert_eq!(answer.remote_rtp.port(), 41000);
    }

    #[test]
    fn zero_connection_address_is_rejected() {
        let body = "v=0\r\nc=IN IP4 0.0.0.0\r\nm=audio 41000 RTP/AVP 0\r\n";
        assert!(parse_answer(body).is_err());
    }

    #[test]
    fn zero_port_is_rejected() {
        let body = "v=0\r\nc=IN IP4 192.168.15.10\r\nm=audio 0 RTP/AVP 0\r\n";
        assert!(parse_answer(body).is_err());
    }

    #[test]
    fn inactive_answer_is_rejected() {
        let body = "v=0\r\nc=IN IP4 192.168.15.10\r\nm=audio 41000 RTP/AVP 0\r\na=inactive\r\n";
        assert!(parse_answer(body).is_err());
    }
}
