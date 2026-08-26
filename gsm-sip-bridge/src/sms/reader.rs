use crate::error::{BridgeError, BridgeResult};
use crate::modules::at_commander::{AtCommander, AtResponse, ResponseBudget};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct IncomingSms {
    pub sender: String,
    pub body: String,
    pub index: u32,
}

pub fn read_sms(at: &mut AtCommander, index: u32) -> BridgeResult<IncomingSms> {
    let cmd = format!("AT+CMGR={index}");
    match at.send_command(&cmd)? {
        AtResponse::Ok(lines) => parse_cmgr_response(&lines, index),
        AtResponse::Error(e) | AtResponse::CmeError(_, e) => Err(BridgeError::Sms(format!(
            "CMGR failed for index {index}: {e}"
        ))),
    }
}

/// What `AT+CMGL="ALL"` is allowed to spend, in place of the port's ordinary
/// per-command bounds.
///
/// Both defaults are wrong for this command, and both were observed to be
/// wrong on a live line (2026-08-22, 208 messages in storage: 461 lines,
/// 46,717 bytes):
///
/// * **Lines.** Text-mode `CMGL` emits a `+CMGL:` header plus at least one
///   body line per message, and a long or UCS2-encoded body runs to several.
///   Storage holds up to 255 messages, so budget for every one of them being
///   multi-line rather than for the average.
/// * **Time.** The default deadline bounds *wire time*, not just idleness. A
///   full store is ~56 KB, which at 115200 8N1 is close to five seconds of
///   transmission on a perfectly healthy line — already past
///   `DEFAULT_TIMEOUT`. The headroom here is for the modem's own paging
///   through storage on top of that.
///
/// Getting either one wrong is not a slow sweep but a permanently stuck one:
/// the sweep deletes only what it first managed to list, so a listing that
/// always aborts means storage only ever grows. See
/// [`crate::modules::at_commander::ResponseBudget`].
pub const BULK_LIST_BUDGET: ResponseBudget = ResponseBudget {
    max_lines: 4096,
    timeout: Duration::from_secs(30),
};

/// Lists the indexes of messages already sitting in the modem's storage.
///
/// Needed at startup: texts that arrived while nothing was reading the modem
/// would otherwise be stepped over and eventually lost when storage filled
/// (specs/017-volte-inbound-bridge US5).
pub fn list_sms_indexes(at: &mut AtCommander) -> BridgeResult<Vec<u32>> {
    // 4 = all messages, read and unread. In PDU mode (the caller has already
    // set `AT+CMGF=0` — see `volte::sms::sweep_modem_storage`) this is a bare
    // integer rather than the text-mode `"ALL"` string.
    match at.send_command_within("AT+CMGL=4", BULK_LIST_BUDGET)? {
        AtResponse::Ok(lines) => Ok(crate::volte::sms::parse_cmgl_indexes(&lines)),
        AtResponse::Error(e) | AtResponse::CmeError(_, e) => {
            Err(BridgeError::Sms(format!("CMGL failed: {e}")))
        }
    }
}

pub fn delete_sms(at: &mut AtCommander, index: u32) -> BridgeResult<()> {
    let cmd = format!("AT+CMGD={index}");
    match at.send_command(&cmd)? {
        AtResponse::Ok(_) => Ok(()),
        AtResponse::Error(e) | AtResponse::CmeError(_, e) => Err(BridgeError::Sms(format!(
            "CMGD failed for index {index}: {e}"
        ))),
    }
}

/// Parses a PDU-mode `AT+CMGR` response (TS 27.005 §3.1): a `+CMGR:
/// <stat>,[<alpha>],<length>` header line (the length counts the TPDU only,
/// not the SMSC prefix), followed by one line of hex digits carrying the
/// SMSC address field and the TPDU back to back. Decoded through
/// [`decode_pdu_line`] with the same TPDU parser the IMS `MESSAGE` route
/// uses — see its docs for why sharing that parser (rather than reading
/// text mode, as this used to) is load-bearing, not cosmetic.
fn parse_cmgr_response(lines: &[String], index: u32) -> BridgeResult<IncomingSms> {
    let header_pos = lines
        .iter()
        .position(|l| l.starts_with("+CMGR:"))
        .ok_or_else(|| {
            BridgeError::Sms(format!(
                "no +CMGR: header in the response for index {index}"
            ))
        })?;
    let hex_line = lines[header_pos + 1..]
        .iter()
        .find(|l| !l.trim().is_empty())
        .ok_or_else(|| {
            BridgeError::Sms(format!(
                "no PDU line after the +CMGR: header for index {index}"
            ))
        })?;
    decode_pdu_line(hex_line, index)
}

/// Decodes one hex-encoded PDU line — the shape both `AT+CMGR` and
/// `AT+CMGL`'s PDU mode hand back a message in. TS 27.005 §3.1: the first
/// byte is the SMSC address field's own length in octets (`0` when no SMSC
/// info is present, which some modems send even though the field is still
/// there), that many bytes are the SMSC address, and everything after is the
/// TPDU — decoded with the same parser `ims::sms_pdu` uses for the IMS
/// `MESSAGE` route, not a second one.
fn decode_pdu_line(hex: &str, index: u32) -> BridgeResult<IncomingSms> {
    let bytes = hex_decode(hex.trim())
        .ok_or_else(|| BridgeError::Sms(format!("PDU for index {index} is not valid hex")))?;
    let smsc_len = *bytes
        .first()
        .ok_or_else(|| BridgeError::Sms(format!("PDU for index {index} is empty")))?
        as usize;
    let tpdu = bytes.get(1 + smsc_len..).ok_or_else(|| {
        BridgeError::Sms(format!(
            "PDU for index {index}'s SMSC address length exceeds the buffer"
        ))
    })?;

    let decoded = crate::ims::sms_pdu::decode_sms_deliver_tpdu(tpdu)
        .map_err(|e| BridgeError::Sms(format!("could not decode PDU for index {index}: {e}")))?;
    let body = match decoded.part {
        Some((seq, total)) => format!("[{seq}/{total}] {}", decoded.text),
        None => decoded.text,
    };
    Ok(IncomingSms {
        sender: decoded.sender,
        body,
        index,
    })
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}
