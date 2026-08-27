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
//!
//! The other half of the module is [`build_rp_ack`], the delivery report owed
//! back to the network for every message decoded here. The two are joined by
//! [`DecodedSms::rp_mr`], which the report echoes.

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
    /// The envelope's RP-Message-Reference (TS 24.011 §7.3.1, octet 1),
    /// echoed verbatim in the delivery report so the network can match the
    /// acknowledgement to this specific delivery. See [`build_rp_ack`].
    pub rp_mr: u8,
    /// A **Short Message Type 0** (TS 23.040 §9.2.3.9): a silent probe the
    /// network sends to test whether the subscriber is reachable. It must be
    /// acknowledged and its contents discarded — never stored, never shown.
    /// `text` is still decoded (it costs nothing and makes the probe legible
    /// in a trace), but a caller must not treat it as a message.
    pub is_type_zero: bool,
}

/// What a 3GPP SMS-over-IP body's RP envelope actually turned out to be.
///
/// A `MESSAGE` on this transport is not always a deliverable short message —
/// see [`decode_vnd_3gpp_sms`]'s docs for why the RP-Message-Type-Indicator
/// has to be checked before the rest of the envelope is trusted to mean
/// what `Message`'s layout assumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedRp {
    /// RP-DATA (network→MS): an actual short message, decoded.
    Message(DecodedSms),
    /// RP-ACK (network→MS, TS 24.011 §7.3.3): acknowledges an RP-DATA *we*
    /// submitted. This bridge never submits an SMS over IMS itself — it only
    /// answers what the network delivers — so a well-behaved peer never sends
    /// this; it exists here so a peer that does gets logged and ignored
    /// rather than misread as a message.
    Ack { rp_mr: u8 },
    /// RP-ERROR (network→MS, TS 24.011 §7.3.4): the network refusing an
    /// RP-DATA *we* submitted, with a §8.2.5.4 cause octet when present.
    /// Same "nothing to forward" as `Ack`.
    Error { rp_mr: u8, cause: Option<u8> },
    /// specs/045 SMS-02: the RP-DATA envelope was fine, but the TPDU inside
    /// it isn't SMS-DELIVER (its own TP-MTI says SMS-SUBMIT-REPORT or
    /// SMS-STATUS-REPORT) — recognized, not garbled, and not a deliverable
    /// message either. Same "nothing to forward" treatment as `Ack`/`Error`;
    /// the RP-DATA itself was still received, so the caller still owes an
    /// RP-ACK, not an RP-ERROR.
    UnsupportedTpdu { rp_mr: u8, kind: TpduMessageType },
    /// specs/045 SMS-03: the TPDU claimed to be SMS-DELIVER but its bytes
    /// don't parse as one (truncated/malformed) — a genuine decode
    /// failure, unlike `UnsupportedTpdu`. The caller sends an RP-ERROR
    /// instead of relaying `req.body` as if it were text.
    Undecodable { rp_mr: u8 },
}

/// TS 23.040 §9.2.3.1, SC→MS direction (the only direction this bridge
/// ever receives a TPDU in): the first octet's low 2 bits (TP-MTI) say what
/// shape the rest of the TPDU is — only `Deliver` has the TP-OA/TP-PID/
/// TP-DCS/TP-SCTS/TP-UDL layout `SmsDeliverTpdu` assumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpduMessageType {
    Deliver,
    SubmitReport,
    StatusReport,
    Reserved,
}

impl TpduMessageType {
    fn from_first_octet(first_octet: u8) -> Self {
        match first_octet & 0x03 {
            0b00 => Self::Deliver,
            0b01 => Self::SubmitReport,
            0b10 => Self::StatusReport,
            _ => Self::Reserved,
        }
    }
}

