//! Decodes a 3GPP SMS-over-IP body (`Content-Type: application/vnd.3gpp.sms`,
//! TS 24.341 §7.1.1.1) into the sender and text a person can actually read.
//!
//! The body is an **RP-DATA** message (TS 24.011 §7.3.1) wrapping an
//! **SMS-DELIVER TPDU** (TS 23.040 §9.2.2.1) — the same PDU the circuit-switched
//! path never has to decode, because `AT+CMGF=1` (text mode, `sms/reader.rs`)
//! makes the modem do it. Nothing does it for the IMS path, and nothing needed
//! to until a real carrier actually delivered an SMS over it: forwarding
//! `req.body` as if it were text produced unreadable output (the reported
//! symptom), and the raw bytes are not incidentally-garbled text — they are a
//! binary PDU that has never been decoded at all.
//!
//! Scope: the common case (GSM 7-bit default alphabet or UCS-2, with or
//! without a concatenation UDH). Not implemented: national-language
//! locking/single-shift tables (TS 23.038 §6.2.1.2/6.2.1.3 — vanishingly rare
//! in practice), compressed user data, and reassembling a concatenated
//! message's parts (each part is its own SIP `MESSAGE` transaction; decoded
//! individually and labelled `part`, not buffered and joined).

/// One decoded SMS. `sender` is the real originating number from the TPDU's
/// TP-OA — **not** the SIP `From`, which on a real network names an IMS
/// network element (an SMSC gateway's own hostname), not the person who sent
/// the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedSms {
    pub sender: String,
    pub text: String,
    /// `Some((sequence, total))` if the UDH marked this as one part of a
    /// concatenated message — 1-indexed, as the UDH itself encodes it.
    pub part: Option<(u8, u8)>,
}

/// Top-level entry point: an RP-DATA envelope in, a readable message out.
pub fn decode_vnd_3gpp_sms(body: &[u8]) -> Result<DecodedSms, String> {
    let rp = RpData::parse(body)?;
    let tpdu = SmsDeliverTpdu::parse(rp.user_data)?;
    Ok(DecodedSms {
        sender: tpdu.originating_address,
        text: tpdu.text,
        part: tpdu.part,
    })
}

/// The RP-DATA (network→MS) envelope: message type/reference, the SC address
/// (unused — we want the *originator*, from the TPDU inside, not the SC that
/// relayed it), and a length-prefixed RP-User-Data field holding the TPDU.
struct RpData<'a> {
    user_data: &'a [u8],
}

impl<'a> RpData<'a> {
    fn parse(buf: &'a [u8]) -> Result<Self, String> {
        // Octet 0: RP-Message-Type-Indicator + spare bits. Octet 1:
        // RP-Message-Reference. Both consumed but not otherwise used — the
        // structure below (length-prefixed fields) is what we actually walk,
        // so getting the exact MTI encoding right doesn't gate decoding.
        let mut pos = 2usize;
        pos = skip_length_prefixed(buf, pos, "RP-Originator-Address")?;
        pos = skip_length_prefixed(buf, pos, "RP-Destination-Address")?;

        let ud_len = *buf
            .get(pos)
            .ok_or("RP-DATA truncated before RP-User-Data length")? as usize;
        pos += 1;
        let user_data = buf
            .get(pos..pos + ud_len)
            .ok_or("RP-DATA's RP-User-Data length exceeds the buffer")?;
        Ok(Self { user_data })
    }
}

/// Advance past a `[length][length bytes]` field, returning the offset just
/// after it. Used for both RP-layer address fields — for RP-DATA
/// (network→MS) the destination address is always absent (length 0), but the
/// walk is identical either way since the length prefix says how far to skip.
fn skip_length_prefixed(buf: &[u8], pos: usize, field: &str) -> Result<usize, String> {
    let len = *buf
        .get(pos)
        .ok_or_else(|| format!("truncated before {field}"))? as usize;
    let end = pos + 1 + len;
    if end > buf.len() {
        return Err(format!("{field}'s length exceeds the buffer"));
    }
    Ok(end)
}

struct SmsDeliverTpdu {
    originating_address: String,
    text: String,
    part: Option<(u8, u8)>,
}

