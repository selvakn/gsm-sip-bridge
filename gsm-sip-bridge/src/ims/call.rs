//! Places one call over an already-registered Gm-protected IMS session
//! (reusing `super::register_session`) and records the received audio to a
//! WAV file. Offers two codecs — G.711 μ-law (PCMU, see `super::rtp`) and,
//! when linked (`amr-linked` feature — see `amr-safe`), AMR-WB — since a
//! live test call against Airtel found the network rejects a PCMU-only
//! offer outright (`488 Not Acceptable Here`): VoWiFi/VoLTE mandates AMR-WB
//! and most networks won't fall back to G.711.
//!
//! This is deliberately not a general SIP dialog implementation: no
//! re-INVITE, no mid-call target refresh, no PRACK/100rel. Just enough to
//! place a call, exchange audio for a fixed duration, and hang up.

use super::sdp::NegotiatedCodec;
use super::sip_client::{format_sip_addr, random_hex};
use super::{sdp, ImsRegisterConfig};
use crate::error::{BridgeError, BridgeResult};
use std::io;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RTP_TIMEOUT: Duration = Duration::from_millis(200);
const PCMU_PAYLOAD_TYPE: u8 = 0;
/// Must match `sdp::AMR_WB_PAYLOAD_TYPE` (not exported — the two modules
/// agree on this by both being fed the same negotiated `NegotiatedCodec`
/// rather than by sharing the numeric constant directly).
const AMR_WB_RTP_PAYLOAD_TYPE: u8 = 96;

/// Per-codec framing parameters needed to run the RTP send/receive loop —
/// resolved once from the negotiated `NegotiatedCodec` so the loop itself
/// doesn't need to match on the codec per-packet.
struct CodecParams {
    samples_per_packet: usize,
    sample_rate: u32,
    rtp_payload_type: u8,
}

impl CodecParams {
    fn for_codec(codec: NegotiatedCodec) -> Self {
        match codec {
            NegotiatedCodec::Pcmu => Self {
                // 20ms @ 8kHz (G.711's rate) — the conventional RTP audio
                // packetization interval.
                samples_per_packet: 160,
                sample_rate: 8000,
                rtp_payload_type: PCMU_PAYLOAD_TYPE,
            },
            NegotiatedCodec::AmrWb => Self {
                samples_per_packet: amr_safe::FRAME_SAMPLES,
                sample_rate: amr_safe::SAMPLE_RATE,
                rtp_payload_type: AMR_WB_RTP_PAYLOAD_TYPE,
            },
        }
    }
}

pub struct CallConfig {
    pub register: ImsRegisterConfig,
    /// E.164, e.g. +919789063708.
    pub callee: String,
    pub record_path: PathBuf,
    /// How long to wait for the callee to answer before giving up.
    pub ring_timeout: Duration,
    /// How long to hold the call open (exchanging audio) once answered.
    pub call_duration: Duration,
}

#[derive(Debug)]
pub enum CallOutcome {
    Answered {
        recorded_path: PathBuf,
        recorded_samples: u32,
    },
    NotAnswered {
        status: u16,
        reason: String,
    },
}