/// Top-level entry point: an RP envelope in, what it actually was out.
///
/// # Why the RP-Message-Type-Indicator has to be checked first
///
/// TS 24.011 table 8.4 gives the envelope's first octet seven possible
/// meanings — RP-DATA, RP-ACK and RP-ERROR, each in both directions — and
/// only one of them (RP-DATA, network→MS) has the
/// address/address/user-data-length layout [`RpData`] and
/// [`SmsDeliverTpdu`] assume. RP-ACK and RP-ERROR (network→MS) carry no
/// address fields at all: walking their bytes as if they were RP-DATA reads
/// an unrelated octet (the RP-Cause length, for RP-ERROR) as an address
/// length, desyncs everything after it, and can hand `SmsDeliverTpdu::parse`
/// a garbled slice that occasionally still parses — producing a plausible
/// sender and body for a "message" nobody sent. Reading the MTI first is
/// what tells these apart before any of that walking starts.
pub fn decode_vnd_3gpp_sms(body: &[u8]) -> Result<DecodedRp, String> {
    match RpMessage::parse(body)? {
        RpMessage::Data(rp) => {
            // Classified before ever calling into the SMS-DELIVER walker
            // (specs/045 SMS-02): a non-Deliver TPDU is recognized, not
            // garbled — the RP-DATA itself was still received, so it still
            // gets an RP-ACK (via `UnsupportedTpdu`), just never a decoded
            // message.
            let kind = rp
                .user_data
                .first()
                .map(|&b| TpduMessageType::from_first_octet(b))
                .unwrap_or(TpduMessageType::Reserved);
            if kind != TpduMessageType::Deliver {
                return Ok(DecodedRp::UnsupportedTpdu { rp_mr: rp.mr, kind });
            }
            match decode_sms_deliver_tpdu(rp.user_data) {
                Ok(mut decoded) => {
                    decoded.rp_mr = rp.mr;
                    Ok(DecodedRp::Message(decoded))
                }
                // specs/045 SMS-03: TP-MTI already confirmed SMS-DELIVER, so
                // this is a genuine malformation, not a recognized-other
                // type — the caller sends an RP-ERROR, not an RP-ACK.
                Err(_) => Ok(DecodedRp::Undecodable { rp_mr: rp.mr }),
            }
        }
        RpMessage::Ack { mr } => Ok(DecodedRp::Ack { rp_mr: mr }),
        RpMessage::Error { mr, cause } => Ok(DecodedRp::Error { rp_mr: mr, cause }),
    }
}

/// Decodes a raw SMS-DELIVER TPDU (TS 23.040 §9.2.2.1) with no RP-layer
/// envelope around it at all — the shape `AT+CMGR`/`AT+CMGL` PDU mode hands
/// back (TS 27.005 §3.1: a length-prefixed SMSC-address field, stripped by
/// the caller, then the TPDU verbatim), unlike [`decode_vnd_3gpp_sms`]'s
/// RP-DATA-wrapped IMS `MESSAGE` body.
///
/// Shared so `sms::reader` (the modem-storage delivery route) decodes with
/// the exact same TPDU parser the IMS route uses, rather than a second,
/// weaker one built on `AT+CMGF=1` text mode (specs/041 conformance review,
/// CS-01/CS-02): text mode cannot represent UCS-2 (comes back as a hex
/// string) or expose the UDH a concatenated message needs, and the two
/// routes must agree on what a message *is* or `volte::sms::Dedupe` cannot
/// tell one delivery of the same text from the other.
///
/// `rp_mr` on the returned `DecodedSms` is always `0` — there is no RP layer
/// on this path to have supplied a real one, and this route never sends a
/// delivery report (`AT+CMGD` deleting the message from storage is its
/// acknowledgement instead).
pub fn decode_sms_deliver_tpdu(tpdu: &[u8]) -> Result<DecodedSms, String> {
    let parsed = SmsDeliverTpdu::parse(tpdu)?;
    Ok(DecodedSms {
        sender: parsed.originating_address,
        text: parsed.text,
        part: parsed.part,
        rp_mr: 0,
        is_type_zero: parsed.is_type_zero,
    })
}

/// The RP-ACK that acknowledges a delivered short message at the RP layer
/// (TS 24.011 §7.3.4) — the body of the delivery report the receiver owes the
/// network, which TS 24.341 §5.3.2.4 carries in a **new `MESSAGE` request**,
/// not in the `200 OK` answering the inbound one. The SIP response says only
/// that the request reached us; this says the *message* was taken.
///
/// The shape follows TS 24.341 Annex B.6 (table B.6-7): an RP-ACK whose
/// RP-User-Data information element carries a TPDU of type SMS-DELIVER-REPORT.
pub fn build_rp_ack(rp_mr: u8) -> Vec<u8> {
    vec![
        // RP-Message-Type-Indicator 010 = RP-ACK, MS to network
        // (TS 24.011 table 8.4), then the echoed RP-MR.
        0x02, rp_mr,
        // RP-User-Data (IEI 0x41, TS 24.011 §8.2.5.3), 2 octets long,
        // holding the shortest well-formed SMS-DELIVER-REPORT there is:
        // first octet 0x00 is TP-MTI=00 (DELIVER-REPORT) with TP-UDHI clear,
        // and TP-PI=0x00 declares that none of the optional TP-PID/TP-DCS/
        // TP-UDL fields follow.
        0x41, 0x02, 0x00, 0x00,
    ]
}

