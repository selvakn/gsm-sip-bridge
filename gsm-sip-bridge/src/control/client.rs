use crate::control::protocol::{ControlCmd, ControlResp};
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub fn send_cmd(socket_path: &str, cmd: &ControlCmd) -> Result<ControlResp, String> {
    let stream = UnixStream::connect(socket_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound
            || e.kind() == std::io::ErrorKind::ConnectionRefused
        {
            format!("daemon not running (socket not found at {socket_path})")
        } else {
            format!("failed to connect to control socket: {e}")
        }
    })?;

    stream
        .set_read_timeout(Some(Duration::from_secs(35)))
        .map_err(|e| format!("set_read_timeout: {e}"))?;

    let read_stream = stream
        .try_clone()
        .map_err(|e| format!("clone stream: {e}"))?;
    let mut writer = stream;
    let mut reader = BufReader::new(read_stream);

    // Write the command using the same framing as the server
    let mut json = serde_json::to_string(cmd).map_err(|e| format!("serialize command: {e}"))?;
    json.push('\n');
    writer
        .write_all(json.as_bytes())
        .map_err(|e| format!("write command: {e}"))?;

    // Read response
    let mut line = String::new();
    use std::io::BufRead;
    reader.read_line(&mut line).map_err(|e| {
        if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock {
            "timed out waiting for daemon response".to_string()
        } else {
            format!("read response: {e}")
        }
    })?;

    parse_resp(line.trim())
}

fn parse_resp(json: &str) -> Result<ControlResp, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("parse response: {e}"))?;
    let ok = v.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if !ok {
        let error = v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown error")
            .to_string();
        return Ok(ControlResp::Err { error });
    }
    if let Some(mode) = v.get("mode").and_then(|m| m.as_str()) {
        return Ok(ControlResp::OkMode {
            mode: mode.to_string(),
        });
    }
    if let Some(slots) = v.get("slots") {
        let slots: Vec<crate::control::protocol::SlotInfo> =
            serde_json::from_value(slots.clone()).map_err(|e| format!("parse slots: {e}"))?;
        return Ok(ControlResp::OkSlots { slots });
    }
    Ok(ControlResp::Ok)
}