pub fn run_call(cfg: &CallConfig) -> BridgeResult<CallOutcome> {
    let mut session = super::register_session(&cfg.register)?;
    if session.status != 200 {
        let status = session.status;
        let reason = session.reason.clone();
        session.cleanup();
        return Err(BridgeError::Ims(format!(
            "registration failed before a call could be attempted: {status} {reason}"
        )));
    }
    tracing::info!("registered — placing call");

    // RFC 3608: subsequent requests within this registration's association
    // must route via the Service-Route set the registrar returned, in order.
    let route_headers: Vec<String> = session
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("Service-Route"))
        .map(|(_, v)| format!("Route: {v}"))
        .collect();

    let rtp_socket = UdpSocket::bind((session.local_addr.ip(), 0))
        .map_err(|e| BridgeError::Ims(format!("RTP socket bind failed: {e}")))?;
    let rtp_port = rtp_socket
        .local_addr()
        .map_err(|e| BridgeError::Ims(format!("RTP local_addr failed: {e}")))?
        .port();

    let session_id: u64 = rand::random::<u32>() as u64;
    let offer = sdp::build_offer(
        session.local_addr.ip(),
        rtp_port,
        session_id,
        amr_safe::is_available(),
    );

    // `;user=phone` (RFC 3261 §19.1.1 / TS 24.229) tells the network this is
    // a PSTN/mobile number, not a resolvable SIP address — a bare sip: URI
    // reached a terminating application server that never rang the callee
    // and gave up after ~23s with 487, twice, on real test calls.
    let callee_uri = format!("{}@{};user=phone", cfg.callee, session.realm);
    let call_id = random_hex(8);
    let from_tag = random_hex(4);
    let invite_cseq = session.cseq;
    let via_transport = if session.use_tcp { "TCP" } else { "UDP" };
    let branch = format!("z9hG4bK{}", random_hex(6));

    let invite = build_invite(&InviteParts {
        request_uri: &callee_uri,
        route_headers: &route_headers,
        via_transport,
        local_addr: session.local_addr,
        public_uri: &session.public_uri,
        callee_uri: &callee_uri,
        call_id: &call_id,
        from_tag: &from_tag,
        cseq: invite_cseq,
        branch: &branch,
        body: &offer,
    });

    tracing::info!(callee = %cfg.callee, "sending INVITE");
    session.transport.send(&invite)?;
    let resp = session.transport.recv_final_response(cfg.ring_timeout)?;
    tracing::info!(status = resp.status, reason = %resp.reason, "final INVITE response");

    if resp.status != 200 {
        // Non-2xx final response to INVITE: ACK reuses the INVITE's own
        // branch/CSeq (RFC 3261 §17.1.1.3) rather than being a new
        // transaction — best-effort, errors here don't change the outcome.
        let ack = build_ack(&AckParts {
            request_uri: &callee_uri,
            route_headers: &route_headers,
            via_transport,
            local_addr: session.local_addr,
            public_uri: &session.public_uri,
            to_header: resp.header("To").unwrap_or(&callee_uri),
            call_id: &call_id,
            from_tag: &from_tag,
            cseq: invite_cseq,
            branch: &branch,
        });
        let _ = session.transport.send(&ack);
        session.cleanup();
        return Ok(CallOutcome::NotAnswered {
            status: resp.status,
            reason: resp.reason,
        });
    }

    let to_header = resp
        .header("To")
        .ok_or_else(|| BridgeError::Ims("200 OK to INVITE missing To header".into()))?
        .to_string();
    let answer = sdp::parse_answer(&resp.body)?;
    tracing::info!(remote_rtp = %answer.remote_rtp, codec = ?answer.codec, "call answered, starting RTP");

    let ack_branch = format!("z9hG4bK{}", random_hex(6));
    let ack = build_ack(&AckParts {
        request_uri: &callee_uri,
        route_headers: &route_headers,
        via_transport,
        local_addr: session.local_addr,
        public_uri: &session.public_uri,
        to_header: &to_header,
        call_id: &call_id,
        from_tag: &from_tag,
        cseq: invite_cseq,
        branch: &ack_branch,
    });
    session.transport.send(&ack)?;

    let recorded_samples = run_rtp_session(&rtp_socket, answer.remote_rtp, answer.codec, cfg)?;

    let bye_branch = format!("z9hG4bK{}", random_hex(6));
    let bye = build_bye(&AckParts {
        request_uri: &callee_uri,
        route_headers: &route_headers,
        via_transport,
        local_addr: session.local_addr,
        public_uri: &session.public_uri,
        to_header: &to_header,
        call_id: &call_id,
        from_tag: &from_tag,
        cseq: invite_cseq + 1,
        branch: &bye_branch,
    });
    // Best-effort — the recording already happened; a BYE-send failure
    // shouldn't turn a successful call test into an error.
    if let Err(e) = session.transport.send(&bye) {
        tracing::warn!(error = %e, "failed to send BYE");
    } else if let Ok(resp) = session.transport.recv_response() {
        tracing::info!(status = resp.status, reason = %resp.reason, "BYE response");
    }

    session.cleanup();
    Ok(CallOutcome::Answered {
        recorded_path: cfg.record_path.clone(),
        recorded_samples,
    })
}