/// The RP-ERROR that reports a TPDU this bridge received but could not
/// decode (TS 24.011 §7.3.4) — the delivery-report counterpart to
/// [`build_rp_ack`] for a genuine decode failure (specs/045 SMS-03), sent
/// the same way (a new `MESSAGE` request, TS 24.341 §5.3.2.4) instead of
/// silently relaying the undecoded bytes as if they were text.
///
/// `cause` is the TS 24.011 Annex E RP-Cause value when a specific one
/// applies; `111` ("unspecified error cause") is used otherwise — this
/// bridge's decoder reports *that* a TPDU didn't parse, not a granular
/// reason code for every possible way it could fail.
pub fn build_rp_error(rp_mr: u8, cause: Option<u8>) -> Vec<u8> {
    vec![
        // RP-Message-Type-Indicator 100 = RP-ERROR, MS to network
        // (TS 24.011 table 8.4), then the echoed RP-MR.
        0x04,
        rp_mr,
        // RP-Cause (TS 24.011 §8.2.5.4): a length-value element (no IEI,
        // unlike RP-User-Data) — length 1, then the cause octet.
        0x01,
        cause.unwrap_or(111),
    ]
}

/// The RP-DATA (network→MS) envelope: message reference, the SC address
/// (unused — we want the *originator*, from the TPDU inside, not the SC that
/// relayed it), and a length-prefixed RP-User-Data field holding the TPDU.
struct RpData<'a> {
    mr: u8,
    user_data: &'a [u8],
}

/// One parsed RP envelope, tagged by its actual TS 24.011 table 8.4 message
/// type — see [`decode_vnd_3gpp_sms`] for why this has to be the first thing
/// read, before any of RP-DATA's address/length fields are assumed to be
/// there at all.
enum RpMessage<'a> {
    Data(RpData<'a>),
    Ack { mr: u8 },
    Error { mr: u8, cause: Option<u8> },
}

impl<'a> RpMessage<'a> {
    fn parse(buf: &'a [u8]) -> Result<Self, String> {
        // Octet 0 bits 2-0 are the MTI (table 8.4); the upper bits are spare
        // and not always sent as zero in practice, so they are masked off
        // rather than compared against the whole octet.
        let mti = buf.first().ok_or("empty RP envelope")? & 0x07;
        let mr = *buf
            .get(1)
            .ok_or("RP envelope truncated before RP-Message-Reference")?;
        match mti {
            // RP-DATA, network→MS (TS 24.011 §7.3.1): message reference, then
            // originator/destination addresses, then a length-prefixed
            // RP-User-Data field holding the TPDU.
            1 => {
                let mut pos = 2usize;
                pos = skip_length_prefixed(buf, pos, "RP-Originator-Address")?;
                pos = skip_length_prefixed(buf, pos, "RP-Destination-Address")?;
                let ud_len = *buf
                    .get(pos)
                    .ok_or("RP-DATA truncated before RP-User-Data length")?
                    as usize;
                pos += 1;
                let user_data = buf
                    .get(pos..pos + ud_len)
                    .ok_or("RP-DATA's RP-User-Data length exceeds the buffer")?;
                Ok(RpMessage::Data(RpData { mr, user_data }))
            }
            // RP-ACK, network→MS (TS 24.011 §7.3.3): no address fields at
            // all — just the message reference, and an optional RP-User-Data
            // this bridge has no use for (it never submitted the RP-DATA
            // being acknowledged).
            3 => Ok(RpMessage::Ack { mr }),
            // RP-ERROR, network→MS (TS 24.011 §7.3.4): message reference,
            // then a length-prefixed RP-Cause element — octet 2 is the
            // length, octet 3 the cause value (TS 24.011 §8.2.5.4);
            // diagnostic bytes beyond it are not needed just to log the
            // cause.
            5 => {
                let cause = match buf.get(2) {
                    Some(&len) if len >= 1 => buf.get(3).copied(),
                    _ => None,
                };
                Ok(RpMessage::Error { mr, cause })
            }
            other => Err(format!(
                "RP message type indicator {other} is not a network-to-MS \
                 value this bridge expects to receive"
            )),
        }
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
    is_type_zero: bool,
}

impl SmsDeliverTpdu {
    fn parse(buf: &[u8]) -> Result<Self, String> {
        let first_octet = *buf.first().ok_or("empty TPDU")?;
        // specs/045 SMS-02: only SMS-DELIVER (TP-MTI 00) has the
        // TP-OA/TP-PID/TP-DCS/TP-SCTS/TP-UDL layout the rest of this
        // function assumes — walking an SMS-SUBMIT-REPORT or
        // SMS-STATUS-REPORT the same way desyncs every field after the
        // first octet, the same class of bug already fixed one layer up at
        // the RP envelope (SMS-01).
        let mti = TpduMessageType::from_first_octet(first_octet);
        if mti != TpduMessageType::Deliver {
            return Err(format!("TPDU is {mti:?}, not SMS-DELIVER"));
        }
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

        // TP-PID (1 octet). Mostly message-handling hints we don't act on
        // (a status report, a replace-message request), but one value changes
        // what the message *is* rather than how it renders — see
        // `is_type_zero_pid`.
        let pid = *buf.get(pos).ok_or("TPDU truncated before TP-PID")?;
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
            is_type_zero: is_type_zero_pid(pid),
        })
    }
}

