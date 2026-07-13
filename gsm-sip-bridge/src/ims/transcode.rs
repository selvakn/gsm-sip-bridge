//! Transcoding RTP relay between the carrier leg and the veth leg of an
//! inbound VoWiFi call — terminating the codec on each side and re-encoding,
//! rather than forwarding opaque payloads.
//!
//! `ims::agent`'s plain `relay_rtp` forwards RTP packets untouched, which is
//! all that's needed when both legs agreed on PCMU. But a real carrier
//! commonly offers **no PCMU at all** on a mobile-terminating VoWiFi INVITE —
//! Airtel was observed offering AMR-WB+AMR-NB on some calls and AMR-NB alone
//! on others — so without transcoding those calls can only be declined.
//!
//! Either leg can be narrowband or wideband, and the two need not match:
//!
//! | carrier leg | veth leg     | what happens here                        |
//! |-------------|--------------|------------------------------------------|
//! | AMR-WB 16k  | L16 16k      | decode/encode only — **no resampling**   |
//! | AMR-WB 16k  | PCMU 8k      | decode, downsample, µ-law                |
//! | AMR-NB 8k   | PCMU 8k      | decode, µ-law — no resampling            |
//! | PCMU 8k     | PCMU 8k      | not here: `agent::relay_rtp` passes bytes |
//!
//! The first row is the one worth having: the carrier's AMR-WB *is* real
//! 16 kHz audio, and L16 carries it to Agent B's PJSIP leg (whose conference
//! bridge also runs at 16 kHz) without ever passing through 8 kHz. The second
//! row is the fallback when Agent B's PJSIP has no L16 codec to offer, and is
//! what every wideband call did before L16 existed.
//!
//! RTP framing for either AMR flavour is handled by `super::amr_rtp`, which
//! supports both the octet-aligned and the bandwidth-efficient payload
//! formats — the carrier's offer decides which, and it is not ours to choose.

use super::amr_rtp::{self, AmrKind};
use super::rtp;
use super::sdp::{ChosenCodec, NegotiatedCodec};
use crate::error::{BridgeError, BridgeResult};
use std::io;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How often a blocking `recv` wakes to re-check `stop`, bounding how
/// quickly a hangup silences the relay. Matches `agent::RELAY_POLL_INTERVAL`.
const RELAY_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// The rates we encode AMR at, one per flavour. Both are the usual VoLTE
/// operating points and sit inside the `mode-set` carriers advertise. The far
/// end signals its own mode per-frame and we decode whatever it sends, so
/// these only govern our own encoding.
const WB_ENCODE_MODE: amr_safe::Mode = amr_safe::Mode::R1265;
const NB_ENCODE_MODE: amr_safe::NbMode = amr_safe::NbMode::R1220;
/// Big enough for any payload either leg can send: the largest is one 20 ms
/// L16/16000 frame, 320 samples × 2 bytes + RTP header.
const RECV_BUF: usize = 2048;

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

/// Rate conversion for one direction of the relay. Every codec here is either
/// 8 kHz or 16 kHz, so the only ratios that can arise are 1:1, 2:1 and 1:2 —
/// and 1:1 is the interesting one, since that is what an AMR-WB carrier leg
/// bridged to an L16 veth leg gets: no resampling at all.
enum Resampler {
    Passthrough,
    Down(Downsampler),
    Up(Upsampler),
}

impl Resampler {
    fn between(from_rate: u32, to_rate: u32) -> BridgeResult<Self> {
        match (from_rate, to_rate) {
            (a, b) if a == b => Ok(Self::Passthrough),
            (16000, 8000) => Ok(Self::Down(Downsampler::default())),
            (8000, 16000) => Ok(Self::Up(Upsampler::default())),
            (a, b) => Err(BridgeError::Ims(format!("no resampler for {a}Hz -> {b}Hz"))),
        }
    }

    fn process(&mut self, pcm: Vec<i16>) -> Vec<i16> {
        match self {
            Self::Passthrough => pcm,
            Self::Down(d) => d.process(&pcm),
            Self::Up(u) => u.process(&pcm),
        }
    }
}

