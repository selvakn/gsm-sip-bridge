//! Raw APDU access to the USIM via the modem's `AT+CSIM` passthrough
//! (3GPP TS 27.007 §8.17), used to run 3GPP AKA (TS 33.102) challenges
//! against the real SIM for both EAP-AKA (VoWiFi/ePDG) and IMS-AKA (SIP
//! REGISTER) — the AUTHENTICATE command is identical for both.
//!
//! `P2=0x00` on SELECT is rejected ("wrong P1/P2", SW 6B00) by at least one
//! card/modem combination in the field (Quectel EC200U + Vodafone India
//! USIM); `P2=0x0C` (no FCP/FCI returned) works broadly and is used
//! throughout. The USIM ADF's AID is read from EF_DIR rather than hardcoded,
//! since it is card-specific.

use super::at_commander::{AtCommander, AtResponse};
use crate::error::{BridgeError, BridgeResult};

const SW_SUCCESS: &str = "9000";

/// A raw APDU carrier: send a hex-encoded command APDU, get back the
/// hex-encoded response (data + SW1SW2). Two implementations exist — a
/// modem's `AT+CSIM` passthrough (`AtCommander`, this file's original and
/// only transport until specs/023-omnikey-pcsc-vowifi) and a real PC/SC
/// reader (`modules::pcsc_card::PcscTransport`) — so the SELECT/READ
/// RECORD/AUTHENTICATE logic below runs unchanged over either one.
pub trait ApduTransport {
    fn transmit_apdu(&mut self, apdu_hex: &str) -> BridgeResult<String>;
}

impl ApduTransport for AtCommander {
    fn transmit_apdu(&mut self, apdu_hex: &str) -> BridgeResult<String> {
        csim(self, apdu_hex)
    }
}

/// Outcome of a 3GPP AKA AUTHENTICATE command run against the USIM.
#[derive(Debug, Clone)]
pub enum AkaResult {
    /// Network authenticated successfully; RES/CK/IK are the raw octets.
    Success {
        res: Vec<u8>,
        ck: Vec<u8>,
        ik: Vec<u8>,
    },
    /// SQN out of sync; AUTS must be sent back to the network to resync.
    SyncFailure { auts: Vec<u8> },
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

pub(crate) fn hex_decode(s: &str) -> BridgeResult<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(BridgeError::Ims(format!("odd-length hex string: {s}")));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|e| BridgeError::Ims(format!("invalid hex byte in {s}: {e}")))
        })
        .collect()
}

