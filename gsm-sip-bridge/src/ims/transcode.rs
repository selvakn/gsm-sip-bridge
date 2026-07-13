//! AMR (narrowband or wideband) <-> PCMU transcoding RTP relay, for bridging
//! an inbound VoWiFi call whose carrier-side leg speaks AMR to Agent B's
//! PJSIP leg, which is fixed at PCMU/8000.
//!
//! `ims::agent`'s plain `relay_rtp` forwards RTP packets untouched, which is
//! all that's needed when both legs agreed on PCMU. But a real carrier
//! commonly offers **no PCMU at all** on a mobile-terminating VoWiFi INVITE
//! — Airtel was observed offering AMR-WB+AMR-NB on some calls and AMR-NB
//! alone on others — so without transcoding those calls can only be declined.
//! This module terminates the codec on each side and re-encodes, rather than
//! relaying opaque payloads.
//!
//! Narrowband needs no resampling (it is already 8kHz, like PCMU); wideband
//! is 16kHz and does. RTP framing for either flavour is handled by
//! `super::amr_rtp`, which supports both the octet-aligned and the
//! bandwidth-efficient payload formats — the carrier's offer decides which,
//! and it is not ours to choose.

use super::amr_rtp::{self, AmrKind};
use super::rtp;
use super::sdp::{ChosenCodec, NegotiatedCodec};
use crate::error::{BridgeError, BridgeResult};
use std::io;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Payload type for PCMU — static, assigned by RFC 3551, so it is the same
/// number on every call and safe to hardcode. (An AMR payload type is
/// *dynamic* and must always come from the offer.)
const PCMU_PAYLOAD_TYPE: u8 = 0;
/// 20ms at 8kHz — one PCMU packet, one AMR-NB frame, half an AMR-WB frame.
const PCMU_FRAME_SAMPLES: usize = 160;
/// How often a blocking `recv` wakes to re-check `stop`, bounding how
/// quickly a hangup silences the relay. Matches `agent::RELAY_POLL_INTERVAL`.
const RELAY_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// The rates we encode at, one per flavour. Both are the usual VoLTE
/// operating points and sit inside the `mode-set` carriers advertise. The far
/// end signals its own mode per-frame and we decode whatever it sends, so
/// these only govern our own encoding.
const WB_ENCODE_MODE: amr_safe::Mode = amr_safe::Mode::R1265;
const NB_ENCODE_MODE: amr_safe::NbMode = amr_safe::NbMode::R1220;

/// Halves the sample rate with a 3-tap `(1,2,1)/4` FIR to suppress the
/// aliasing a bare drop-every-other-sample would fold back into the band —
/// AMR-WB carries real energy above 4kHz, so decimating without it makes
/// speech sound harsh and metallic rather than merely dull.
#[derive(Default)]
struct Downsampler {
    /// The last input sample of the previous block, so the filter window can
    /// straddle block boundaries instead of resetting (which would tick
    /// audibly every 20ms).
    prev: i16,
}

