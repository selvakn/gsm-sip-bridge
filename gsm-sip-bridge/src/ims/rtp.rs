//! RTP framing, ITU-T G.711 μ-law (PCMU) codec, and a minimal WAV writer —
//! everything needed to send/receive a test call's audio without pulling in
//! a codec library. PCMU is the simplest payload RTP/AVP defines (8-bit
//! non-linear PCM, no state, no external table needed beyond the standard
//! μ-law formula), and is offered in `ims::sdp`'s SDP because of that.

use crate::error::{BridgeError, BridgeResult};
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

pub const RTP_HEADER_LEN: usize = 12;
pub const RTP_VERSION: u8 = 2;

/// Build one RTP packet (RFC 3550 §5.1) carrying `payload` (already
/// encoded, e.g. μ-law bytes).
pub fn build_packet(
    seq: u16,
    timestamp: u32,
    ssrc: u32,
    payload_type: u8,
    payload: &[u8],
) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
    pkt.push(RTP_VERSION << 6); // V=2, P=0, X=0, CC=0
    pkt.push(payload_type & 0x7f); // M=0
    pkt.extend_from_slice(&seq.to_be_bytes());
    pkt.extend_from_slice(&timestamp.to_be_bytes());
    pkt.extend_from_slice(&ssrc.to_be_bytes());
    pkt.extend_from_slice(payload);
    pkt
}

/// A parsed RTP packet's header fields plus a view of its payload. Not
/// every field is consumed by the current single-codec, no-reordering call
/// flow, but they're part of a correctly-parsed packet's identity.
#[allow(dead_code)]
pub struct ParsedPacket<'a> {
    pub seq: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload_type: u8,
    pub payload: &'a [u8],
}

pub fn parse_packet(data: &[u8]) -> Option<ParsedPacket<'_>> {
    if data.len() < RTP_HEADER_LEN {
        return None;
    }
    let version = data[0] >> 6;
    if version != RTP_VERSION {
        return None;
    }
    let cc = (data[0] & 0x0f) as usize;
    let payload_type = data[1] & 0x7f;
    let seq = u16::from_be_bytes([data[2], data[3]]);
    let timestamp = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    let header_len = RTP_HEADER_LEN + cc * 4;
    if data.len() < header_len {
        return None;
    }
    Some(ParsedPacket {
        seq,
        timestamp,
        ssrc,
        payload_type,
        payload: &data[header_len..],
    })
}

/// ITU-T G.711 μ-law encode: 16-bit linear PCM -> 8-bit μ-law. Sign +
/// magnitude, `BIAS`-shifted then floating-point-like segment (3-bit
/// exponent) + mantissa (4 bits) encoded, inverted for transmission (the
/// historical DC-balance convention) — `BIAS`/`CLIP` are the standard G.711
/// constants. `exponent` is `floor(log2(magnitude_biased / 128))`: silence
/// (`sample = 0`) biases to exactly 132, giving `exponent = 0, mantissa =
/// 0`, i.e. byte `0xFF` — the conventional PCMU silence byte, which is a
/// useful sanity check on this derivation.
pub fn linear_to_ulaw(sample: i16) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32635;

    let sign: u8 = if sample < 0 { 0x80 } else { 0x00 };
    let mut magnitude = if sample < 0 {
        -(sample as i32)
    } else {
        sample as i32
    };
    if magnitude > CLIP {
        magnitude = CLIP;
    }
    magnitude += BIAS;

    // magnitude >= BIAS = 132 always, so magnitude >> 7 is always >= 1.
    let v = (magnitude >> 7) as u32;
    let exponent: u8 = (31 - v.leading_zeros()).min(7) as u8; // floor(log2(v))
    let mantissa = ((magnitude >> (exponent + 3)) & 0x0f) as u8;
    !(sign | (exponent << 4) | mantissa)
}

