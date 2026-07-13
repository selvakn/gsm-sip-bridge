//! RFC 4867 RTP payload framing for AMR / AMR-WB, in both the
//! **octet-aligned** (§4.4) and **bandwidth-efficient** (§4.3) formats.
//!
//! `amr_safe`'s encoders emit, and its decoders consume, a frame in the
//! codec libraries' "MIME storage" layout: one header byte
//! (`F<<7 | FT<<3 | Q<<2`) followed by the packed speech bytes. That layout
//! happens to be bit-for-bit identical to an octet-aligned RTP ToC entry plus
//! its frame data, which is why the octet-aligned path is nearly free — it
//! just prepends a CMR byte.
//!
//! Bandwidth-efficient framing is the one that needs real work: nothing is
//! octet-aligned, so the CMR (4 bits), the ToC (6 bits) and the speech bits
//! run continuously and the speech data ends up shifted 10 bits into the
//! payload. It cannot be skipped: carriers offer AMR-NB *only* in
//! bandwidth-efficient form (observed on Airtel, whose mobile-terminating
//! INVITE offered `AMR/8000` with no `octet-align=1` on any payload type), so
//! without this an inbound call on those offers can only be declined.
//!
//! Only the single-frame-per-packet case is handled (`F=0`), which is what
//! 20ms ptime negotiation yields and what every offer seen in practice uses.

/// Which AMR flavour a payload carries. The frame-type numbering is *not*
/// shared between them — index 7 means 12.2kbps/244 bits in narrowband but
/// 23.05kbps/461 bits in wideband — so every framing operation has to know
/// which codec it is working on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmrKind {
    Nb,
    Wb,
}

/// Number of speech bits in one 20ms frame, per frame type (TS 26.101 §4.1
/// for narrowband, TS 26.201 §4.1 for wideband). `None` for a frame type
/// that is not a speech frame (SID/comfort noise, NO_DATA, or reserved) —
/// those carry no audio and are skipped rather than decoded.
fn speech_bits(kind: AmrKind, frame_type: u8) -> Option<usize> {
    let table: &[usize] = match kind {
        AmrKind::Nb => &[95, 103, 118, 134, 148, 159, 204, 244],
        AmrKind::Wb => &[132, 177, 253, 285, 317, 365, 397, 461, 477],
    };
    table.get(frame_type as usize).copied()
}

/// Reads big-endian bit fields across byte boundaries — RFC 4867's
/// bandwidth-efficient format packs everything MSB-first with no padding
/// between fields.
struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// Reads `n` bits (n <= 8), MSB-first. `None` if the input runs out.
    fn read(&mut self, n: usize) -> Option<u8> {
        let mut out = 0u8;
        for _ in 0..n {
            let byte = self.bytes.get(self.pos / 8)?;
            let bit = (byte >> (7 - (self.pos % 8))) & 1;
            out = (out << 1) | bit;
            self.pos += 1;
        }
        Some(out)
    }

    /// Reads `n` bits into left-aligned bytes (the layout the codec libraries
    /// expect for the speech payload: first speech bit in the MSB of byte 0,
    /// trailing bits of the last byte zero-padded).
    fn read_bits_to_bytes(&mut self, n: usize) -> Option<Vec<u8>> {
        let mut out = vec![0u8; n.div_ceil(8)];
        for i in 0..n {
            let byte = self.bytes.get(self.pos / 8)?;
            let bit = (byte >> (7 - (self.pos % 8))) & 1;
            out[i / 8] |= bit << (7 - (i % 8));
            self.pos += 1;
        }
        Some(out)
    }
}

/// The mirror of `BitReader` — accumulates MSB-first bit fields, zero-padding
/// the final byte (RFC 4867 §4.3 requires the payload be padded to an octet
/// boundary).
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    fn write(&mut self, value: u8, n: usize) {
        for i in (0..n).rev() {
            let bit = (value >> i) & 1;
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let idx = self.bits / 8;
            self.bytes[idx] |= bit << (7 - (self.bits % 8));
            self.bits += 1;
        }
    }

    fn write_bits_from_bytes(&mut self, src: &[u8], n: usize) {
        for i in 0..n {
            let bit = (src[i / 8] >> (7 - (i % 8))) & 1;
            self.write(bit, 1);
        }
    }
}

/// CMR = 15 ("no mode request"): we never ask the far end to change its
/// encoding rate.
const CMR_NO_REQUEST: u8 = 0x0f;

/// Parse one RTP payload into the codec libraries' MIME-layout frame (header
/// byte + speech bytes), ready to hand to `amr_safe`'s decoder.
///
/// Returns `None` for a payload carrying no speech frame (comfort noise,
/// NO_DATA, or a truncated/garbled packet) — the caller should simply skip it
/// rather than treat it as an error, since these occur routinely on a live
/// call.
pub fn payload_to_frame(payload: &[u8], kind: AmrKind, octet_aligned: bool) -> Option<Vec<u8>> {
    if octet_aligned {
        // [CMR byte][ToC byte][speech bytes...] — the ToC byte and what
        // follows are already exactly the MIME layout.
        if payload.len() < 2 {
            return None;
        }
        let toc = payload[1];
        let frame_type = (toc >> 3) & 0x0f;
        speech_bits(kind, frame_type)?;
        return Some(payload[1..].to_vec());
    }

    // Bandwidth-efficient: CMR(4) | F(1) FT(4) Q(1) | speech bits | padding.
    let mut reader = BitReader::new(payload);
    let _cmr = reader.read(4)?;
    let _f = reader.read(1)?;
    let frame_type = reader.read(4)?;
    let quality = reader.read(1)?;
    let bits = speech_bits(kind, frame_type)?;
    let speech = reader.read_bits_to_bytes(bits)?;

    // Rebuild the MIME header byte the decoder expects (F=0: a single frame).
    let mut frame = Vec::with_capacity(1 + speech.len());
    frame.push((frame_type << 3) | (quality << 2));
    frame.extend_from_slice(&speech);
    Some(frame)
}