impl SmsDeliverTpdu {
    fn parse(buf: &[u8]) -> Result<Self, String> {
        let first_octet = *buf.first().ok_or("empty TPDU")?;
        // TS 23.040 §9.2.3.23: bit 6 = TP-UDHI (a User Data Header is
        // present, prepended to TP-UD).
        let udhi = (first_octet & 0x40) != 0;

        let mut pos = 1usize;
        let oa_digit_count = *buf.get(pos).ok_or("TPDU truncated before TP-OA length")? as usize;
        pos += 1;
        let oa_type = *buf.get(pos).ok_or("TPDU truncated before TP-OA type")?;
        pos += 1;
        let oa_octets = oa_digit_count.div_ceil(2);
        let oa_bytes = buf
            .get(pos..pos + oa_octets)
            .ok_or("TP-OA digits exceed the buffer")?;
        pos += oa_octets;
        let originating_address = decode_address(oa_type, oa_digit_count, oa_bytes);

        // TP-PID (1 octet): message-handling hints (e.g. a status report,
        // a replace-message request) we don't act on — skip.
        pos += 1;
        let dcs = *buf.get(pos).ok_or("TPDU truncated before TP-DCS")?;
        pos += 1;
        // TP-SCTS (7 octets, semi-octet BCD timestamp): informational only,
        // not needed to render the text.
        pos += 7;

        let udl = *buf.get(pos).ok_or("TPDU truncated before TP-UDL")? as usize;
        pos += 1;
        let ud = buf.get(pos..).ok_or("TPDU truncated at TP-UD")?;

        let alphabet = Alphabet::from_dcs(dcs);
        let (part, text) = decode_user_data(ud, udl, alphabet, udhi)?;

        Ok(Self {
            originating_address,
            text,
            part,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Alphabet {
    Gsm7,
    Octet,
    Ucs2,
}

impl Alphabet {
    /// TS 23.038 §4: only the two coding-scheme groups real MT traffic
    /// overwhelmingly uses. Anything else (compressed, national-language
    /// shift tables, message-waiting indication groups) falls back to GSM7 —
    /// wrong for genuinely rare encodings, but never a crash, and right for
    /// the vast majority of real messages.
    fn from_dcs(dcs: u8) -> Self {
        if dcs & 0xC0 == 0x00 {
            // General Data Coding group: bits 3-2 select the alphabet.
            match (dcs >> 2) & 0x03 {
                1 => Alphabet::Octet,
                2 => Alphabet::Ucs2,
                _ => Alphabet::Gsm7,
            }
        } else if dcs & 0xF0 == 0xF0 {
            // Data coding / message class group: bit 2 selects the alphabet
            // (7-bit vs 8-bit; this group has no UCS-2 option).
            if dcs & 0x04 != 0 {
                Alphabet::Octet
            } else {
                Alphabet::Gsm7
            }
        } else {
            Alphabet::Gsm7
        }
    }
}

/// Splits `ud` into an optional concatenation UDH and the message text,
/// decoded per `alphabet`.
///
/// The GSM7 case carries the one genuinely fiddly rule (TS 23.040 §9.2.3.24):
/// `udl` counts **septets**, including whatever septets the UDH's octets
/// occupy once padded out to a septet boundary — the text does not start at
/// an octet boundary in general, it starts at the next *septet* boundary.
/// Unpacking every septet the length claims and then dropping the ones the
/// UDH occupies handles this without having to reason about the padding bits
/// directly: they fall inside the discarded septets.
fn decode_user_data(
    ud: &[u8],
    udl: usize,
    alphabet: Alphabet,
    udhi: bool,
) -> Result<(Option<(u8, u8)>, String), String> {
    let (part, udh_octets) = if udhi {
        let udhl = *ud.first().ok_or("TP-UDHI set but TP-UD is empty")? as usize;
        let udh = ud.get(1..1 + udhl).ok_or("UDH length exceeds TP-UD")?;
        (parse_concatenation_udh(udh), 1 + udhl)
    } else {
        (None, 0)
    };

    let text = match alphabet {
        Alphabet::Gsm7 => {
            let fill_septets = (udh_octets * 8).div_ceil(7);
            let septets = unpack_septets(ud, udl);
            let text_septets = septets.get(fill_septets..).unwrap_or(&[]);
            decode_gsm7(text_septets)
        }
        Alphabet::Ucs2 => {
            let text_bytes = ud.get(udh_octets..udl).unwrap_or(&[]);
            decode_ucs2(text_bytes)
        }
        Alphabet::Octet => {
            let text_bytes = ud.get(udh_octets..udl).unwrap_or(&[]);
            String::from_utf8_lossy(text_bytes).into_owned()
        }
    };

    Ok((part, text))
}

/// Recognises the two concatenation IEIs (TS 23.040 §9.2.3.24.1): `0x00`
/// (8-bit reference) and `0x08` (16-bit reference). Any other IE
/// (national-language shift tables, port addressing, ...) is skipped —
/// this only exists to label multi-part messages, not to act on every IE.
fn parse_concatenation_udh(udh: &[u8]) -> Option<(u8, u8)> {
    let mut pos = 0;
    while pos + 1 < udh.len() {
        let iei = udh[pos];
        let ie_len = udh[pos + 1] as usize;
        let ie = udh.get(pos + 2..pos + 2 + ie_len)?;
        match iei {
            0x00 if ie.len() >= 3 => return Some((ie[2], ie[1])),
            0x08 if ie.len() >= 4 => return Some((ie[3], ie[2])),
            _ => {}
        }
        pos += 2 + ie_len;
    }
    None
}

/// TS 23.040 §9.1.2.5: bits 6-4 of the type-of-address octet name the
/// numbering scheme; `0b001` is "international", which is the one case
/// worth a display convention (a leading `+`) since everything else is
/// scheme-specific and not safe to assume a format for.
fn decode_address(type_of_address: u8, digit_count: usize, bytes: &[u8]) -> String {
    // Alphanumeric originator (a service name instead of a number): TP-OA is
    // GSM7-packed text, and `digit_count` is a *semi-octet* count that
    // doubles as a septet-count-in-nibbles for this encoding — halve it for
    // the septet count TS 23.040 §9.1.2.5 actually intends.
    if (type_of_address >> 4) & 0x07 == 0b101 {
        let septets = unpack_septets(bytes, digit_count * 4 / 7);
        return decode_gsm7(&septets);
    }

    let mut digits = String::with_capacity(digit_count + 1);
    if (type_of_address >> 4) & 0x07 == 0b001 {
        digits.push('+');
    }
    'outer: for &byte in bytes {
        for nibble in [byte & 0x0F, byte >> 4] {
            if digits.len() >= digit_count + usize::from(digits.starts_with('+')) {
                break 'outer;
            }
            match nibble {
                0..=9 => digits.push((b'0' + nibble) as char),
                0xF => break 'outer,   // fill nibble on an odd digit count
                _ => digits.push('?'), // *, #, a/b/c — not a phone digit
            }
        }
    }
    digits
}

/// TS 23.038 §6.1.2.1: unpack `count` 7-bit septets from octet-packed `data`.
/// A septet's bits can straddle two octets, so each is read as a 7-bit
/// window at bit offset `k * 7` rather than assumed to align to octets.
fn unpack_septets(data: &[u8], count: usize) -> Vec<u8> {
    let mut septets = Vec::with_capacity(count);
    for k in 0..count {
        let bit_offset = k * 7;
        let byte_index = bit_offset / 8;
        let bit_index = bit_offset % 8;
        let Some(&low_byte) = data.get(byte_index) else {
            break;
        };
        let low = u16::from(low_byte) >> bit_index;
        let high = match data.get(byte_index + 1) {
            Some(&b) if bit_index > 0 => u16::from(b) << (8 - bit_index),
            _ => 0,
        };
        septets.push(((low | high) & 0x7F) as u8);
    }
    septets
}

/// TS 23.038 §6.2.1: the default alphabet, plus the handful of extension-table
/// entries (escaped with `0x1B`) real text messages actually use. An escape
/// this doesn't recognise decodes as a space rather than failing the whole
/// message — a wrong character beats losing the rest of the text over it.
fn decode_gsm7(septets: &[u8]) -> String {
    const DEFAULT_ALPHABET: [char; 128] = [
        '@', '£', '$', '¥', 'è', 'é', 'ù', 'ì', 'ò', 'Ç', '\n', 'Ø', 'ø', '\r', 'Å', 'å', 'Δ', '_',
        'Φ', 'Γ', 'Λ', 'Ω', 'Π', 'Ψ', 'Σ', 'Θ', 'Ξ', '\u{1B}', 'Æ', 'æ', 'ß', 'É', ' ', '!', '"',
        '#', '¤', '%', '&', '\'', '(', ')', '*', '+', ',', '-', '.', '/', '0', '1', '2', '3', '4',
        '5', '6', '7', '8', '9', ':', ';', '<', '=', '>', '?', '¡', 'A', 'B', 'C', 'D', 'E', 'F',
        'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X',
        'Y', 'Z', 'Ä', 'Ö', 'Ñ', 'Ü', '§', '¿', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j',
        'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z', 'ä', 'ö',
        'ñ', 'ü', 'à',
    ];
    let mut out = String::with_capacity(septets.len());
    let mut chars = septets.iter();
    while let Some(&sep) = chars.next() {
        if sep == 0x1B {
            let ext = chars.next().copied().unwrap_or(0);
            out.push(match ext {
                0x14 => '^',
                0x28 => '{',
                0x29 => '}',
                0x2F => '\\',
                0x3C => '[',
                0x3D => '~',
                0x3E => ']',
                0x40 => '|',
                0x65 => '€',
                _ => ' ',
            });
        } else {
            out.push(DEFAULT_ALPHABET[(sep & 0x7F) as usize]);
        }
    }
    out
}

/// TS 23.038 §6.2.2: UCS-2, i.e. big-endian UTF-16 code units. Real SMS never
/// carries a surrogate pair (UCS-2 predates them), so this decodes one code
/// unit at a time; an unpaired surrogate (from malformed input) becomes
/// U+FFFD rather than corrupting the rest of the string.
fn decode_ucs2(bytes: &[u8]) -> String {
    bytes
        .chunks(2)
        .map(|pair| {
            let unit = match pair {
                [hi, lo] => u16::from_be_bytes([*hi, *lo]),
                [hi] => u16::from(*hi) << 8,
                _ => unreachable!(),
            };
            char::from_u32(u32::from(unit)).unwrap_or('\u{FFFD}')
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inverse of `unpack_septets`, kept test-only: production code never
    /// needs to *encode* a PDU, only decode one a carrier sent, but a
    /// synthetic PDU that round-trips through both is the only way to check
    /// the unpacking bit arithmetic without a captured reference PDU for
    /// every case (concatenation, UCS-2, ...).
    fn pack_septets(septets: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut acc: u16 = 0;
        let mut acc_bits = 0u32;
        for &sep in septets {
            acc |= u16::from(sep) << acc_bits;
            acc_bits += 7;
            while acc_bits >= 8 {
                bytes.push((acc & 0xFF) as u8);
                acc >>= 8;
                acc_bits -= 8;
            }
        }
        if acc_bits > 0 {
            bytes.push((acc & 0xFF) as u8);
        }
        bytes
    }

    fn gsm7_encode(text: &str) -> Vec<u8> {
        const DEFAULT_ALPHABET: &str = "@£$¥èéùìòÇ\nØø\rÅåΔ_ΦΓΛΩΠΨΣΘΞ\u{1B}Ææß\
             É !\"#¤%&'()*+,-./0123456789:;<=>?¡ABCDEFGHIJKLMNOPQRSTUVWXYZÄÖÑÜ§¿\
             abcdefghijklmnopqrstuvwxyzäöñüà";
        text.chars()
            .map(|c| {
                DEFAULT_ALPHABET
                    .chars()
                    .position(|a| a == c)
                    .expect("test text must be in the default alphabet") as u8
            })
            .collect()
    }

    fn bcd_encode(digits: &str) -> Vec<u8> {
        let nibbles: Vec<u8> = digits.bytes().map(|b| b - b'0').collect();
        nibbles
            .chunks(2)
            .map(|pair| match pair {
                [a, b] => b << 4 | a,
                [a] => 0xF0 | a,
                _ => unreachable!(),
            })
            .collect()
    }

    /// A minimal, hand-built RP-DATA + SMS-DELIVER TPDU: no UDH, GSM7 text,
    /// international sender. Exercises the whole decode path end to end —
    /// this is the shape every ordinary MT text takes.
    #[test]
    fn decodes_a_plain_gsm7_message_with_an_international_sender() {
        let sender_digits = "919876543210";
        let oa_bytes = bcd_encode(sender_digits);
        let text_septets = gsm7_encode("Hello");

        let mut tpdu = vec![0x00]; // first octet: DELIVER, no UDHI
        tpdu.push(sender_digits.len() as u8); // TP-OA length, in digits
        tpdu.push(0x91); // international
        tpdu.extend_from_slice(&oa_bytes);
        tpdu.push(0x00); // TP-PID
        tpdu.push(0x00); // TP-DCS: GSM7, general group
        tpdu.extend_from_slice(&[0u8; 7]); // TP-SCTS, not asserted on
        tpdu.push(text_septets.len() as u8); // TP-UDL, in septets
        tpdu.extend_from_slice(&pack_septets(&text_septets));

        let mut rp = vec![0x01, 0x00]; // RP-MTI (net->MS), RP-MR
        rp.push(0); // RP-OA length (SC address) — irrelevant to this decoder
        rp.push(0); // RP-DA length — absent for MT
        rp.push(tpdu.len() as u8); // RP-UD length
        rp.extend_from_slice(&tpdu);

        let decoded = decode_vnd_3gpp_sms(&rp).unwrap();
        assert_eq!(decoded.sender, "+919876543210");
        assert_eq!(decoded.text, "Hello");
        assert_eq!(decoded.part, None);
    }

    #[test]
    fn decodes_ucs2_text() {
        let text_bytes: Vec<u8> = "Hi".encode_utf16().flat_map(u16::to_be_bytes).collect();

        let mut tpdu = vec![0x00];
        tpdu.push(0); // TP-OA length 0 — sender not under test here
        tpdu.push(0x81); // unknown/national numbering, no digits to decode
        tpdu.push(0x00); // TP-PID
        tpdu.push(0x08); // TP-DCS: general group, UCS-2
        tpdu.extend_from_slice(&[0u8; 7]);
        tpdu.push(text_bytes.len() as u8); // TP-UDL, in OCTETS for UCS-2
        tpdu.extend_from_slice(&text_bytes);

        let mut rp = vec![0x01, 0x00, 0, 0];
        rp.push(tpdu.len() as u8);
        rp.extend_from_slice(&tpdu);

        assert_eq!(decode_vnd_3gpp_sms(&rp).unwrap().text, "Hi");
    }

    /// Same packing algorithm as `pack_septets`, but starting `leading_zero_bits`
    /// bits into the stream instead of at bit 0 — the shape a UDH's fill
    /// padding actually takes on the wire: the UDH is whole raw octets, then
    /// zero bits up to the next septet boundary, then the text septets.
    fn pack_septets_with_offset(septets: &[u8], leading_zero_bits: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut acc: u16 = 0;
        let mut acc_bits = leading_zero_bits;
        for &sep in septets {
            acc |= u16::from(sep) << acc_bits;
            acc_bits += 7;
            while acc_bits >= 8 {
                bytes.push((acc & 0xFF) as u8);
                acc >>= 8;
                acc_bits -= 8;
            }
        }
        if acc_bits > 0 {
            bytes.push((acc & 0xFF) as u8);
        }
        bytes
    }

    /// The fiddly case: a concatenation UDH followed by GSM7 text that does
    /// NOT start on an octet boundary — this is what actually exercises the
    /// septet-fill-bit handling in `decode_user_data`, unlike the no-UDH test.
    #[test]
    fn decodes_a_concatenated_part_and_reports_its_sequence() {
        let text_septets = gsm7_encode("part2");
        let udh = [0x00, 0x03, 0xAA, 0x02, 0x02]; // IEI 0x00, ref=0xAA, total=2, seq=2
        let udhl_octets = udh.len();
        let udh_total_octets = 1 + udhl_octets; // + the UDHL length byte itself

        let fill_septets = (udh_total_octets * 8).div_ceil(7);
        let fill_bits = (fill_septets * 7 - udh_total_octets * 8) as u32;

        let mut tp_ud = vec![udhl_octets as u8];
        tp_ud.extend_from_slice(&udh);
        tp_ud.extend_from_slice(&pack_septets_with_offset(&text_septets, fill_bits));

        let mut tpdu = vec![0x40]; // DELIVER, TP-UDHI set
        tpdu.push(0); // TP-OA length 0 — sender not under test here
        tpdu.push(0x81);
        tpdu.push(0x00); // TP-PID
        tpdu.push(0x00); // TP-DCS: GSM7
        tpdu.extend_from_slice(&[0u8; 7]);
        tpdu.push((fill_septets + text_septets.len()) as u8); // TP-UDL, in septets
        tpdu.extend_from_slice(&tp_ud);

        let mut rp = vec![0x01, 0x00, 0, 0];
        rp.push(tpdu.len() as u8);
        rp.extend_from_slice(&tpdu);

        let decoded = decode_vnd_3gpp_sms(&rp).unwrap();
        assert_eq!(decoded.part, Some((2, 2)));
        assert_eq!(decoded.text, "part2");
    }

    #[test]
    fn truncated_input_is_an_error_not_a_panic() {
        assert!(decode_vnd_3gpp_sms(&[]).is_err());
        assert!(decode_vnd_3gpp_sms(&[0x01, 0x00, 0, 0, 200]).is_err());
    }
}
