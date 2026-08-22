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
    // 4 = all messages, read and unread, in text mode.
    match at.send_command_within("AT+CMGL=\"ALL\"", BULK_LIST_BUDGET)? {
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

fn parse_cmgr_response(lines: &[String], index: u32) -> BridgeResult<IncomingSms> {
    let mut sender = String::new();
    let mut body = String::new();

    for (i, line) in lines.iter().enumerate() {
        if let Some(header) = line.strip_prefix("+CMGR: ") {
            let parts: Vec<&str> = header.split(',').collect();
            if parts.len() >= 2 {
                sender = parts[1].trim_matches('"').to_string();
            }
            if i + 1 < lines.len() {
                body = lines[i + 1..].join("\n");
            }
            break;
        }
    }

    if sender.is_empty() && body.is_empty() {
        return Err(BridgeError::Sms(format!(
            "could not parse CMGR response for index {index}"
        )));
    }

    Ok(IncomingSms {
        sender,
        body,
        index,
    })
}