/// Sends a looping tone as outgoing audio and records whatever comes back,
/// for `cfg.call_duration`. Runs the receive side on a background thread
/// (sharing the same connected UDP socket via `try_clone`) so sending stays
/// on a steady 20ms packetization clock regardless of how much incoming
/// audio there is to drain. `codec` picks the framing/encode/decode used on
/// both sides — see `CodecParams::for_codec`.
fn run_rtp_session(
    rtp_socket: &UdpSocket,
    remote_rtp: std::net::SocketAddr,
    codec: NegotiatedCodec,
    cfg: &CallConfig,
) -> BridgeResult<u32> {
    rtp_socket
        .connect(remote_rtp)
        .map_err(|e| BridgeError::Ims(format!("RTP connect to {remote_rtp} failed: {e}")))?;

    let recv_socket = rtp_socket
        .try_clone()
        .map_err(|e| BridgeError::Ims(format!("RTP socket clone failed: {e}")))?;
    recv_socket
        .set_read_timeout(Some(RTP_TIMEOUT))
        .map_err(|e| BridgeError::Ims(format!("RTP set_read_timeout failed: {e}")))?;

    let params = CodecParams::for_codec(codec);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_recv = stop.clone();
    let record_path = cfg.record_path.clone();
    let sample_rate = params.sample_rate;
    let recv_handle = std::thread::spawn(move || -> BridgeResult<u32> {
        let mut wav = super::rtp::WavWriter::create(&record_path, sample_rate)?;
        // Constructed once per call (not per packet) since it's stateful —
        // AMR-WB decoding carries filter/predictor history across frames.
        let mut amr_decoder = match codec {
            NegotiatedCodec::AmrWb => Some(
                amr_safe::WbDecoder::new()
                    .map_err(|e| BridgeError::Ims(format!("AMR-WB decoder init failed: {e}")))?,
            ),
            NegotiatedCodec::Pcmu => None,
        };
        let mut buf = [0u8; 2048];
        while !stop_recv.load(Ordering::Relaxed) {
            match recv_socket.recv(&mut buf) {
                Ok(n) => {
                    let Some(pkt) = super::rtp::parse_packet(&buf[..n]) else {
                        continue;
                    };
                    let samples: Vec<i16> = match codec {
                        NegotiatedCodec::Pcmu => pkt
                            .payload
                            .iter()
                            .map(|&b| super::rtp::ulaw_to_linear(b))
                            .collect(),
                        NegotiatedCodec::AmrWb => {
                            // RFC 4867 §4.3.1 octet-aligned payload: 1 CMR
                            // byte, then (for our single-frame-per-packet
                            // design) exactly one ToC byte + frame data —
                            // which is bit-for-bit what D_IF_decode expects.
                            if pkt.payload.len() < 2 {
                                continue;
                            }
                            amr_decoder
                                .as_mut()
                                .expect("amr_decoder is Some when codec is AmrWb")
                                .decode(&pkt.payload[1..])
                                .to_vec()
                        }
                    };
                    wav.write_samples(&samples)?;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    continue
                }
                Err(e) => return Err(BridgeError::Ims(format!("RTP recv failed: {e}"))),
            }
        }
        let samples_written = wav.samples_written();
        wav.finish()?;
        Ok(samples_written)
    });

    // Same reasoning as the decoder above: one stateful encoder for the
    // whole call, not per packet.
    let mut amr_encoder = match codec {
        NegotiatedCodec::AmrWb => Some(
            amr_safe::WbEncoder::new()
                .map_err(|e| BridgeError::Ims(format!("AMR-WB encoder init failed: {e}")))?,
        ),
        NegotiatedCodec::Pcmu => None,
    };

    let ssrc: u32 = rand::random();
    let mut seq: u16 = rand::random();
    let mut timestamp: u32 = 0;
    let mut sample_index: u64 = 0;
    let tone_freq = 440.0;
    let start = Instant::now();

    while start.elapsed() < cfg.call_duration {
        let mut pcm = Vec::with_capacity(params.samples_per_packet);
        for i in 0..params.samples_per_packet {
            let t = (sample_index + i as u64) as f64 / sample_rate as f64;
            let sample =
                (0.3 * (2.0 * std::f64::consts::PI * tone_freq * t).sin() * i16::MAX as f64) as i16;
            pcm.push(sample);
        }
        sample_index += params.samples_per_packet as u64;

        let rtp_payload = match codec {
            NegotiatedCodec::Pcmu => pcm.iter().map(|&s| super::rtp::linear_to_ulaw(s)).collect(),
            NegotiatedCodec::AmrWb => {
                let pcm_frame: [i16; amr_safe::FRAME_SAMPLES] = pcm
                    .as_slice()
                    .try_into()
                    .expect("pcm has exactly FRAME_SAMPLES elements for the AmrWb codec branch");
                let encoded = amr_encoder
                    .as_mut()
                    .expect("amr_encoder is Some when codec is AmrWb")
                    // Mode 2 (12.65kbps) — a common VoLTE/VoWiFi default;
                    // this client doesn't implement adaptive mode selection
                    // in response to the network's CMR requests.
                    .encode(amr_safe::Mode::R1265, &pcm_frame);
                // RFC 4867 §4.3.1: 1 CMR byte (0xF0 = value 15, "no mode
                // request") followed by the ToC+data `encoded` already is.
                let mut payload = Vec::with_capacity(1 + encoded.len());
                payload.push(0xF0);
                payload.extend_from_slice(&encoded);
                payload
            }
        };

        let pkt =
            super::rtp::build_packet(seq, timestamp, ssrc, params.rtp_payload_type, &rtp_payload);
        rtp_socket
            .send(&pkt)
            .map_err(|e| BridgeError::Ims(format!("RTP send failed: {e}")))?;
        seq = seq.wrapping_add(1);
        timestamp = timestamp.wrapping_add(params.samples_per_packet as u32);

        std::thread::sleep(Duration::from_millis(20));
    }

    stop.store(true, Ordering::Relaxed);
    recv_handle
        .join()
        .map_err(|_| BridgeError::Ims("RTP receive thread panicked".into()))?
}