/// Either AMR flavour's decoder. An enum rather than a trait object: there are
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

/// The AMR flavour a `NegotiatedCodec` names, or `None` if it isn't AMR.
fn amr_kind(codec: NegotiatedCodec) -> Option<AmrKind> {
    match codec {
        NegotiatedCodec::AmrNb => Some(AmrKind::Nb),
        NegotiatedCodec::AmrWb => Some(AmrKind::Wb),
        NegotiatedCodec::Pcmu | NegotiatedCodec::L16 => None,
    }
}

/// Turns one leg's RTP payloads into PCM at that leg's own sample rate.
enum Decoder {
    Pcmu,
    L16,
    Amr {
        decoder: AmrDecoder,
        kind: AmrKind,
        octet_aligned: bool,
    },
}

impl Decoder {
    fn new(leg: ChosenCodec) -> BridgeResult<Self> {
        Ok(match amr_kind(leg.codec) {
            None if leg.codec == NegotiatedCodec::L16 => Self::L16,
            None => Self::Pcmu,
            Some(kind) => Self::Amr {
                decoder: match kind {
                    AmrKind::Nb => AmrDecoder::Nb(amr_safe::NbDecoder::new().map_err(|e| {
                        BridgeError::Ims(format!("AMR-NB decoder init failed: {e}"))
                    })?),
                    AmrKind::Wb => AmrDecoder::Wb(amr_safe::WbDecoder::new().map_err(|e| {
                        BridgeError::Ims(format!("AMR-WB decoder init failed: {e}"))
                    })?),
                },
                kind,
                octet_aligned: leg.octet_aligned,
            },
        })
    }

    /// `None` for a payload carrying no speech (comfort noise, AMR `NO_DATA`,
    /// or a garbled packet) — those occur routinely on a live call and are
    /// simply skipped.
    fn decode(&mut self, payload: &[u8]) -> Option<Vec<i16>> {
        match self {
            Self::Pcmu => Some(payload.iter().map(|&b| rtp::ulaw_to_linear(b)).collect()),
            // RFC 3551 §4.5.11: L16 samples are 16-bit two's complement, in
            // network (big-endian) byte order, with no payload header.
            Self::L16 => Some(
                payload
                    .chunks_exact(2)
                    .map(|s| i16::from_be_bytes([s[0], s[1]]))
                    .collect(),
            ),
            Self::Amr {
                decoder,
                kind,
                octet_aligned,
            } => {
                let frame = amr_rtp::payload_to_frame(payload, *kind, *octet_aligned)?;
                Some(decoder.decode(&frame))
            }
        }
    }
}

/// Turns PCM at one leg's own sample rate back into that leg's RTP payloads.
enum Encoder {
    Pcmu,
    L16,
    Amr {
        encoder: AmrEncoder,
        kind: AmrKind,
        octet_aligned: bool,
    },
}

impl Encoder {
    fn new(leg: ChosenCodec) -> BridgeResult<Self> {
        Ok(match amr_kind(leg.codec) {
            None if leg.codec == NegotiatedCodec::L16 => Self::L16,
            None => Self::Pcmu,
            Some(kind) => Self::Amr {
                encoder: match kind {
                    AmrKind::Nb => AmrEncoder::Nb(amr_safe::NbEncoder::new().map_err(|e| {
                        BridgeError::Ims(format!("AMR-NB encoder init failed: {e}"))
                    })?),
                    AmrKind::Wb => AmrEncoder::Wb(amr_safe::WbEncoder::new().map_err(|e| {
                        BridgeError::Ims(format!("AMR-WB encoder init failed: {e}"))
                    })?),
                },
                kind,
                octet_aligned: leg.octet_aligned,
            },
        })
    }