/// TS 23.040 §9.2.3.9: a TP-PID with bits 7-6 = `01` and bits 5-0 = `000000`
/// is **Short Message Type 0** — a silent message the network uses to probe
/// whether a subscriber is reachable. "The MS shall acknowledge receipt ...
/// but shall discard its contents": it must be answered, and it must never be
/// stored or shown to anyone.
///
/// Matched on the exact bit layout rather than `== 0x40` because the
/// neighbouring values in that group are *not* the same thing: `0x41`-`0x47`
/// are Replace Short Message Type 1-7, ordinary messages that do get stored
/// (they merely supersede an earlier one from the same sender).
fn is_type_zero_pid(pid: u8) -> bool {
    (pid >> 6) == 0b01 && (pid & 0x3F) == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Alphabet {
    Gsm7,
    Octet,
    Ucs2,
}

impl Alphabet {
    /// TS 23.038 §4: the coding-scheme groups real MT traffic uses.
    /// Compressed user data and the two Discard/Store-GSM7 message-waiting
    /// groups (`0xC0`-`0xDF`) fall back to GSM7 — the fallback happens to be
    /// correct for those two, unlike the Store-UCS2 group below, which was
    /// wrong until specs/045 SMS-04.
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
        } else if dcs & 0xF0 == 0xE0 {
            // specs/045 SMS-04: Message Waiting Indication group, Store
            // Message, UCS2 (TS 23.038 §4 table) — the whole `0xE0`-`0xEF`
            // nibble means UCS2 unconditionally, the same way `0xD0`-`0xDF`
            // (its GSM7 sibling, already correct via the fallback below)
            // means GSM7 unconditionally. Unlike that sibling, this one was
            // concretely wrong before this fix — decoded as GSM7 regardless,
            // garbling real UCS2 text.
            Alphabet::Ucs2
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
            let (unpacked, bad_escape) = decode_gsm7_reporting_escapes(text_septets);

            // Some senders set TP-DCS to GSM7 (7-bit packed) while putting
            // raw, unpacked 8-bit text in TP-UD — one byte per character,
            // TP-UDL set to that byte count as if it were a septet count.
            // Unpacking that as real septets turns "welcome" into "wJ1 -Ä"
            // (measured on Jio's own message-centre traffic, 2026-08-20).
            //
            // Length alone cannot tell the two apart: for <= 7 characters a
            // packed stream occupies the same byte count as an unpacked one.
            // So the test is *corroborated*, never a lone guess — the packed
            // reading must be self-evidently broken (it decoded an escape
            // this alphabet does not define, which a well-formed GSM7 stream
            // never produces) AND the raw bytes must independently read as
            // printable ASCII.
            //
            // Requiring the packed reading to be provably invalid is what
            // keeps a legitimate short message safe: "ab" packs to
            // `0x61 0x31`, which *is* all-printable ("a1"), so a
            // printable-only test would silently corrupt it — but its packed
            // reading decodes cleanly, so it never reaches this branch.
            if udh_octets == 0 && bad_escape {
                if let Some(raw) = ud.get(..udl) {
                    if raw.iter().all(|&b| (0x20..=0x7E).contains(&b)) {
                        return Ok((part, String::from_utf8_lossy(raw).into_owned()));
                    }
                }
            }
            unpacked
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
    decode_gsm7_reporting_escapes(septets).0
}

/// [`decode_gsm7`] plus whether it met an escape the alphabet does not define.
///
/// A well-formed GSM7 stream only ever escapes into the handful of extension
/// entries below, so an undefined one is evidence the bytes were never really
/// septet-packed — which is exactly what [`decode_user_data`] needs to tell a
/// genuine 7-bit message from a gateway's unpacked-ASCII one, without
/// resorting to guessing from the text's content.
fn decode_gsm7_reporting_escapes(septets: &[u8]) -> (String, bool) {
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
    let mut bad_escape = false;
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
                _ => {
                    bad_escape = true;
                    ' '
                }
            });
        } else {
            out.push(DEFAULT_ALPHABET[(sep & 0x7F) as usize]);
        }
    }
    (out, bad_escape)
}