impl Downsampler {
    fn process(&mut self, input: &[i16]) -> Vec<i16> {
        let mut out = Vec::with_capacity(input.len() / 2);
        for pair in input.chunks_exact(2) {
            let (a, b) = (pair[0], pair[1]);
            let filtered = (self.prev as i32 + 2 * a as i32 + b as i32) / 4;
            out.push(filtered.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
            self.prev = b;
        }
        out
    }
}

/// Doubles the sample rate by linear interpolation. Cheap, and adequate
/// here: the source is 8kHz telephony audio, so there is genuinely no
/// content above 4kHz to reconstruct — a fancier filter would invent detail
/// that was never in the signal.
#[derive(Default)]
struct Upsampler {
    prev: i16,
}

impl Upsampler {
    fn process(&mut self, input: &[i16]) -> Vec<i16> {
        let mut out = Vec::with_capacity(input.len() * 2);
        for &s in input {
            out.push(((self.prev as i32 + s as i32) / 2) as i16);
            out.push(s);
            self.prev = s;
        }
        out
    }
}

/// The carrier leg's codec, resolved from the SDP answer — which AMR flavour,
/// on which (dynamic) payload type, in which RTP framing.
#[derive(Debug, Clone, Copy)]
struct AmrLeg {
    kind: AmrKind,
    payload_type: u8,
    octet_aligned: bool,
}

impl AmrLeg {
    /// Samples in one 20ms frame at this flavour's own rate.
    fn frame_samples(&self) -> usize {
        match self.kind {
            AmrKind::Nb => amr_safe::NB_FRAME_SAMPLES,
            AmrKind::Wb => amr_safe::FRAME_SAMPLES,
        }
    }
}

/// Either flavour's decoder. An enum rather than a trait object: there are
/// exactly two, they are chosen once per call, and the frame sizes differ.
enum AmrDecoder {
    Nb(amr_safe::NbDecoder),
    Wb(amr_safe::WbDecoder),
}

impl AmrDecoder {
    /// Decode one MIME-layout frame to PCM *at the codec's own rate*
    /// (8kHz narrowband, 16kHz wideband).
    fn decode(&mut self, frame: &[u8]) -> Vec<i16> {
        match self {
            Self::Nb(d) => d.decode(frame).to_vec(),
            Self::Wb(d) => d.decode(frame).to_vec(),
        }
    }
}

enum AmrEncoder {
    Nb(amr_safe::NbEncoder),
    Wb(amr_safe::WbEncoder),
}

impl AmrEncoder {
    /// Encode exactly one frame's worth of PCM (already at the codec's own
    /// rate), returning the MIME-layout frame.
    fn encode(&mut self, pcm: &[i16]) -> Option<Vec<u8>> {
        match self {
            Self::Nb(e) => {
                let frame: [i16; amr_safe::NB_FRAME_SAMPLES] = pcm.try_into().ok()?;
                Some(e.encode(NB_ENCODE_MODE, &frame))
            }
            Self::Wb(e) => {
                let frame: [i16; amr_safe::FRAME_SAMPLES] = pcm.try_into().ok()?;
                Some(e.encode(WB_ENCODE_MODE, &frame))
            }
        }
    }
}

/// Growing RTP sequence/timestamp state for one direction of the relay. We
/// re-originate packets rather than forwarding them, so their sequence
/// numbers and timestamps are ours to generate — the far end's are
/// meaningless once the payload has been re-encoded.
struct RtpSender {
    seq: u16,
    timestamp: u32,
    ssrc: u32,
    payload_type: u8,
}

impl RtpSender {
    fn new(payload_type: u8) -> Self {
        Self {
            seq: rand::random(),
            timestamp: rand::random(),
            ssrc: rand::random(),
            payload_type,
        }
    }

