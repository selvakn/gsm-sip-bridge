use crate::error::{BridgeError, BridgeResult};
use crate::modules::at_commander::{AtCommander, AtResponse};

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

/// Lists the indexes of messages already sitting in the modem's storage.
///
/// Needed at startup: texts that arrived while nothing was reading the modem
/// would otherwise be stepped over and eventually lost when storage filled
/// (specs/017-volte-inbound-bridge US5).
pub fn list_sms_indexes(at: &mut AtCommander) -> BridgeResult<Vec<u32>> {
    // 4 = all messages, read and unread, in text mode.
    match at.send_command("AT+CMGL=\"ALL\"")? {
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