    /// `pcm` is exactly one frame at this leg's rate.
    fn encode(&mut self, pcm: &[i16]) -> Option<Vec<u8>> {
        match self {
            Self::Pcmu => Some(pcm.iter().map(|&s| rtp::linear_to_ulaw(s)).collect()),
            Self::L16 => Some(pcm.iter().flat_map(|s| s.to_be_bytes()).collect()),
            Self::Amr {
                encoder,
                kind,
                octet_aligned,
            } => {
                let frame = encoder.encode(pcm)?;
                amr_rtp::frame_to_payload(&frame, *kind, *octet_aligned)
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
    /// AMR-NB, 16kHz for AMR-WB and L16) — the two directions can therefore
    /// step by different amounts.
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

/// Bridge the carrier leg (`ims`) to Agent B's veth leg (`veth`), transcoding
/// in both directions until `stop` is set.
///
/// Both `carrier` and `veth_codec` must be the codecs the respective SDP
/// answers actually selected — their payload types and framing are read from
/// the offers, never assumed. Sending RTP marked with a payload type or framed
/// in a way the far end never agreed to gets the media discarded and the call
/// torn down.
pub fn spawn_transcoding_relay(
    ims: UdpSocket,
    veth: UdpSocket,
    carrier: ChosenCodec,
    veth_codec: ChosenCodec,
    stop: Arc<AtomicBool>,
) -> BridgeResult<()> {
    if carrier.codec == NegotiatedCodec::Pcmu && veth_codec.codec == NegotiatedCodec::Pcmu {
        return Err(BridgeError::Ims(
            "both legs are PCMU; use the passthrough relay".into(),
        ));
    }

    // Build every codec up front, so a missing library fails the call before
    // it is answered rather than leaving a live call with one silent
    // direction.
    let carrier_decoder = Decoder::new(carrier)?;
    let carrier_encoder = Encoder::new(carrier)?;
    let veth_decoder = Decoder::new(veth_codec)?;
    let veth_encoder = Encoder::new(veth_codec)?;

    let (carrier_rate, veth_rate) = (carrier.codec.sample_rate(), veth_codec.codec.sample_rate());
    let to_veth = Resampler::between(carrier_rate, veth_rate)?;
    let to_carrier = Resampler::between(veth_rate, carrier_rate)?;

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
        carrier_codec = ?carrier.codec,
        carrier_payload_type = carrier.payload_type,
        carrier_octet_aligned = carrier.octet_aligned,
        veth_codec = ?veth_codec.codec,
        veth_payload_type = veth_codec.payload_type,
        resampling = carrier_rate != veth_rate,
        "starting transcoding relay"
    );

    let stop_a = stop.clone();
    std::thread::spawn(move || {
        relay_direction(
            ims,
            veth_tx,
            carrier_decoder,
            veth_encoder,
            to_veth,
            veth_codec,
            "carrier->veth",
            stop_a,
        )
    });
    std::thread::spawn(move || {
        relay_direction(
            veth,
            ims_tx,
            veth_decoder,
            carrier_encoder,
            to_carrier,
            carrier,
            "veth->carrier",
            stop,
        )
    });
    Ok(())
}

/// One direction: decode `src`'s payloads to PCM, resample to the far leg's
/// rate, and re-encode into whole `out` frames. PCM left over from a packet
/// that didn't divide evenly into output frames (a 16 kHz AMR-WB frame is two
/// 8 kHz PCMU packets; an 8 kHz one is half of an L16 frame) carries over to
/// the next packet rather than being padded or dropped.
#[allow(clippy::too_many_arguments)]
fn relay_direction(
    src: UdpSocket,
    dst: UdpSocket,
    mut decoder: Decoder,
    mut encoder: Encoder,
    mut resampler: Resampler,
    out: ChosenCodec,
    direction: &'static str,
    stop: Arc<AtomicBool>,
) {
    let frame_samples = out.codec.frame_samples();
    let mut sender = RtpSender::new(out.payload_type);
    let mut pending: Vec<i16> = Vec::new();
    let mut buf = [0u8; RECV_BUF];

    while !stop.load(Ordering::Relaxed) {
        let Some(n) = recv(&src, &mut buf, direction) else {
            continue;
        };
        let Some(pkt) = rtp::parse_packet(&buf[..n]) else {
            continue;
        };
        let Some(pcm) = decoder.decode(pkt.payload) else {
            continue;
        };
        pending.extend_from_slice(&resampler.process(pcm));

        while pending.len() >= frame_samples {
            let frame: Vec<i16> = pending.drain(..frame_samples).collect();
            let Some(payload) = encoder.encode(&frame) else {
                continue;
            };
            if let Err(e) = sender.send(&dst, &payload, frame_samples as u32) {
                tracing::warn!(error = %e, direction, "transcoding relay: send failed");
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

    fn codec(codec: NegotiatedCodec, payload_type: u8) -> ChosenCodec {
        ChosenCodec {
            codec,
            payload_type,
            octet_aligned: true,
        }
    }

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

    /// Two legs at the same rate must not be resampled at all — a stray
    /// resample would halve or double the pitch. This is the AMR-WB-to-L16
    /// path, the whole reason L16 exists on the veth link.
    #[test]
    fn equal_rates_resample_by_passing_through_untouched() {
        let mut r = Resampler::between(16000, 16000).unwrap();
        let pcm: Vec<i16> = (0..320).map(|i| i as i16 * 100).collect();
        assert_eq!(r.process(pcm.clone()), pcm);
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

    /// L16 is uncompressed, so unlike µ-law its round trip must be *exact* —
    /// that losslessness is the reason it's worth 256 kbit/s on the veth.
    #[test]
    fn l16_round_trip_is_lossless_and_big_endian() {
        let pcm: Vec<i16> = vec![-32768, -1000, -1, 0, 1, 1000, 32767];
        let mut encoder = Encoder::new(codec(NegotiatedCodec::L16, 96)).unwrap();
        let payload = encoder.encode(&pcm).unwrap();
        assert_eq!(payload.len(), pcm.len() * 2);
        assert_eq!(&payload[..2], &(-32768i16).to_be_bytes());

        let mut decoder = Decoder::new(codec(NegotiatedCodec::L16, 96)).unwrap();
        assert_eq!(decoder.decode(&payload).unwrap(), pcm);
    }

    /// A live relay, driven over real sockets: 8 kHz µ-law in on the carrier
    /// side must come out the veth side as 16 kHz L16 — right payload type,
    /// one whole 20 ms frame, and a timestamp ticking at the *output* leg's
    /// clock. This is the wiring (decode → resample → re-encode → re-originate
    /// RTP) that a wideband call depends on; only the codec at the carrier end
    /// differs, and that one needs a linked AMR library to exercise.
    #[test]
    fn a_live_relay_converts_8k_pcmu_into_20ms_16k_l16_frames() {
        // Two connected socket pairs: one standing in for the carrier, one for
        // Agent B at the far end of the veth.
        let (ims, carrier) = connected_pair();
        let (veth, agent_b) = connected_pair();
        agent_b
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        spawn_transcoding_relay(
            ims,
            veth,
            codec(NegotiatedCodec::Pcmu, 0),
            codec(NegotiatedCodec::L16, 96),
            stop.clone(),
        )
        .unwrap();

        // One 20 ms PCMU packet: 160 samples at 8 kHz.
        let payload: Vec<u8> = (0..160).map(|i| rtp::linear_to_ulaw(i * 100)).collect();
        carrier
            .send(&rtp::build_packet(1, 0, 0xdead_beef, 0, &payload))
            .unwrap();

        let mut buf = [0u8; RECV_BUF];
        let n = agent_b
            .recv(&mut buf)
            .expect("relay should emit an L16 packet");
        let (first_seq, first_ts) = {
            let pkt = rtp::parse_packet(&buf[..n]).expect("output should parse as RTP");
            assert_eq!(
                pkt.payload_type, 96,
                "must be marked with the L16 type Agent B offered"
            );
            assert_eq!(
                pkt.payload.len(),
                320 * 2,
                "160 samples at 8 kHz upsample to one 320-sample 16 kHz frame, 2 bytes each"
            );
            (pkt.seq, pkt.timestamp)
        };

        // A second packet must advance the timestamp by a 16 kHz frame, not an
        // 8 kHz one — getting this wrong plays the audio at half speed.
        carrier
            .send(&rtp::build_packet(2, 160, 0xdead_beef, 0, &payload))
            .unwrap();
        let n = agent_b.recv(&mut buf).unwrap();
        let next = rtp::parse_packet(&buf[..n]).unwrap();
        assert_eq!(next.timestamp.wrapping_sub(first_ts), 320);
        assert_eq!(next.seq.wrapping_sub(first_seq), 1);

        stop.store(true, Ordering::Relaxed);
    }

    /// The same relay in the other direction: Agent B's 16 kHz L16 must reach
    /// the carrier as 8 kHz µ-law.
    #[test]
    fn a_live_relay_converts_16k_l16_back_into_8k_pcmu_frames() {
        let (ims, carrier) = connected_pair();
        let (veth, agent_b) = connected_pair();
        carrier
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        spawn_transcoding_relay(
            ims,
            veth,
            codec(NegotiatedCodec::Pcmu, 0),
            codec(NegotiatedCodec::L16, 96),
            stop.clone(),
        )
        .unwrap();

        // One 20 ms L16 frame: 320 samples at 16 kHz, big-endian.
        let payload: Vec<u8> = (0..320i16).flat_map(|i| (i * 50).to_be_bytes()).collect();
        agent_b
            .send(&rtp::build_packet(1, 0, 0xfeed_face, 96, &payload))
            .unwrap();

        let mut buf = [0u8; RECV_BUF];
        let n = carrier
            .recv(&mut buf)
            .expect("relay should emit a PCMU packet");
        let pkt = rtp::parse_packet(&buf[..n]).unwrap();
        assert_eq!(pkt.payload_type, 0);
        assert_eq!(
            pkt.payload.len(),
            160,
            "320 samples at 16 kHz downsample to one 160-sample 8 kHz µ-law packet"
        );

        stop.store(true, Ordering::Relaxed);
    }

    /// Two sockets `connect`ed to each other, as both relay legs are in a real
    /// call.
    fn connected_pair() -> (UdpSocket, UdpSocket) {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        a.connect(b.local_addr().unwrap()).unwrap();
        b.connect(a.local_addr().unwrap()).unwrap();
        (a, b)
    }

    /// The relay re-originates RTP, so each packet must advance the sequence
    /// number by one and the timestamp by that packet's own sample count.
    #[test]
    fn rtp_sender_advances_sequence_and_timestamp_per_packet() {
        let mut sender = RtpSender::new(0);
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
        assert_eq!(pkt.payload_type, 0);
        assert_eq!(pkt.payload.len(), 160);
    }

    /// Two PCMU legs need no transcoding at all — routing them here would be a
    /// bug in the caller (`agent::relay_rtp` forwards the bytes verbatim), so
    /// it must be rejected rather than silently burning CPU on a µ-law round
    /// trip that can only lose quality.
    #[test]
    fn two_pcmu_legs_are_rejected_by_the_transcoding_relay() {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(spawn_transcoding_relay(
            a,
            b,
            codec(NegotiatedCodec::Pcmu, 0),
            codec(NegotiatedCodec::Pcmu, 0),
            Arc::new(AtomicBool::new(false)),
        )
        .is_err());
    }

    /// A PCMU carrier leg bridged to an L16 veth leg is not a passthrough —
    /// it must be accepted and upsampled. (Agent A doesn't choose this pairing
    /// today, but nothing here may silently mangle it if it ever does.)
    #[test]
    fn a_pcmu_carrier_leg_with_an_l16_veth_leg_is_transcodable() {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        a.connect(b.local_addr().unwrap()).unwrap();
        b.connect(a.local_addr().unwrap()).unwrap();
        let stop = Arc::new(AtomicBool::new(true)); // threads exit immediately
        spawn_transcoding_relay(
            a,
            b,
            codec(NegotiatedCodec::Pcmu, 0),
            codec(NegotiatedCodec::L16, 96),
            stop,
        )
        .expect("8k carrier to 16k veth must resample rather than be refused");
    }
}