struct InviteParts<'a> {
    request_uri: &'a str,
    route_headers: &'a [String],
    via_transport: &'a str,
    local_addr: std::net::SocketAddr,
    public_uri: &'a str,
    callee_uri: &'a str,
    call_id: &'a str,
    from_tag: &'a str,
    cseq: u32,
    branch: &'a str,
    body: &'a str,
}

fn build_invite(p: &InviteParts) -> String {
    let via_addr = format_sip_addr(p.local_addr);
    let public_user = p.public_uri.split('@').next().unwrap_or(p.public_uri);
    let mut msg = format!(
        "INVITE sip:{request_uri} SIP/2.0\r\n\
         Via: SIP/2.0/{transport} {via_addr};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n",
        request_uri = p.request_uri,
        transport = p.via_transport,
        via_addr = via_addr,
        branch = p.branch,
    );
    for route in p.route_headers {
        msg.push_str(route);
        msg.push_str("\r\n");
    }
    msg.push_str(&format!(
        "From: <sip:{public_uri}>;tag={from_tag}\r\n\
         To: <sip:{callee_uri}>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} INVITE\r\n\
         Contact: <sip:{public_user}@{via_addr};transport={transport}>\r\n\
         Allow: INVITE, ACK, BYE, CANCEL, OPTIONS\r\n\
         P-Access-Network-Info: 3GPP-WLAN\r\n\
         User-Agent: motorola_XT2241-1_Android15_V1SQS35H.58-10-8-9\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {body_len}\r\n\r\n\
         {body}",
        public_uri = p.public_uri,
        from_tag = p.from_tag,
        callee_uri = p.callee_uri,
        call_id = p.call_id,
        cseq = p.cseq,
        public_user = public_user,
        via_addr = via_addr,
        transport = p.via_transport,
        body_len = p.body.len(),
        body = p.body,
    ));
    msg
}

