//! Octet-aligned ↔ bandwidth-efficient conversion (RFC 4867 §4.3/§4.4) that
//! never decodes anything.
//!
//! `ims::amr_rtp` converts to and from the codec libraries' layout and drops
//! comfort-noise frames; forwarding needs the opposite: every frame type kept
//! (speech, SID, NO_DATA), the codec mode request kept, and several frames per
//! packet supported. CRC, robust sorting and interleaving are never repacked:
//! compatibility (`params::compatible`) refuses them, so those calls are
//! transcoded.

use crate::ims::amr_rtp::{speech_bits, AmrKind, BitReader, BitWriter};

/// A frame's payload size in bits, for every frame type a relay may see.
/// `None` for a reserved type, which cannot be repacked because its length is
/// unknown.
fn frame_bits(kind: AmrKind, frame_type: u8) -> Option<usize> {
    if let Some(bits) = speech_bits(kind, frame_type) {
        return Some(bits);
    }
    match (kind, frame_type) {
        (AmrKind::Nb, 8) => Some(39),           // AMR SID
        (AmrKind::Wb, 9) => Some(40),           // AMR-WB SID
        (AmrKind::Wb, 14) | (_, 15) => Some(0), // SPEECH_LOST (WB), NO_DATA
        _ => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Frame {
    frame_type: u8,
    quality: u8,
    /// Payload bits, left-aligned, trailing pad bits zero.
    bits: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
struct Packet {
    cmr: u8,
    frames: Vec<Frame>,
}

/// Zero the unused low bits of the last byte of a `n`-bit field.
fn canonical(mut bytes: Vec<u8>, n: usize) -> Vec<u8> {
    if !n.is_multiple_of(8) {
        if let Some(last) = bytes.last_mut() {
            *last &= 0xffu8 << (8 - n % 8);
        }
    }
    bytes
}

fn parse(payload: &[u8], kind: AmrKind, octet_aligned: bool) -> Option<Packet> {
    if octet_aligned {
        let cmr = payload.first()? >> 4;
        let mut toc = Vec::new();
        let mut at = 1;
        loop {
            let entry = *payload.get(at)?;
            at += 1;
            toc.push(((entry >> 3) & 0x0f, (entry >> 2) & 1));
            if entry & 0x80 == 0 {
                break;
            }
        }
        let mut frames = Vec::with_capacity(toc.len());
        for (frame_type, quality) in toc {
            let bits = frame_bits(kind, frame_type)?;
            let len = bits.div_ceil(8);
            let data = payload.get(at..at + len)?.to_vec();
            at += len;
            frames.push(Frame {
                frame_type,
                quality,
                bits: canonical(data, bits),
            });
        }
        // Octet-aligned payloads are exact; anything left is not AMR we parsed.
        (at == payload.len()).then_some(Packet { cmr, frames })
    } else {
        let mut r = BitReader::new(payload);
        let cmr = r.read(4)?;
        let mut toc = Vec::new();
        loop {
            let follow = r.read(1)?;
            let frame_type = r.read(4)?;
            let quality = r.read(1)?;
            toc.push((frame_type, quality));
            if follow == 0 {
                break;
            }
        }
        let mut frames = Vec::with_capacity(toc.len());
        for (frame_type, quality) in toc {
            let bits = frame_bits(kind, frame_type)?;
            frames.push(Frame {
                frame_type,
                quality,
                bits: r.read_bits_to_bytes(bits)?,
            });
        }
        Some(Packet { cmr, frames })
    }
}

fn build(packet: &Packet, kind: AmrKind, octet_aligned: bool) -> Option<Vec<u8>> {
    let last = packet.frames.len().checked_sub(1)?;
    if octet_aligned {
        let mut out = vec![packet.cmr << 4];
        for (i, f) in packet.frames.iter().enumerate() {
            let follow = u8::from(i != last) << 7;
            out.push(follow | (f.frame_type << 3) | (f.quality << 2));
        }
        for f in &packet.frames {
            let bits = frame_bits(kind, f.frame_type)?;
            out.extend_from_slice(f.bits.get(..bits.div_ceil(8))?);
        }
        Some(out)
    } else {
        let mut w = BitWriter::default();
        w.write(packet.cmr, 4);
        for (i, f) in packet.frames.iter().enumerate() {
            w.write(u8::from(i != last), 1);
            w.write(f.frame_type, 4);
            w.write(f.quality, 1);
        }
        for f in &packet.frames {
            let bits = frame_bits(kind, f.frame_type)?;
            if f.bits.len() < bits.div_ceil(8) {
                return None;
            }
            w.write_bits_from_bytes(&f.bits, bits);
        }
        Some(w.bytes)
    }
}

/// Convert one RTP payload between packings, frame for frame. Same packing is
/// a plain copy (the caller usually skips the call). `None` for a truncated or
/// garbled payload, or a reserved frame type; the relay drops such a packet
/// rather than forward something the receiver cannot parse.
pub fn repack(
    payload: &[u8],
    kind: AmrKind,
    from_octet_aligned: bool,
    to_octet_aligned: bool,
) -> Option<Vec<u8>> {
    if from_octet_aligned == to_octet_aligned {
        return Some(payload.to_vec());
    }
    let packet = parse(payload, kind, from_octet_aligned)?;
    build(&packet, kind, to_octet_aligned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ims::amr_rtp::payload_to_frame;

    /// Every frame type the relay may see, per flavour.
    fn frame_types(kind: AmrKind) -> Vec<u8> {
        match kind {
            AmrKind::Nb => (0..=8).chain([15]).collect(),
            AmrKind::Wb => (0..=9).chain([14, 15]).collect(),
        }
    }

    /// A deterministic frame of type `ft` whose trailing pad bits are zero.
    fn frame(kind: AmrKind, ft: u8, seed: u8) -> Frame {
        let bits = frame_bits(kind, ft).unwrap();
        let bytes: Vec<u8> = (0..bits.div_ceil(8))
            .map(|i| (i as u8).wrapping_mul(37).wrapping_add(seed) | 1)
            .collect();
        Frame {
            frame_type: ft,
            quality: 1,
            bits: canonical(bytes, bits),
        }
    }

    fn packet(kind: AmrKind, fts: &[u8], cmr: u8) -> Packet {
        Packet {
            cmr,
            frames: fts
                .iter()
                .enumerate()
                .map(|(i, &ft)| frame(kind, ft, i as u8 * 11))
                .collect(),
        }
    }

    #[test]
    fn round_trip_is_identical_for_every_frame_type_and_frame_count() {
        for kind in [AmrKind::Nb, AmrKind::Wb] {
            for ft in frame_types(kind) {
                for count in 1..=3usize {
                    for cmr in [0u8, 7, 15] {
                        let p = packet(kind, &vec![ft; count], cmr);
                        let oa = build(&p, kind, true).unwrap();
                        let be = build(&p, kind, false).unwrap();

                        assert_eq!(
                            repack(&oa, kind, true, false).unwrap(),
                            be,
                            "{kind:?} ft={ft} n={count}"
                        );
                        assert_eq!(
                            repack(&be, kind, false, true).unwrap(),
                            oa,
                            "{kind:?} ft={ft} n={count}"
                        );
                        // a -> b -> a
                        let there = repack(&oa, kind, true, false).unwrap();
                        assert_eq!(repack(&there, kind, false, true).unwrap(), oa);
                        assert_eq!(
                            parse(&be, kind, false).unwrap(),
                            p,
                            "CMR and frame types survive"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn mixed_frame_types_in_one_packet_round_trip() {
        let p = packet(AmrKind::Wb, &[2, 9, 15], 3);
        let oa = build(&p, AmrKind::Wb, true).unwrap();
        let be = repack(&oa, AmrKind::Wb, true, false).unwrap();
        assert_eq!(parse(&be, AmrKind::Wb, false).unwrap(), p);
        assert_eq!(repack(&be, AmrKind::Wb, false, true).unwrap(), oa);
    }

    #[test]
    fn the_codec_mode_request_is_preserved_not_reset() {
        let p = packet(AmrKind::Wb, &[2], 5);
        let oa = build(&p, AmrKind::Wb, true).unwrap();
        assert_eq!(oa[0] >> 4, 5);
        let be = repack(&oa, AmrKind::Wb, true, false).unwrap();
        assert_eq!(be[0] >> 4, 5);
    }

    #[test]
    fn same_packing_is_a_byte_copy() {
        let bytes = vec![0xf0, 0x14, 1, 2, 3];
        assert_eq!(repack(&bytes, AmrKind::Wb, true, true).unwrap(), bytes);
    }

    #[test]
    fn speech_bits_match_the_existing_decoder_path() {
        // The same single speech frame through `ims::amr_rtp` and through the
        // repacker must carry identical speech bits.
        for kind in [AmrKind::Nb, AmrKind::Wb] {
            for ft in frame_types(kind) {
                if speech_bits(kind, ft).is_none() {
                    continue;
                }
                let p = packet(kind, &[ft], 15);
                let oa = build(&p, kind, true).unwrap();
                let be = repack(&oa, kind, true, false).unwrap();
                assert_eq!(
                    payload_to_frame(&oa, kind, true),
                    payload_to_frame(&be, kind, false),
                    "{kind:?} ft={ft}"
                );
            }
        }
    }

    #[test]
    fn truncated_or_garbled_input_is_rejected() {
        for kind in [AmrKind::Nb, AmrKind::Wb] {
            assert!(repack(&[], kind, true, false).is_none());
            assert!(repack(&[], kind, false, true).is_none());
            assert!(repack(&[0xf0], kind, true, false).is_none(), "no ToC");
        }
        // Wideband 12.65k frame cut short.
        let p = packet(AmrKind::Wb, &[2], 15);
        let oa = build(&p, AmrKind::Wb, true).unwrap();
        assert!(repack(&oa[..oa.len() - 1], AmrKind::Wb, true, false).is_none());
        let be = build(&p, AmrKind::Wb, false).unwrap();
        assert!(repack(&be[..be.len() - 2], AmrKind::Wb, false, true).is_none());
        // Trailing bytes after an octet-aligned packet are not AMR.
        let mut long = oa.clone();
        long.push(0);
        assert!(repack(&long, AmrKind::Wb, true, false).is_none());
    }

    #[test]
    fn a_reserved_frame_type_cannot_be_repacked() {
        // Narrowband FT 12 is reserved; its length is unknown.
        let payload = vec![0xf0, 12 << 3, 0, 0];
        assert!(repack(&payload, AmrKind::Nb, true, false).is_none());
    }
}