/// The inverse: wrap one encoder-produced MIME-layout frame as an RTP
/// payload in the negotiated framing.
pub fn frame_to_payload(frame: &[u8], kind: AmrKind, octet_aligned: bool) -> Option<Vec<u8>> {
    if frame.is_empty() {
        return None;
    }
    let frame_type = (frame[0] >> 3) & 0x0f;
    let quality = (frame[0] >> 2) & 1;
    let bits = speech_bits(kind, frame_type)?;

    if octet_aligned {
        // Prepend the CMR byte; the encoder's own header byte already *is*
        // the ToC entry.
        let mut payload = Vec::with_capacity(1 + frame.len());
        payload.push(CMR_NO_REQUEST << 4);
        payload.extend_from_slice(frame);
        return Some(payload);
    }

    let speech = frame.get(1..)?;
    if speech.len() < bits.div_ceil(8) {
        return None;
    }
    let mut writer = BitWriter::default();
    writer.write(CMR_NO_REQUEST, 4);
    writer.write(0, 1); // F=0: this is the last (only) frame
    writer.write(frame_type, 4);
    writer.write(quality, 1);
    writer.write_bits_from_bytes(speech, bits);
    Some(writer.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn octet_aligned_round_trip_preserves_the_frame() {
        // AMR-WB mode 2 (12.65kbps): 253 bits -> 32 speech bytes.
        let mut frame = vec![(2u8 << 3) | (1 << 2)];
        frame.extend((0..32u8).map(|i| i.wrapping_mul(7)));

        let payload = frame_to_payload(&frame, AmrKind::Wb, true).unwrap();
        assert_eq!(payload[0], 0xf0, "CMR byte");
        assert_eq!(&payload[1..], &frame[..]);

        let back = payload_to_frame(&payload, AmrKind::Wb, true).unwrap();
        assert_eq!(back, frame);
    }

    /// The one that matters: bandwidth-efficient shifts the speech data 10
    /// bits into the payload, so a round trip that loses or misaligns even
    /// one bit produces garbled audio rather than an outright failure.
    #[test]
    fn bandwidth_efficient_round_trip_preserves_the_frame() {
        for (kind, ft, speech_len) in [
            (AmrKind::Nb, 7u8, 244usize.div_ceil(8)), // 12.2kbps, 31 bytes
            (AmrKind::Nb, 0, 95usize.div_ceil(8)),    // 4.75kbps, 12 bytes
            (AmrKind::Wb, 2, 253usize.div_ceil(8)),   // 12.65kbps, 32 bytes
            (AmrKind::Wb, 8, 477usize.div_ceil(8)),   // 23.85kbps, 60 bytes
        ] {
            let bits = speech_bits(kind, ft).unwrap();
            let mut frame = vec![(ft << 3) | (1 << 2)];
            frame.extend((0..speech_len).map(|i| (i as u8).wrapping_mul(37)));
            // Zero the bits past the frame's real length — they are padding
            // and are not expected to survive a round trip.
            let tail = speech_len * 8 - bits;
            if tail > 0 {
                let last = frame.len() - 1;
                frame[last] &= !((1u8 << tail) - 1);
            }

            let payload = frame_to_payload(&frame, kind, false).unwrap();
            // 10 header bits + speech bits, padded up to a whole octet.
            assert_eq!(payload.len(), (10 + bits).div_ceil(8), "payload length");

            let back = payload_to_frame(&payload, kind, false).unwrap();
            assert_eq!(back, frame, "round trip for {kind:?} FT={ft}");
        }
    }

    #[test]
    fn bandwidth_efficient_payload_starts_with_the_cmr_and_toc_bits() {
        // FT=7, Q=1 -> CMR(1111) F(0) FT(0111) Q(1) = 1111 0011 11...
        let mut frame = vec![(7u8 << 3) | (1 << 2)];
        frame.extend(std::iter::repeat_n(0u8, 31));
        let payload = frame_to_payload(&frame, AmrKind::Nb, false).unwrap();
        assert_eq!(payload[0], 0b1111_0011);
        // Next bits: the trailing "1" of FT, then Q=1, then speech zeros.
        assert_eq!(payload[1] >> 6, 0b11);
    }

    #[test]
    fn non_speech_frame_types_are_skipped_rather_than_decoded() {
        // FT=8 is SID (comfort noise) for narrowband — not a speech frame.
        let payload = vec![0xf0, 8 << 3];
        assert!(payload_to_frame(&payload, AmrKind::Nb, true).is_none());
        // ...but FT=8 *is* a valid speech frame for wideband (23.85kbps).
        assert!(speech_bits(AmrKind::Wb, 8).is_some());
    }

    #[test]
    fn truncated_payloads_are_rejected_rather_than_panicking() {
        assert!(payload_to_frame(&[], AmrKind::Nb, false).is_none());
        assert!(payload_to_frame(&[0xf0], AmrKind::Nb, true).is_none());
        // Header claims a 12.2kbps frame but the speech bits are cut short.
        assert!(payload_to_frame(&[0b1111_0011, 0xff], AmrKind::Nb, false).is_none());
    }
}