/// TS 23.038 §6.2.2: UCS-2, i.e. big-endian UTF-16 code units — but real
/// handsets routinely send emoji outside the Basic Multilingual Plane as
/// UTF-16 surrogate pairs over "UCS-2" SMS anyway, despite the strict spec
/// predating surrogates (confirmed live 2026-08-26: every such emoji in a
/// real inbound SMS came through as `U+FFFD` before this combined them). A
/// high surrogate followed by a low surrogate is combined into the code point
/// it encodes (RFC 2781 §2.2); anything else — an unpaired or lone surrogate
/// — becomes `U+FFFD` rather than corrupting the rest of the string.
fn decode_ucs2(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks(2)
        .map(|pair| match pair {
            [hi, lo] => u16::from_be_bytes([*hi, *lo]),
            [hi] => u16::from(*hi) << 8,
            _ => unreachable!(),
        })
        .collect();

    let mut out = String::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let unit = units[i];
        if (0xD800..=0xDBFF).contains(&unit) {
            if let Some(&low) = units.get(i + 1) {
                if (0xDC00..=0xDFFF).contains(&low) {
                    let code =
                        0x10000 + (u32::from(unit) - 0xD800) * 0x400 + (u32::from(low) - 0xDC00);
                    out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                    i += 2;
                    continue;
                }
            }
            out.push('\u{FFFD}');
        } else {
            out.push(char::from_u32(u32::from(unit)).unwrap_or('\u{FFFD}'));
        }
        i += 1;
    }
    out
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

        let mut rp = vec![0x01, 0x17]; // RP-MTI (net->MS), RP-MR
        rp.push(0); // RP-OA length (SC address) — irrelevant to this decoder
        rp.push(0); // RP-DA length — absent for MT
        rp.push(tpdu.len() as u8); // RP-UD length
        rp.extend_from_slice(&tpdu);

        let DecodedRp::Message(decoded) = decode_vnd_3gpp_sms(&rp).unwrap() else {
            panic!("RP-DATA must decode to DecodedRp::Message");
        };
        assert_eq!(decoded.sender, "+919876543210");
        assert_eq!(decoded.text, "Hello");
        assert_eq!(decoded.part, None);
        // The reference the delivery report has to quote back, taken from the
        // envelope rather than defaulted — a report carrying the wrong one
        // acknowledges nothing.
        assert_eq!(decoded.rp_mr, 0x17);
    }

    /// CS-02 (specs/041 conformance review): the IMS `MESSAGE` route
    /// (`decode_vnd_3gpp_sms`, RP-DATA-wrapped) and the modem-storage route
    /// (`decode_sms_deliver_tpdu`, bare TPDU — `sms::reader`) must decode the
    /// *same* TPDU to the *same* sender and text. `volte::sms::Dedupe`'s key
    /// is exactly `sender + body`; if the two routes ever disagreed on either
    /// (as text mode used to, for anything outside plain GSM7), the same
    /// real-world message delivered over both bearers would fail to collapse
    /// and the operator would see it twice.
    #[test]
    fn both_delivery_routes_decode_the_same_tpdu_identically() {
        let sender_digits = "919876543210";
        let oa_bytes = bcd_encode(sender_digits);
        let text_septets = gsm7_encode("Hi");

        let mut tpdu = vec![0x00];
        tpdu.push(sender_digits.len() as u8);
        tpdu.push(0x91);
        tpdu.extend_from_slice(&oa_bytes);
        tpdu.push(0x00);
        tpdu.push(0x00);
        tpdu.extend_from_slice(&[0u8; 7]);
        tpdu.push(text_septets.len() as u8);
        tpdu.extend_from_slice(&pack_septets(&text_septets));

        // The IMS route: this same TPDU, wrapped in an RP-DATA envelope.
        let mut rp = vec![0x01, 0x00, 0, 0];
        rp.push(tpdu.len() as u8);
        rp.extend_from_slice(&tpdu);
        let DecodedRp::Message(over_registration) = decode_vnd_3gpp_sms(&rp).unwrap() else {
            panic!("RP-DATA must decode to DecodedRp::Message");
        };

        // The modem-storage route: the bare TPDU, no RP layer at all.
        let through_modem = decode_sms_deliver_tpdu(&tpdu).unwrap();

        assert_eq!(over_registration.sender, through_modem.sender);
        assert_eq!(over_registration.text, through_modem.text);
    }

    /// TS 23.040 §9.2.3.9 vs the Replace Short Message group next to it. Only
    /// `0x40` is the silent probe; `0x41`-`0x47` are ordinary messages that
    /// must still be stored and shown, so an `== 0x40 & 0xF8`-style test would
    /// silently swallow seven kinds of real text.
    #[test]
    fn type_zero_is_only_pid_0x40_not_the_replace_group() {
        assert!(is_type_zero_pid(0x40));
        for pid in 0x41..=0x47 {
            assert!(!is_type_zero_pid(pid), "0x{pid:02x} is Replace, not Type 0");
        }
        assert!(!is_type_zero_pid(0x00));
        assert!(!is_type_zero_pid(0xC0));
    }

    /// A gateway that sets TP-DCS to packed GSM7 but puts raw unpacked ASCII
    /// in TP-UD. The packed reading of "welcome" decodes an escape the
    /// alphabet does not define, which is the structural signal that lets the
    /// raw bytes be trusted instead.
    #[test]
    fn recovers_unpacked_ascii_mislabelled_as_packed_gsm7() {
        let raw = b"welcome";
        let (part, text) = decode_user_data(raw, raw.len(), Alphabet::Gsm7, false).unwrap();
        assert_eq!(part, None);
        assert_eq!(text, "welcome");
    }

    /// The guard that keeps that recovery safe. "ab" packs to `0x61 0x31`,
    /// whose bytes *are* all printable ASCII ("a1") — so a printable-only
    /// test would corrupt it. Its packed reading decodes cleanly (no
    /// undefined escape), so it must never reach the recovery branch.
    #[test]
    fn a_genuine_packed_message_is_never_mistaken_for_unpacked_ascii() {
        let packed = pack_septets(&gsm7_encode("ab"));
        assert!(
            packed.iter().all(|&b| (0x20..=0x7E).contains(&b)),
            "this test is only meaningful while the packed bytes look printable"
        );
        let (_, text) = decode_user_data(&packed, 2, Alphabet::Gsm7, false).unwrap();
        assert_eq!(text, "ab");
    }

    /// TS 24.341 Annex B.6 (table B.6-7): an RP-ACK whose RP-User-Data IE
    /// carries an SMS-DELIVER-REPORT TPDU.
    #[test]
    fn builds_an_rp_ack_echoing_the_message_reference() {
        assert_eq!(build_rp_ack(0x17), vec![0x02, 0x17, 0x41, 0x02, 0x00, 0x00]);
    }

    /// RP-MR is a full octet, so a report is routinely not valid UTF-8 — the
    /// reason the whole send path down to the socket is bytes, never a
    /// `String`. Pins the value that would be lost to a lossy conversion.
    #[test]
    fn an_rp_ack_is_not_necessarily_valid_utf8() {
        let ack = build_rp_ack(0xB4);
        assert_eq!(ack[1], 0xB4);
        assert!(String::from_utf8(ack).is_err());
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

        let DecodedRp::Message(decoded) = decode_vnd_3gpp_sms(&rp).unwrap() else {
            panic!("RP-DATA must decode to DecodedRp::Message");
        };
        assert_eq!(decoded.text, "Hi");
    }

    /// specs/045 SMS-04: DCS `0xE8` is Message Waiting Indication (Store),
    /// bit 2 set — UCS2, per TS 23.038 §4. Before this fix it fell through
    /// to the default-GSM7 branch and garbled the text the same way plain
    /// UCS2 (DCS `0x08`) would have before UCS2 support existed at all.
    #[test]
    fn decodes_message_waiting_indication_ucs2_text() {
        let text_bytes: Vec<u8> = "Hi".encode_utf16().flat_map(u16::to_be_bytes).collect();

        let mut tpdu = vec![0x00];
        tpdu.push(0); // TP-OA length 0
        tpdu.push(0x81);
        tpdu.push(0x00); // TP-PID
        tpdu.push(0xE8); // TP-DCS: message waiting indication, Store, UCS2
        tpdu.extend_from_slice(&[0u8; 7]);
        tpdu.push(text_bytes.len() as u8);
        tpdu.extend_from_slice(&text_bytes);

        let mut rp = vec![0x01, 0x00, 0, 0];
        rp.push(tpdu.len() as u8);
        rp.extend_from_slice(&tpdu);

        let DecodedRp::Message(decoded) = decode_vnd_3gpp_sms(&rp).unwrap() else {
            panic!("RP-DATA must decode to DecodedRp::Message");
        };
        assert_eq!(decoded.text, "Hi");
    }

    /// The sibling Discard/Store-GSM7 groups (`0xC0`/`0xD0`) were already
    /// correct (GSM7 is both the fallback and the spec answer) — pinned so
    /// the new `0xE0` branch above can't regress them.
    #[test]
    fn message_waiting_indication_gsm7_groups_are_unaffected() {
        assert_eq!(Alphabet::from_dcs(0xC0), Alphabet::Gsm7);
        assert_eq!(Alphabet::from_dcs(0xD0), Alphabet::Gsm7);
    }

    /// SMS-EMOJI-01 (specs/041 conformance review, found live 2026-08-26): a
    /// real inbound SMS carrying an emoji outside the Basic Multilingual
    /// Plane came through as `U+FFFD` because the two UTF-16 code units of
    /// its surrogate pair were decoded independently instead of combined.
    #[test]
    fn decode_ucs2_reassembles_a_surrogate_pair() {
        let bytes: Vec<u8> = "Hello 😀 world"
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();
        assert_eq!(decode_ucs2(&bytes), "Hello 😀 world");
    }

    /// A high surrogate with no valid low surrogate after it — truncated
    /// input, or a genuinely malformed encoder — must not panic or silently
    /// combine with whatever follows; it becomes `U+FFFD` like any other
    /// undecodable unit.
    #[test]
    fn decode_ucs2_replaces_an_unpaired_high_surrogate() {
        // 0xD83D is the high surrogate half of an emoji pair; 0x0041 ('A')
        // is not a valid low surrogate, so it must decode on its own.
        let bytes = [0xD8, 0x3D, 0x00, 0x41];
        assert_eq!(decode_ucs2(&bytes), "\u{FFFD}A");
    }

    /// A lone high surrogate at the very end of the buffer (no second unit to
    /// even check) must not panic.
    #[test]
    fn decode_ucs2_replaces_a_high_surrogate_at_end_of_buffer() {
        let bytes = [0xD8, 0x3D];
        assert_eq!(decode_ucs2(&bytes), "\u{FFFD}");
    }

    /// A low surrogate encountered on its own (not preceded by a matching
    /// high surrogate) is not a valid scalar value either.
    #[test]
    fn decode_ucs2_replaces_a_lone_low_surrogate() {
        let bytes = [0xDC, 0x00];
        assert_eq!(decode_ucs2(&bytes), "\u{FFFD}");
    }

    /// End-to-end: the exact shape of the real message that surfaced
    /// SMS-EMOJI-01 — plain text either side of an astral-plane emoji,
    /// decoded through the full RP-DATA/TPDU path, not just `decode_ucs2`
    /// directly.
    #[test]
    fn decodes_a_ucs2_message_with_an_emoji() {
        let text_bytes: Vec<u8> = "Hello world! 😀 Welcome"
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();

        let mut tpdu = vec![0x00];
        tpdu.push(0);
        tpdu.push(0x81);
        tpdu.push(0x00);
        tpdu.push(0x08); // TP-DCS: general group, UCS-2
        tpdu.extend_from_slice(&[0u8; 7]);
        tpdu.push(text_bytes.len() as u8);
        tpdu.extend_from_slice(&text_bytes);

        let mut rp = vec![0x01, 0x00, 0, 0];
        rp.push(tpdu.len() as u8);
        rp.extend_from_slice(&tpdu);

        let DecodedRp::Message(decoded) = decode_vnd_3gpp_sms(&rp).unwrap() else {
            panic!("RP-DATA must decode to DecodedRp::Message");
        };
        assert_eq!(decoded.text, "Hello world! 😀 Welcome");
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

        let DecodedRp::Message(decoded) = decode_vnd_3gpp_sms(&rp).unwrap() else {
            panic!("RP-DATA must decode to DecodedRp::Message");
        };
        assert_eq!(decoded.part, Some((2, 2)));
        assert_eq!(decoded.text, "part2");
    }

    #[test]
    fn truncated_input_is_an_error_not_a_panic() {
        assert!(decode_vnd_3gpp_sms(&[]).is_err());
        assert!(decode_vnd_3gpp_sms(&[0x01, 0x00, 0, 0, 200]).is_err());
    }

    /// TS 24.011 §7.3.3: an RP-ACK carries no address fields, so walking it
    /// as RP-DATA would misread its RP-User-Data-length byte (absent here)
    /// as an address length. Reading the MTI first must route it to `Ack`
    /// instead, with nothing to decode as a message.
    #[test]
    fn an_rp_ack_is_recognized_and_not_walked_as_rp_data() {
        // MTI=3 (RP-ACK, network->MS), MR=0x42, no RP-User-Data.
        let rp = vec![0x03, 0x42];
        assert_eq!(
            decode_vnd_3gpp_sms(&rp).unwrap(),
            DecodedRp::Ack { rp_mr: 0x42 }
        );
    }

    /// TS 24.011 §7.3.4: an RP-ERROR carries a length-prefixed RP-Cause
    /// element in place of RP-DATA's addresses. This is the exact envelope
    /// shape that used to be misread as RP-DATA and could produce a
    /// plausible-looking sender/body for a "message" that was actually a
    /// submission failure report.
    #[test]
    fn an_rp_error_is_recognized_with_its_cause_and_not_walked_as_rp_data() {
        // MTI=5 (RP-ERROR, network->MS), MR=0x07, RP-Cause length=1, cause=95
        // (semantically incorrect message, TS 24.011 §8.2.5.4).
        let rp = vec![0x05, 0x07, 0x01, 95];
        assert_eq!(
            decode_vnd_3gpp_sms(&rp).unwrap(),
            DecodedRp::Error {
                rp_mr: 0x07,
                cause: Some(95)
            }
        );
    }

    /// An MS-to-network MTI (RP-DATA/RP-ACK/RP-ERROR submitted *by* an MS, or
    /// RP-SMMA) has no business arriving in a MESSAGE addressed to us — it
    /// names a direction, not just a shape, and guessing which shape it might
    /// still fit is exactly the mistake this type-check exists to avoid.
    #[test]
    fn a_ms_to_network_mti_is_rejected_rather_than_guessed_at() {
        for mti in [0u8, 2, 4, 6] {
            assert!(
                decode_vnd_3gpp_sms(&[mti, 0x00]).is_err(),
                "MTI {mti} is an MS-to-network value and must not decode"
            );
        }
    }

    /// One well-formed RP-DATA envelope (MTI=1) wrapping `tpdu` — the shape
    /// `decode_vnd_3gpp_sms`'s `RpMessage::Data` arm expects, with no
    /// originator/destination address (both length 0).
    fn rp_data(mr: u8, tpdu: &[u8]) -> Vec<u8> {
        let mut rp = vec![0x01, mr, 0x00, 0x00, tpdu.len() as u8];
        rp.extend_from_slice(tpdu);
        rp
    }

    /// specs/045 SMS-02: an SMS-STATUS-REPORT (TP-MTI=10) inside an
    /// otherwise well-formed RP-DATA must be recognized as such, not walked
    /// with the SMS-DELIVER field layout — the RP-DATA itself was received,
    /// so it's still owed an RP-ACK (`UnsupportedTpdu`, not `Undecodable`).
    #[test]
    fn a_status_report_tpdu_is_recognized_not_misread_as_deliver() {
        let rp = rp_data(0x11, &[0b10]); // TP-MTI=10 (StatusReport)
        assert_eq!(
            decode_vnd_3gpp_sms(&rp).unwrap(),
            DecodedRp::UnsupportedTpdu {
                rp_mr: 0x11,
                kind: TpduMessageType::StatusReport,
            }
        );
    }

    /// A submit-report TPDU gets the same treatment.
    #[test]
    fn a_submit_report_tpdu_is_recognized_not_misread_as_deliver() {
        let rp = rp_data(0x12, &[0b01]); // TP-MTI=01 (SubmitReport)
        assert_eq!(
            decode_vnd_3gpp_sms(&rp).unwrap(),
            DecodedRp::UnsupportedTpdu {
                rp_mr: 0x12,
                kind: TpduMessageType::SubmitReport,
            }
        );
    }

    /// specs/045 SMS-03: a TPDU that claims to be SMS-DELIVER (TP-MTI=00)
    /// but is truncated before its own fields end must be recognized as a
    /// genuine failure (`Undecodable`, owed an RP-ERROR) — distinct from
    /// `UnsupportedTpdu`, which is never a decode failure.
    #[test]
    fn a_truncated_deliver_tpdu_is_undecodable_not_unsupported() {
        let rp = rp_data(0x13, &[0x00]); // TP-MTI=00, nothing after it
        assert_eq!(
            decode_vnd_3gpp_sms(&rp).unwrap(),
            DecodedRp::Undecodable { rp_mr: 0x13 }
        );
    }

    #[test]
    fn build_rp_error_states_the_given_cause() {
        let rp = build_rp_error(0x20, Some(95));
        assert_eq!(rp, vec![0x04, 0x20, 0x01, 95]);
    }

    #[test]
    fn build_rp_error_falls_back_to_unspecified_when_no_cause_is_known() {
        let rp = build_rp_error(0x21, None);
        assert_eq!(rp, vec![0x04, 0x21, 0x01, 111]);
    }
}