struct AckParts<'a> {
    request_uri: &'a str,
    route_headers: &'a [String],
    via_transport: &'a str,
    local_addr: std::net::SocketAddr,
    public_uri: &'a str,
    to_header: &'a str,
    call_id: &'a str,
    from_tag: &'a str,
    cseq: u32,
    branch: &'a str,
}

fn build_ack(p: &AckParts) -> String {
    build_in_dialog_request("ACK", p)
}

fn build_bye(p: &AckParts) -> String {
    build_in_dialog_request("BYE", p)
}

fn build_in_dialog_request(method: &str, p: &AckParts) -> String {
    let via_addr = format_sip_addr(p.local_addr);
    let mut msg = format!(
        "{method} sip:{request_uri} SIP/2.0\r\n\
         Via: SIP/2.0/{transport} {via_addr};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n",
        method = method,
        request_uri = p.request_uri,
        transport = p.via_transport,
        via_addr = via_addr,
        branch = p.branch,
    );
    for route in p.route_headers {
        msg.push_str(route);
        msg.push_str("\r\n");
    }
    msg.push_str(&format!(
        "From: <sip:{public_uri}>;tag={from_tag}\r\n\
         To: {to_header}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} {method}\r\n\
         Content-Length: 0\r\n\r\n",
        public_uri = p.public_uri,
        from_tag = p.from_tag,
        to_header = p.to_header,
        call_id = p.call_id,
        cseq = p.cseq,
        method = method,
    ));
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_invite_includes_sdp_body_and_content_length() {
        let addr: std::net::SocketAddr = "1.2.3.4:5060".parse().unwrap();
        let msg = build_invite(&InviteParts {
            request_uri: "+919789063708@realm",
            route_headers: &[],
            via_transport: "TCP",
            local_addr: addr,
            public_uri: "12345@realm",
            callee_uri: "+919789063708@realm",
            call_id: "callid",
            from_tag: "tag1",
            cseq: 1,
            branch: "branch1",
            body: "v=0\r\n",
        });
        assert!(msg.starts_with("INVITE sip:+919789063708@realm SIP/2.0\r\n"));
        assert!(msg.contains("Content-Length: 5\r\n"));
        assert!(msg.ends_with("v=0\r\n"));
        assert!(msg.contains("CSeq: 1 INVITE"));
    }

    #[test]
    fn build_invite_includes_route_headers_in_order() {
        let addr: std::net::SocketAddr = "1.2.3.4:5060".parse().unwrap();
        let routes = vec!["Route: <sip:a>".to_string(), "Route: <sip:b>".to_string()];
        let msg = build_invite(&InviteParts {
            request_uri: "x@realm",
            route_headers: &routes,
            via_transport: "TCP",
            local_addr: addr,
            public_uri: "u@realm",
            callee_uri: "x@realm",
            call_id: "c",
            from_tag: "f",
            cseq: 1,
            branch: "b",
            body: "",
        });
        let a_pos = msg.find("Route: <sip:a>").unwrap();
        let b_pos = msg.find("Route: <sip:b>").unwrap();
        assert!(a_pos < b_pos);
    }

    #[test]
    fn build_bye_reuses_to_header_verbatim() {
        let addr: std::net::SocketAddr = "1.2.3.4:5060".parse().unwrap();
        let msg = build_bye(&AckParts {
            request_uri: "x@realm",
            route_headers: &[],
            via_transport: "TCP",
            local_addr: addr,
            public_uri: "u@realm",
            to_header: "<sip:x@realm>;tag=abc123",
            call_id: "c",
            from_tag: "f",
            cseq: 2,
            branch: "b",
        });
        assert!(msg.starts_with("BYE sip:x@realm SIP/2.0\r\n"));
        assert!(msg.contains("To: <sip:x@realm>;tag=abc123\r\n"));
        assert!(msg.contains("CSeq: 2 BYE"));
    }
}