/// ITU-T G.711 μ-law decode: 8-bit μ-law -> 16-bit linear PCM. Inverse of
/// `linear_to_ulaw`: reconstructs the segment's biased magnitude at the
/// midpoint of its quantization step (`+ 1 << (exponent+2)`, half a step,
/// to minimize expected reconstruction error) rather than its start, then
/// removes `BIAS`.
pub fn ulaw_to_linear(ulaw: u8) -> i16 {
    const BIAS: i32 = 0x84;

    let ulaw = !ulaw;
    let sign = ulaw & 0x80;
    let exponent = ((ulaw >> 4) & 0x07) as i32;
    let mantissa = (ulaw & 0x0f) as i32;
    let magnitude_biased = ((mantissa | 0x10) << (exponent + 3)) + (1 << (exponent + 2));
    let magnitude = magnitude_biased - BIAS;
    let sample = if sign != 0 { -magnitude } else { magnitude };
    sample.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// A mono 16-bit PCM WAV file, written incrementally (header sizes are
/// patched in on `finish()` since the total sample count isn't known
/// upfront for a live-recorded call).
pub struct WavWriter {
    file: BufWriter<File>,
    sample_rate: u32,
    samples_written: u32,
}

impl WavWriter {
    pub fn create(path: &Path, sample_rate: u32) -> BridgeResult<Self> {
        let file = File::create(path)
            .map_err(|e| BridgeError::Ims(format!("failed to create {}: {e}", path.display())))?;
        let mut writer = BufWriter::new(file);
        // Placeholder header, sizes patched in on finish().
        write_wav_header(&mut writer, sample_rate, 0)
            .map_err(|e| BridgeError::Ims(format!("failed to write WAV header: {e}")))?;
        Ok(Self {
            file: writer,
            sample_rate,
            samples_written: 0,
        })
    }

    pub fn write_samples(&mut self, samples: &[i16]) -> BridgeResult<()> {
        for &s in samples {
            self.file
                .write_all(&s.to_le_bytes())
                .map_err(|e| BridgeError::Ims(format!("WAV write failed: {e}")))?;
        }
        self.samples_written += samples.len() as u32;
        Ok(())
    }

    pub fn samples_written(&self) -> u32 {
        self.samples_written
    }

    pub fn finish(mut self) -> BridgeResult<()> {
        self.file
            .flush()
            .map_err(|e| BridgeError::Ims(format!("WAV flush failed: {e}")))?;
        let mut file = self
            .file
            .into_inner()
            .map_err(|e| BridgeError::Ims(format!("WAV flush failed: {e}")))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|e| BridgeError::Ims(format!("WAV seek failed: {e}")))?;
        write_wav_header(&mut file, self.sample_rate, self.samples_written)
            .map_err(|e| BridgeError::Ims(format!("failed to patch WAV header: {e}")))?;
        Ok(())
    }
}

fn write_wav_header<W: Write>(
    w: &mut W,
    sample_rate: u32,
    num_samples: u32,
) -> std::io::Result<()> {
    const BITS_PER_SAMPLE: u32 = 16;
    const NUM_CHANNELS: u32 = 1;
    let byte_rate = sample_rate * NUM_CHANNELS * BITS_PER_SAMPLE / 8;
    let block_align = NUM_CHANNELS * BITS_PER_SAMPLE / 8;
    let data_len = num_samples * (BITS_PER_SAMPLE / 8);

    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_len).to_le_bytes())?;
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?; // fmt chunk size
    w.write_all(&1u16.to_le_bytes())?; // PCM
    w.write_all(&(NUM_CHANNELS as u16).to_le_bytes())?;
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&byte_rate.to_le_bytes())?;
    w.write_all(&(block_align as u16).to_le_bytes())?;
    w.write_all(&(BITS_PER_SAMPLE as u16).to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_parse_packet_roundtrips_header_fields() {
        let payload = [0xffu8, 0xfe, 0x00];
        let pkt = build_packet(42, 1234, 0xdeadbeef, 0, &payload);
        let parsed = parse_packet(&pkt).unwrap();
        assert_eq!(parsed.seq, 42);
        assert_eq!(parsed.timestamp, 1234);
        assert_eq!(parsed.ssrc, 0xdeadbeef);
        assert_eq!(parsed.payload_type, 0);
        assert_eq!(parsed.payload, &payload);
    }

    #[test]
    fn parse_packet_rejects_too_short_buffer() {
        assert!(parse_packet(&[0u8; 4]).is_none());
    }

    #[test]
    fn ulaw_roundtrip_is_close_to_original_within_quantization_error() {
        for sample in [0i16, 100, -100, 1000, -1000, 30000, -30000] {
            let encoded = linear_to_ulaw(sample);
            let decoded = ulaw_to_linear(encoded);
            // mu-law is lossy (8 bits from 16); allow generous tolerance.
            let diff = (decoded as i32 - sample as i32).abs();
            assert!(
                diff < 1500,
                "sample {sample} decoded to {decoded}, diff {diff}"
            );
        }
    }

    #[test]
    fn ulaw_silence_roundtrips_exactly_and_uses_conventional_byte() {
        let encoded = linear_to_ulaw(0);
        assert_eq!(encoded, 0xFF, "0xFF is the conventional PCMU silence byte");
        assert_eq!(ulaw_to_linear(encoded), 0);
    }

    #[test]
    fn wav_writer_produces_valid_header_and_data_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wav");
        let mut writer = WavWriter::create(&path, 8000).unwrap();
        writer.write_samples(&[1, 2, 3, 4]).unwrap();
        writer.finish().unwrap();

        let data = std::fs::read(&path).unwrap();
        assert_eq!(&data[0..4], b"RIFF");
        assert_eq!(&data[8..12], b"WAVE");
        let data_len = u32::from_le_bytes(data[40..44].try_into().unwrap());
        assert_eq!(data_len, 8); // 4 samples * 2 bytes
        assert_eq!(data.len(), 44 + 8);
    }
}
