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

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

fn hex_decode(s: &str) -> BridgeResult<Vec<u8>> {
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
fn csim(at: &mut AtCommander, apdu_hex: &str) -> BridgeResult<String> {
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
fn select_fid(at: &mut AtCommander, fid: u16) -> BridgeResult<()> {
    let apdu = format!("00A4000C02{fid:04X}");
    let resp = csim(at, &apdu)?;
    if !resp.ends_with(SW_SUCCESS) {
        return Err(BridgeError::Ims(format!(
            "SELECT {fid:04X} failed: SW={resp}"
        )));
    }
    Ok(())
}

/// SELECT by AID (application), P2=0x0C.
fn select_aid(at: &mut AtCommander, aid: &[u8]) -> BridgeResult<()> {
    let apdu = format!("00A4040C{:02X}{}", aid.len(), hex_encode(aid));
    let resp = csim(at, &apdu)?;
    if !resp.ends_with(SW_SUCCESS) {
        return Err(BridgeError::Ims(format!(
            "SELECT AID {} failed: SW={resp}",
            hex_encode(aid)
        )));
    }
    Ok(())
}

/// READ RECORD (mode: absolute, from current EF), P2=0x04.
fn read_record(at: &mut AtCommander, record: u8) -> BridgeResult<Option<String>> {
    let apdu = format!("00B2{record:02X}0400");
    let resp = csim(at, &apdu)?;
    if resp.ends_with(SW_SUCCESS) && resp.len() > 4 {
        Ok(Some(resp[..resp.len() - 4].to_string()))
    } else {
        Ok(None)
    }
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
pub fn discover_usim_aid(at: &mut AtCommander) -> BridgeResult<Vec<u8>> {
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
pub fn select_usim(at: &mut AtCommander, aid: &[u8]) -> BridgeResult<()> {
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
    at: &mut AtCommander,
    rand: &[u8; 16],
    autn: &[u8; 16],
) -> BridgeResult<AkaResult> {
    let apdu = format!("008800812210{}10{}", hex_encode(rand), hex_encode(autn));
    let mut resp = csim(at, &apdu)?;

    if resp.len() >= 4 {
        let sw = &resp[resp.len() - 4..];
        if sw.starts_with("61") {
            let le = &sw[2..4];
            let follow_up = format!("00C00000{le}");
            resp = csim(at, &follow_up)?;
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
}