    /// Emit one packet, advancing the timestamp by the number of samples it
    /// represents *at that packet's own clock rate* (8kHz for PCMU and
    /// AMR-NB, 16kHz for AMR-WB) — the two directions can therefore step by
    /// different amounts.
    fn send(&mut self, socket: &UdpSocket, payload: &[u8], samples: u32) -> io::Result<()> {
        let pkt = rtp::build_packet(
            self.seq,
            self.timestamp,
            self.ssrc,
            self.payload_type,
            payload,
        );
        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(samples);
        socket.send(&pkt).map(|_| ())
    }
}

/// Bridge an AMR carrier leg (`ims`) to a PCMU PBX-side leg (`veth`),
/// transcoding in both directions until `stop` is set.
///
/// `chosen` must be the codec the SDP answer actually selected — its payload
/// type and framing are read from the carrier's offer, never assumed. Sending
/// RTP marked with a payload type or framed in a way the far end never agreed
/// to gets the media discarded and the call torn down.
pub fn spawn_transcoding_relay(
    ims: UdpSocket,
    veth: UdpSocket,
    chosen: ChosenCodec,
    stop: Arc<AtomicBool>,
) -> BridgeResult<()> {
    let kind = match chosen.codec {
        NegotiatedCodec::AmrNb => AmrKind::Nb,
        NegotiatedCodec::AmrWb => AmrKind::Wb,
        NegotiatedCodec::Pcmu => {
            return Err(BridgeError::Ims(
                "PCMU needs no transcoding; use the passthrough relay".into(),
            ))
        }
    };
    let leg = AmrLeg {
        kind,
        payload_type: chosen.payload_type,
        octet_aligned: chosen.octet_aligned,
    };

    // Build both codecs up front, so a missing library fails the call before
    // it is answered rather than leaving a live call with one silent
    // direction.
    let (decoder, encoder) = match kind {
        AmrKind::Nb => (
            AmrDecoder::Nb(
                amr_safe::NbDecoder::new()
                    .map_err(|e| BridgeError::Ims(format!("AMR-NB decoder init failed: {e}")))?,
            ),
            AmrEncoder::Nb(
                amr_safe::NbEncoder::new()
                    .map_err(|e| BridgeError::Ims(format!("AMR-NB encoder init failed: {e}")))?,
            ),
        ),
        AmrKind::Wb => (
            AmrDecoder::Wb(
                amr_safe::WbDecoder::new()
                    .map_err(|e| BridgeError::Ims(format!("AMR-WB decoder init failed: {e}")))?,
            ),
            AmrEncoder::Wb(
                amr_safe::WbEncoder::new()
                    .map_err(|e| BridgeError::Ims(format!("AMR-WB encoder init failed: {e}")))?,
            ),
        ),
    };

    for socket in [&ims, &veth] {
        socket
            .set_read_timeout(Some(RELAY_POLL_INTERVAL))
            .map_err(|e| BridgeError::Ims(format!("relay set_read_timeout failed: {e}")))?;
    }

    let ims_tx = ims
        .try_clone()
        .map_err(|e| BridgeError::Ims(format!("IMS RTP socket clone failed: {e}")))?;
    let veth_tx = veth
        .try_clone()
        .map_err(|e| BridgeError::Ims(format!("veth RTP socket clone failed: {e}")))?;

    tracing::info!(
        codec = ?chosen.codec,
        payload_type = leg.payload_type,
        octet_aligned = leg.octet_aligned,
        "starting transcoding relay"
    );

    let stop_a = stop.clone();
    std::thread::spawn(move || amr_to_pcmu(ims, veth_tx, decoder, leg, stop_a));
    std::thread::spawn(move || pcmu_to_amr(veth, ims_tx, encoder, leg, stop));
    Ok(())
}

/// Carrier -> PBX: decode AMR, resample to 8kHz if it was wideband, re-encode
/// as µ-law.
fn amr_to_pcmu(
    src: UdpSocket,
    dst: UdpSocket,
    mut decoder: AmrDecoder,
    leg: AmrLeg,
    stop: Arc<AtomicBool>,
) {
    let mut resampler = Downsampler::default();
    let mut sender = RtpSender::new(PCMU_PAYLOAD_TYPE);
    let mut pending: Vec<i16> = Vec::new();
    let mut buf = [0u8; 2048];

    while !stop.load(Ordering::Relaxed) {
        let Some(n) = recv(&src, &mut buf, "AMR") else {
            continue;
        };
        let Some(pkt) = rtp::parse_packet(&buf[..n]) else {
            continue;
        };
        // A payload carrying no speech frame (comfort noise, NO_DATA, or a
        // garbled packet) is skipped — these occur routinely on a live call.
        let Some(frame) = amr_rtp::payload_to_frame(pkt.payload, leg.kind, leg.octet_aligned)
        else {
            continue;
        };
        let pcm = decoder.decode(&frame);
        let pcm8k = match leg.kind {
            AmrKind::Nb => pcm,
            AmrKind::Wb => resampler.process(&pcm),
        };
        pending.extend_from_slice(&pcm8k);

        while pending.len() >= PCMU_FRAME_SAMPLES {
            let payload: Vec<u8> = pending
                .drain(..PCMU_FRAME_SAMPLES)
                .map(rtp::linear_to_ulaw)
                .collect();
            if let Err(e) = sender.send(&dst, &payload, PCMU_FRAME_SAMPLES as u32) {
                tracing::warn!(error = %e, "transcoding relay: PCMU send failed");
                return;
            }
        }
    }
}

/// PBX -> carrier: decode µ-law, resample up to 16kHz if the carrier leg is
/// wideband, re-encode as AMR in the negotiated framing.
fn pcmu_to_amr(
    src: UdpSocket,
    dst: UdpSocket,
    mut encoder: AmrEncoder,
    leg: AmrLeg,
    stop: Arc<AtomicBool>,
) {
    let mut resampler = Upsampler::default();
    let mut sender = RtpSender::new(leg.payload_type);
    let mut pending: Vec<i16> = Vec::new();
    let mut buf = [0u8; 2048];
    let frame_samples = leg.frame_samples();

    while !stop.load(Ordering::Relaxed) {
        let Some(n) = recv(&src, &mut buf, "PCMU") else {
            continue;
        };
        let Some(pkt) = rtp::parse_packet(&buf[..n]) else {
            continue;
        };
        let pcm8k: Vec<i16> = pkt
            .payload
            .iter()
            .map(|&b| rtp::ulaw_to_linear(b))
            .collect();
        match leg.kind {
            AmrKind::Nb => pending.extend_from_slice(&pcm8k),
            AmrKind::Wb => pending.extend_from_slice(&resampler.process(&pcm8k)),
        }

        while pending.len() >= frame_samples {
            let pcm: Vec<i16> = pending.drain(..frame_samples).collect();
            let Some(frame) = encoder.encode(&pcm) else {
                continue;
            };
            let Some(payload) = amr_rtp::frame_to_payload(&frame, leg.kind, leg.octet_aligned)
            else {
                continue;
            };
            if let Err(e) = sender.send(&dst, &payload, frame_samples as u32) {
                tracing::warn!(error = %e, "transcoding relay: AMR send failed");
                return;
            }
        }
    }
}

/// One `recv`, treating a poll timeout as "nothing yet" (the relay is
/// expected to idle whenever the far end is silent) and any real error as
/// terminal for this direction.
fn recv(socket: &UdpSocket, buf: &mut [u8], what: &str) -> Option<usize> {
    match socket.recv(buf) {
        Ok(n) => Some(n),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, direction = %what, "transcoding relay: recv failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsampler_halves_the_sample_count() {
        let mut d = Downsampler::default();
        assert_eq!(d.process(&[100; 320]).len(), 160);
    }

    #[test]
    fn upsampler_doubles_the_sample_count() {
        let mut u = Upsampler::default();
        assert_eq!(u.process(&[100; 160]).len(), 320);
    }

    /// A constant (DC) signal must survive both stages unchanged — any gain
    /// error or off-by-one in the filter windows shows up here immediately.
    #[test]
    fn resampling_a_constant_signal_preserves_its_level() {
        let mut down = Downsampler::default();
        // Prime `prev` so the very first window isn't straddling the initial
        // zero, which legitimately ramps.
        down.prev = 1000;
        let halved = down.process(&[1000i16; 320]);
        assert!(halved.iter().all(|&s| s == 1000), "downsampled DC drifted");

        let mut up = Upsampler::default();
        up.prev = 1000;
        let doubled = up.process(&[1000i16; 160]);
        assert!(doubled.iter().all(|&s| s == 1000), "upsampled DC drifted");
    }

    /// Round-tripping through µ-law is lossy, but must stay close enough that
    /// speech survives — µ-law's worst-case relative error is ~8%.
    #[test]
    fn ulaw_round_trip_stays_within_companding_error() {
        for sample in [-20000i16, -8000, -1000, -100, 0, 100, 1000, 8000, 20000] {
            let back = rtp::ulaw_to_linear(rtp::linear_to_ulaw(sample));
            let tolerance = (sample.abs() / 10).max(64);
            assert!(
                (back as i32 - sample as i32).abs() <= tolerance as i32,
                "µ-law round trip of {sample} gave {back}, beyond tolerance {tolerance}"
            );
        }
    }

    /// Narrowband is already 8kHz, so it must not be resampled — a stray
    /// resample would halve or double the pitch.
    #[test]
    fn narrowband_frame_is_one_pcmu_packet_and_wideband_is_two() {
        let nb = AmrLeg {
            kind: AmrKind::Nb,
            payload_type: 108,
            octet_aligned: false,
        };
        let wb = AmrLeg {
            kind: AmrKind::Wb,
            payload_type: 110,
            octet_aligned: true,
        };
        assert_eq!(nb.frame_samples(), PCMU_FRAME_SAMPLES);
        assert_eq!(wb.frame_samples(), PCMU_FRAME_SAMPLES * 2);
    }

    /// The relay re-originates RTP, so each packet must advance the sequence
    /// number by one and the timestamp by that packet's own sample count.
    #[test]
    fn rtp_sender_advances_sequence_and_timestamp_per_packet() {
        let mut sender = RtpSender::new(PCMU_PAYLOAD_TYPE);
        let (seq0, ts0) = (sender.seq, sender.timestamp);

        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.connect(peer.local_addr().unwrap()).unwrap();

        sender.send(&socket, &[0u8; 160], 160).unwrap();
        assert_eq!(sender.seq, seq0.wrapping_add(1));
        assert_eq!(sender.timestamp, ts0.wrapping_add(160));

        sender.send(&socket, &[0u8; 160], 160).unwrap();
        assert_eq!(sender.seq, seq0.wrapping_add(2));
        assert_eq!(sender.timestamp, ts0.wrapping_add(320));

        let mut buf = [0u8; 512];
        let n = peer.recv(&mut buf).unwrap();
        let pkt = rtp::parse_packet(&buf[..n]).expect("relayed packet should parse as RTP");
        assert_eq!(pkt.payload_type, PCMU_PAYLOAD_TYPE);
        assert_eq!(pkt.payload.len(), 160);
    }

    /// PCMU needs no transcoding at all — routing it here would be a bug in
    /// the caller, so it must be rejected rather than silently mangled.
    #[test]
    fn pcmu_is_rejected_by_the_transcoding_relay() {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        let chosen = ChosenCodec {
            codec: NegotiatedCodec::Pcmu,
            payload_type: 0,
            octet_aligned: false,
        };
        assert!(spawn_transcoding_relay(a, b, chosen, Arc::new(AtomicBool::new(false))).is_err());
    }
}