/// Send one `AT+CSIM` command carrying a raw APDU (as an uppercase hex
/// string, no separators) and return the raw hex response (data + SW1SW2).
pub(crate) fn csim(at: &mut AtCommander, apdu_hex: &str) -> BridgeResult<String> {
    let cmd = format!(r#"AT+CSIM={},"{}""#, apdu_hex.len(), apdu_hex);
    match at.send_command(&cmd)? {
        AtResponse::Ok(lines) => lines
            .iter()
            .find_map(|l| l.strip_prefix("+CSIM: "))
            .and_then(|rest| rest.split_once(','))
            .map(|(_, data)| data.trim().trim_matches('"').to_string())
            .ok_or_else(|| BridgeError::Ims(format!("unexpected +CSIM reply: {lines:?}"))),
        AtResponse::Error(e) | AtResponse::CmeError(_, e) => {
            Err(BridgeError::Ims(format!("AT+CSIM failed: {e}")))
        }
    }
}

/// SELECT by file ID (MF/DF/EF), P2=0x0C (no response data requested).
fn select_fid(at: &mut dyn ApduTransport, fid: u16) -> BridgeResult<()> {
    let apdu = format!("00A4000C02{fid:04X}");
    let resp = at.transmit_apdu(&apdu)?;
    if !resp.ends_with(SW_SUCCESS) {
        return Err(BridgeError::Ims(format!(
            "SELECT {fid:04X} failed: SW={resp}"
        )));
    }
    Ok(())
}

/// SELECT by AID (application), P2=0x0C.
fn select_aid(at: &mut dyn ApduTransport, aid: &[u8]) -> BridgeResult<()> {
    let apdu = format!("00A4040C{:02X}{}", aid.len(), hex_encode(aid));
    let resp = at.transmit_apdu(&apdu)?;
    if !resp.ends_with(SW_SUCCESS) {
        return Err(BridgeError::Ims(format!(
            "SELECT AID {} failed: SW={resp}",
            hex_encode(aid)
        )));
    }
    Ok(())
}

/// READ RECORD (mode: absolute, from current EF), P2=0x04.
///
/// `Le=00` here (asking for "whatever's there") is what `AT+CSIM` over a
/// modem always resolved transparently, but a real PC/SC reader enforces
/// the exact length: it replies SW=6CXX ("wrong Le; SW2 is the actual
/// length") and returns no data, which — before this retry existed — made
/// every record silently look empty and `discover_usim_aid` exhaust all 16
/// records and fail (caught live against a real OmniKey AG 3x21,
/// specs/023-omnikey-pcsc-vowifi's IMS-AKA follow-up work).
fn read_record(at: &mut dyn ApduTransport, record: u8) -> BridgeResult<Option<String>> {
    let apdu = format!("00B2{record:02X}0400");
    let mut resp = at.transmit_apdu(&apdu)?;
    if resp.len() >= 4 {
        let sw = &resp[resp.len() - 4..];
        if sw.starts_with("6C") {
            let le = &sw[2..4];
            let retry = format!("00B2{record:02X}04{le}");
            resp = at.transmit_apdu(&retry)?;
        }
    }
    if resp.ends_with(SW_SUCCESS) && resp.len() > 4 {
        Ok(Some(resp[..resp.len() - 4].to_string()))
    } else {
        Ok(None)
    }
}

/// READ BINARY (transparent EF, offset 0). Same SW=6CXX wrong-length retry
/// as `read_record` — real PC/SC readers enforce `Le` exactly.
fn read_binary(at: &mut dyn ApduTransport, le: u8) -> BridgeResult<Vec<u8>> {
    let apdu = format!("00B00000{le:02X}");
    let mut resp = at.transmit_apdu(&apdu)?;
    if resp.len() >= 4 {
        let sw = &resp[resp.len() - 4..];
        if sw.starts_with("6C") {
            let actual_le = &sw[2..4];
            let retry = format!("00B00000{actual_le}");
            resp = at.transmit_apdu(&retry)?;
        }
    }
    if !resp.ends_with(SW_SUCCESS) {
        return Err(BridgeError::Ims(format!("READ BINARY failed: SW={resp}")));
    }
    hex_decode(&resp[..resp.len() - 4])
}

/// Reverses the nibble order within each byte of a hex string (`"AB"` ->
/// `"BA"`) — the BCD "swapped" representation SIM file contents (IMSI,
/// MSISDN, ...) use throughout TS 51.011/31.102.
fn swap_nibbles_hex(hex: &str) -> String {
    hex.as_bytes()
        .chunks(2)
        .map(|pair| {
            if pair.len() == 2 {
                format!("{}{}", pair[1] as char, pair[0] as char)
            } else {
                (pair[0] as char).to_string()
            }
        })
        .collect()
}

/// Reads and decodes EF_IMSI (`6F07`) from the currently-selected USIM ADF
/// (`select_usim` must run first) — TS 31.102 §4.2.2. Encoding: one length
/// byte (of the following BCD data), then that many bytes of nibble-swapped
/// BCD with a leading parity/oddness nibble that is **not** part of the
/// IMSI (`docs/omnikey-pcsc-vowifi.md`'s hand-decode gotcha — this mirrors
/// pySim's `dec_imsi`, dropping `swapped[..1]`, not `swapped[..0]`).
pub fn read_imsi(at: &mut dyn ApduTransport) -> BridgeResult<String> {
    select_fid(at, 0x6F07)?;
    let raw = read_binary(at, 9)?;
    let Some(&length_byte) = raw.first() else {
        return Err(BridgeError::Ims("EF_IMSI response is empty".into()));
    };
    let digit_count = (length_byte as usize) * 2 - 1;
    let swapped = swap_nibbles_hex(&hex_encode(&raw[1..]));
    if swapped.len() <= digit_count {
        return Err(BridgeError::Ims(format!(
            "EF_IMSI too short for its own declared length: {} nibbles, need {digit_count}",
            swapped.len()
        )));
    }
    let imsi = swapped[1..=digit_count].to_string();
    if !imsi.chars().all(|c| c.is_ascii_digit()) {
        return Err(BridgeError::Ims(format!(
            "EF_IMSI decoded to a non-numeric IMSI: {imsi:?}"
        )));
    }
    Ok(imsi)
}

/// Reads the home network's MNC digit count from EF_AD (`6FAD`) on the
/// currently-selected USIM ADF (`select_usim` must run first) — TS 31.102
/// §4.2.18, byte 4's low nibble. The IMSI alone cannot say whether its MNC
/// is 2 or 3 digits, and this file is the card's own authoritative answer.
///
/// The modem path reaches the same byte through `AT+CRSM=176,28589,0,0,4`
/// (28589 = `0x6FAD`); running it over `ApduTransport` instead means a
/// card-reader line derives its PLMN from the card with no modem involved.
/// A card that leaves the nibble unprogrammed (legacy 2G SIMs may) errors
/// here rather than guessing — `plmn::derive_plmn` decides what to fall
/// back to, since only the modem path has a fallback available.
pub fn read_mnc_length(at: &mut dyn ApduTransport) -> BridgeResult<u8> {
    select_fid(at, 0x6FAD)?;
    let raw = read_binary(at, 4)?;
    let Some(&byte4) = raw.get(3) else {
        return Err(BridgeError::Ims(format!(
            "EF_AD shorter than 4 bytes (no MNC length byte): {}",
            hex_encode(&raw)
        )));
    };
    let mnc_len = byte4 & 0x0F;
    if mnc_len != 2 && mnc_len != 3 {
        return Err(BridgeError::Ims(format!(
            "EF_AD MNC length not 2 or 3 (unprogrammed?): {mnc_len}"
        )));
    }
    Ok(mnc_len)
}

const USIM_RID: &str = "A0000000871002";

/// Extract a USIM AID from one EF_DIR record's raw hex data, if present.
/// Template: `61 <len> 4F <aid_len> <AID> ...` (TS 101.220); only returns an
/// AID whose RID matches the 3GPP USIM RID `A0000000871002` — other entries
/// (e.g. ISIM, proprietary apps) are skipped.
fn extract_usim_aid_from_ef_dir_record(record_hex: &str) -> Option<Vec<u8>> {
    let rest = record_hex.strip_prefix("61")?;
    if rest.len() < 2 {
        return None;
    }
    let tlv = &rest[2..]; // skip template length byte
    let aid_rest = tlv.strip_prefix("4F")?;
    if aid_rest.len() < 2 {
        return None;
    }
    let aid_len = u8::from_str_radix(&aid_rest[..2], 16).ok()? as usize;
    let aid_hex_len = aid_len * 2;
    if aid_rest.len() < 2 + aid_hex_len {
        return None;
    }
    let aid_hex = &aid_rest[2..2 + aid_hex_len];
    if aid_hex.starts_with(USIM_RID) {
        hex_decode(aid_hex).ok()
    } else {
        None
    }
}

/// Discover the USIM application's AID by reading EF_DIR (2F00 under MF).
/// EF_DIR is a linear-fixed file of ASN.1 application templates; this walks
/// records until a USIM entry is found.
pub fn discover_usim_aid(at: &mut dyn ApduTransport) -> BridgeResult<Vec<u8>> {
    select_fid(at, 0x3F00)?;
    select_fid(at, 0x2F00)?;

    for record in 1..=16u8 {
        let Some(data) = read_record(at, record)? else {
            continue;
        };
        if let Some(aid) = extract_usim_aid_from_ef_dir_record(&data) {
            return Ok(aid);
        }
    }
    Err(BridgeError::Ims(
        "no USIM application found in EF_DIR".into(),
    ))
}

/// Select the MF then the USIM ADF (by AID), ready for AUTHENTICATE.
pub fn select_usim(at: &mut dyn ApduTransport, aid: &[u8]) -> BridgeResult<()> {
    select_fid(at, 0x3F00)?;
    select_aid(at, aid)
}

/// Run a 3GPP AKA AUTHENTICATE command (TS 31.102 §7.1.2.1) against the
/// currently-selected USIM ADF, given a 16-byte RAND and 16-byte AUTN from
/// the network challenge.
///
/// Handles both the classic two-step flow (SW=61XX "more data available" ->
/// follow-up GET RESPONSE) and modems that auto-chain GET RESPONSE and
/// return the full result directly with SW=9000 (observed on the Quectel
/// EC200U).
pub fn authenticate(
    at: &mut dyn ApduTransport,
    rand: &[u8; 16],
    autn: &[u8; 16],
) -> BridgeResult<AkaResult> {
    let apdu = format!("008800812210{}10{}", hex_encode(rand), hex_encode(autn));
    let mut resp = at.transmit_apdu(&apdu)?;

    if resp.len() >= 4 {
        let sw = &resp[resp.len() - 4..];
        if sw.starts_with("61") {
            let le = &sw[2..4];
            let follow_up = format!("00C00000{le}");
            resp = at.transmit_apdu(&follow_up)?;
        }
    }

    if !resp.ends_with(SW_SUCCESS) {
        return Err(BridgeError::Ims(format!("AUTHENTICATE failed: SW={resp}")));
    }
    let data = hex_decode(&resp[..resp.len() - 4])?;
    parse_authenticate_response(&data)
}

/// Parse the AUTHENTICATE response data object (TS 31.102 §7.1.2.1):
/// tag 0xDB (success) -> RES_len RES CK_len CK IK_len IK [Kc_len Kc]
/// tag 0xDC (sync failure) -> AUTS_len AUTS (AUTS is fixed at 14 bytes)
fn parse_authenticate_response(data: &[u8]) -> BridgeResult<AkaResult> {
    if data.is_empty() {
        return Err(BridgeError::Ims("empty AUTHENTICATE response".into()));
    }
    match data[0] {
        0xDB => {
            let mut pos = 1;
            let take = |data: &[u8], pos: &mut usize| -> BridgeResult<Vec<u8>> {
                let len = *data
                    .get(*pos)
                    .ok_or_else(|| BridgeError::Ims("truncated AUTHENTICATE response".into()))?
                    as usize;
                *pos += 1;
                let end = *pos + len;
                let bytes = data
                    .get(*pos..end)
                    .ok_or_else(|| BridgeError::Ims("truncated AUTHENTICATE response".into()))?
                    .to_vec();
                *pos = end;
                Ok(bytes)
            };
            let res = take(data, &mut pos)?;
            let ck = take(data, &mut pos)?;
            let ik = take(data, &mut pos)?;
            Ok(AkaResult::Success { res, ck, ik })
        }
        0xDC => {
            let len = *data
                .get(1)
                .ok_or_else(|| BridgeError::Ims("truncated AUTS response".into()))?
                as usize;
            let auts = data
                .get(2..2 + len)
                .ok_or_else(|| BridgeError::Ims("truncated AUTS response".into()))?
                .to_vec();
            Ok(AkaResult::SyncFailure { auts })
        }
        other => Err(BridgeError::Ims(format!(
            "unrecognized AUTHENTICATE response tag: {other:#04x}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let bytes = [0xDEu8, 0xAD, 0xBE, 0xEF];
        let hex = hex_encode(&bytes);
        assert_eq!(hex, "DEADBEEF");
        assert_eq!(hex_decode(&hex).unwrap(), bytes);
    }

    #[test]
    fn hex_decode_rejects_odd_length() {
        assert!(hex_decode("ABC").is_err());
    }

    #[test]
    fn parse_success_response() {
        // DB RESlen=08 RES(8) CKlen=10 CK(16) IKlen=10 IK(16) — no separate
        // overall-length byte; verified against the real device response
        // bytes captured during Phase 1 (docker/epdg) live testing.
        let res = [0x11u8; 8];
        let ck = [0x22u8; 16];
        let ik = [0x33u8; 16];
        let mut data = vec![0xDB];
        data.push(res.len() as u8);
        data.extend_from_slice(&res);
        data.push(ck.len() as u8);
        data.extend_from_slice(&ck);
        data.push(ik.len() as u8);
        data.extend_from_slice(&ik);

        match parse_authenticate_response(&data).unwrap() {
            AkaResult::Success {
                res: r,
                ck: c,
                ik: i,
            } => {
                assert_eq!(r, res);
                assert_eq!(c, ck);
                assert_eq!(i, ik);
            }
            AkaResult::SyncFailure { .. } => panic!("expected Success"),
        }
    }

    #[test]
    fn parse_sync_failure_response() {
        let auts = [0x44u8; 14];
        let mut data = vec![0xDC, auts.len() as u8];
        data.extend_from_slice(&auts);

        match parse_authenticate_response(&data).unwrap() {
            AkaResult::SyncFailure { auts: a } => assert_eq!(a, auts),
            AkaResult::Success { .. } => panic!("expected SyncFailure"),
        }
    }

    #[test]
    fn parse_unknown_tag_errors() {
        assert!(parse_authenticate_response(&[0x00, 0x00]).is_err());
    }

    #[test]
    fn parse_empty_errors() {
        assert!(parse_authenticate_response(&[]).is_err());
    }

    // Exercises the EF_DIR record parser directly rather than through
    // `discover_usim_aid`'s multi-command AT flow: `AtCommander::read_response`
    // builds a fresh `BufReader` per `send_command` call, which over-reads
    // and silently drops any buffered-but-unconsumed bytes from a
    // single-shot mock stream across more than one call — a pre-existing
    // quirk unrelated to this feature, not something to work around here.
    #[test]
    fn ef_dir_record_matches_usim_aid_from_real_card() {
        // Fixture matches the real EC200U/Vi India card response captured
        // during Phase 1 (docker/epdg) live testing.
        let record = "61184F10A0000000871002FFF605FF89000001FF50045553494D9000";
        let aid = extract_usim_aid_from_ef_dir_record(record).unwrap();
        assert_eq!(hex_encode(&aid), "A0000000871002FFF605FF89000001FF");
    }

    #[test]
    fn ef_dir_record_skips_non_usim_entry() {
        // Same template shape but a RID that doesn't match the 3GPP USIM RID.
        let record = "61184F10FFFFFFFFFFFFFFFFF605FF89000001FF50045553494D9000";
        assert!(extract_usim_aid_from_ef_dir_record(record).is_none());
    }

    #[test]
    fn ef_dir_record_rejects_malformed_entry() {
        assert!(extract_usim_aid_from_ef_dir_record("6981").is_none());
        assert!(extract_usim_aid_from_ef_dir_record("").is_none());
    }

    /// A scripted `ApduTransport` returning canned responses in order,
    /// ignoring the request bytes — unlike an `AtCommander`-backed mock
    /// (see the doc comment above `ef_dir_record_matches_usim_aid_from_real_card`),
    /// this has no per-call `BufReader` over-read quirk, so it can actually
    /// exercise the multi-command SELECT/READ RECORD/AUTHENTICATE flow
    /// end-to-end (specs/023-omnikey-pcsc-vowifi's whole point in
    /// generalizing these functions off `AtCommander`).
    struct QueuedTransport(std::collections::VecDeque<String>);

    impl ApduTransport for QueuedTransport {
        fn transmit_apdu(&mut self, _apdu_hex: &str) -> BridgeResult<String> {
            self.0
                .pop_front()
                .ok_or_else(|| BridgeError::Ims("no more scripted responses".into()))
        }
    }

    #[test]
    fn discover_and_select_usim_work_over_a_generic_apdu_transport() {
        let ef_dir_record = "61184F10A0000000871002FFF605FF89000001FF50045553494D9000";
        let mut t = QueuedTransport(std::collections::VecDeque::from([
            "9000".to_string(),        // SELECT MF (discover_usim_aid)
            "9000".to_string(),        // SELECT EF_DIR
            ef_dir_record.to_string(), // READ RECORD 1
            "9000".to_string(),        // SELECT MF (select_usim)
            "9000".to_string(),        // SELECT AID
        ]));
        let aid = discover_usim_aid(&mut t).unwrap();
        assert_eq!(hex_encode(&aid), "A0000000871002FFF605FF89000001FF");
        select_usim(&mut t, &aid).unwrap();
    }

    #[test]
    fn discover_usim_aid_retries_read_record_on_wrong_le() {
        // Caught live on 2026-07-28 against a real OmniKey AG 3x21: unlike
        // AT+CSIM (which resolved Le=00 transparently), a real PC/SC reader
        // enforces the exact Le and answers READ RECORD's Le=00 with
        // SW=6C1A ("wrong length, actual data is 26 bytes") and no data —
        // silently starving `discover_usim_aid`'s EF_DIR walk on every
        // record until this retry existed. Fixture is the exact byte
        // sequence captured from the real card.
        let ef_dir_record = "61184F10A0000000871002FFF605FF89000001FF50045553494D9000";
        let mut t = QueuedTransport(std::collections::VecDeque::from([
            "9000".to_string(),        // SELECT MF
            "9000".to_string(),        // SELECT EF_DIR
            "6C1A".to_string(),        // READ RECORD 1, Le=00 -> wrong length
            ef_dir_record.to_string(), // READ RECORD 1 retried with Le=1A
        ]));
        let aid = discover_usim_aid(&mut t).unwrap();
        assert_eq!(hex_encode(&aid), "A0000000871002FFF605FF89000001FF");
    }

    #[test]
    fn swap_nibbles_hex_reverses_each_byte_pair() {
        assert_eq!(swap_nibbles_hex("4940340838994604"), "9404438083996440");
    }

    #[test]
    fn read_imsi_decodes_a_real_ef_imsi_fixture() {
        // Reverse-derived from the real IMSI 404438083996440 read live off an
        // OmniKey AG 3x21 (docs/omnikey-pcsc-vowifi.md): length byte 0x08 (15
        // digits), then 8 bytes of nibble-swapped BCD with a leading
        // parity/oddness nibble that isn't part of the IMSI.
        let mut t = QueuedTransport(std::collections::VecDeque::from([
            "9000".to_string(),                            // SELECT EF_IMSI
            "084940340838994604".to_string() + SW_SUCCESS, // READ BINARY
        ]));
        assert_eq!(read_imsi(&mut t).unwrap(), "404438083996440");
    }

    #[test]
    fn read_imsi_retries_read_binary_on_wrong_le() {
        let mut t = QueuedTransport(std::collections::VecDeque::from([
            "9000".to_string(),
            "6C09".to_string(), // wrong Le -> retry with 9
            "084940340838994604".to_string() + SW_SUCCESS,
        ]));
        assert_eq!(read_imsi(&mut t).unwrap(), "404438083996440");
    }

    #[test]
    fn read_imsi_rejects_a_non_numeric_result() {
        // Length byte 0x01 (1 digit) over data that swaps to a non-digit —
        // exercises the sanity check rather than trusting the card blindly.
        let mut t = QueuedTransport(std::collections::VecDeque::from([
            "9000".to_string(),
            "01AB".to_string() + SW_SUCCESS,
        ]));
        assert!(read_imsi(&mut t).is_err());
    }

    #[test]
    fn read_mnc_length_decodes_ef_ad_byte_four() {
        // The same EF_AD content the modem path sees as
        // `+CRSM: 144,0,"00000002"` — a 2-digit-MNC card (e.g. 404/094).
        let mut t = QueuedTransport(std::collections::VecDeque::from([
            "9000".to_string(),                  // SELECT EF_AD
            "00000002".to_string() + SW_SUCCESS, // READ BINARY
        ]));
        assert_eq!(read_mnc_length(&mut t).unwrap(), 2);
    }

    #[test]
    fn read_mnc_length_reads_a_three_digit_mnc_card() {
        let mut t = QueuedTransport(std::collections::VecDeque::from([
            "9000".to_string(),
            "00000003".to_string() + SW_SUCCESS,
        ]));
        assert_eq!(read_mnc_length(&mut t).unwrap(), 3);
    }

    #[test]
    fn read_mnc_length_ignores_the_high_nibble() {
        // TS 31.102 reserves byte 4's high nibble; only the low one carries
        // the length, and real cards do set bits up there.
        let mut t = QueuedTransport(std::collections::VecDeque::from([
            "9000".to_string(),
            "000000F2".to_string() + SW_SUCCESS,
        ]));
        assert_eq!(read_mnc_length(&mut t).unwrap(), 2);
    }

    #[test]
    fn read_mnc_length_retries_read_binary_on_wrong_le() {
        // A real PC/SC reader enforces Le exactly, the same 6CXX path
        // read_imsi needed against the OmniKey AG 3x21.
        let mut t = QueuedTransport(std::collections::VecDeque::from([
            "9000".to_string(),
            "6C04".to_string(), // wrong Le -> retry with 4
            "00000002".to_string() + SW_SUCCESS,
        ]));
        assert_eq!(read_mnc_length(&mut t).unwrap(), 2);
    }

    #[test]
    fn read_mnc_length_rejects_an_unprogrammed_nibble() {
        // Legacy SIMs may leave it 0 — better to error and let the caller
        // fall back than to silently build the wrong realm.
        let mut t = QueuedTransport(std::collections::VecDeque::from([
            "9000".to_string(),
            "00000000".to_string() + SW_SUCCESS,
        ]));
        assert!(read_mnc_length(&mut t).is_err());
    }

    #[test]
    fn read_mnc_length_rejects_a_short_ef_ad() {
        let mut t = QueuedTransport(std::collections::VecDeque::from([
            "9000".to_string(),
            "0000".to_string() + SW_SUCCESS,
        ]));
        assert!(read_mnc_length(&mut t).is_err());
    }

    #[test]
    fn authenticate_works_over_a_generic_apdu_transport() {
        let res = [0x11u8; 8];
        let ck = [0x22u8; 16];
        let ik = [0x33u8; 16];
        let mut data = vec![0xDB, res.len() as u8];
        data.extend_from_slice(&res);
        data.push(ck.len() as u8);
        data.extend_from_slice(&ck);
        data.push(ik.len() as u8);
        data.extend_from_slice(&ik);
        let resp_hex = format!("{}{SW_SUCCESS}", hex_encode(&data));

        let mut t = QueuedTransport(std::collections::VecDeque::from([resp_hex]));
        match authenticate(&mut t, &[0u8; 16], &[0u8; 16]).unwrap() {
            AkaResult::Success {
                res: r,
                ck: c,
                ik: i,
            } => {
                assert_eq!(r, res);
                assert_eq!(c, ck);
                assert_eq!(i, ik);
            }
            AkaResult::SyncFailure { .. } => panic!("expected Success"),
        }
    }
}
