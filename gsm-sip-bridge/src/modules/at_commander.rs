use crate::error::{BridgeError, BridgeResult};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const BAUD_RATE: u32 = 115200;

pub struct AtCommander {
    port: Box<dyn ReadWrite + Send>,
}

pub trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

#[derive(Debug, Clone)]
pub enum AtResponse {
    Ok(Vec<String>),
    Error(String),
    CmeError(u32, String),
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum Urc {
    Ring,
    Clip(String),
    Cmti { storage: String, index: u32 },
    NoCarrier,
}

impl AtCommander {
    pub fn open(path: &Path) -> BridgeResult<Self> {
        let port = serialport::new(path.to_string_lossy(), BAUD_RATE)
            .timeout(DEFAULT_TIMEOUT)
            .open()
            .map_err(|e| {
                BridgeError::Discovery(format!("failed to open serial {}: {e}", path.display()))
            })?;
        Ok(Self {
            port: Box::new(port),
        })
    }

    pub fn from_stream<S: Read + Write + Send + 'static>(stream: S, _timeout: Duration) -> Self {
        Self {
            port: Box::new(stream),
        }
    }

    pub fn send_command(&mut self, cmd: &str) -> BridgeResult<AtResponse> {
        let full_cmd = format!("{cmd}\r\n");
        let port = self.port.as_mut();
        port.write_all(full_cmd.as_bytes())
            .map_err(|e| BridgeError::Discovery(format!("AT write failed: {e}")))?;
        port.flush()
            .map_err(|e| BridgeError::Discovery(format!("AT flush failed: {e}")))?;

        tracing::trace!(target: "at", cmd = cmd, "sent");
        self.read_response()
    }

    fn read_response(&mut self) -> BridgeResult<AtResponse> {
        let mut reader = BufReader::new(self.port.as_mut() as &mut dyn Read);
        let mut lines = Vec::new();

        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim().to_string();
                    if trimmed.is_empty() {
                        continue;
                    }
                    tracing::trace!(target: "at", line = %trimmed, "recv");
                    if trimmed == "OK" {
                        return Ok(AtResponse::Ok(lines));
                    } else if trimmed == "ERROR" {
                        return Ok(AtResponse::Error("ERROR".into()));
                    } else if let Some(cme) = trimmed.strip_prefix("+CME ERROR: ") {
                        let code = cme.parse::<u32>().unwrap_or(0);
                        return Ok(AtResponse::CmeError(code, cme.into()));
                    } else {
                        lines.push(trimmed);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    return Err(BridgeError::Discovery("AT command timeout".into()));
                }
                Err(e) => {
                    return Err(BridgeError::Discovery(format!("AT read error: {e}")));
                }
            }
        }
        Ok(AtResponse::Ok(lines))
    }

    pub fn read_line_raw(&mut self) -> BridgeResult<String> {
        let mut buf = [0u8; 1];
        let mut line = Vec::new();

        loop {
            match self.port.as_mut().read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {
                    if buf[0] == b'\n' {
                        break;
                    }
                    if buf[0] != b'\r' {
                        line.push(buf[0]);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    if line.is_empty() {
                        return Err(BridgeError::Discovery("AT read timeout".into()));
                    }
                    break;
                }
                Err(e) => {
                    return Err(BridgeError::Discovery(format!("AT read error: {e}")));
                }
            }
        }

        Ok(String::from_utf8_lossy(&line).to_string())
    }

    pub fn check_signal(&mut self) -> BridgeResult<(u8, u8)> {
        match self.send_command("AT+CSQ")? {
            AtResponse::Ok(lines) => {
                for line in &lines {
                    if let Some(values) = line.strip_prefix("+CSQ: ") {
                        let parts: Vec<&str> = values.split(',').collect();
                        if parts.len() == 2 {
                            let rssi = parts[0].trim().parse().unwrap_or(99);
                            let ber = parts[1].trim().parse().unwrap_or(99);
                            return Ok((rssi, ber));
                        }
                    }
                }
                Err(BridgeError::Discovery("unexpected CSQ response".into()))
            }
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => {
                Err(BridgeError::Discovery(format!("CSQ failed: {e}")))
            }
        }
    }

    pub fn answer_call(&mut self) -> BridgeResult<()> {
        match self.send_command("ATA")? {
            AtResponse::Ok(_) => Ok(()),
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => {
                Err(BridgeError::Discovery(format!("ATA failed: {e}")))
            }
        }
    }

    pub fn hangup(&mut self) -> BridgeResult<()> {
        match self.send_command("AT+CHUP")? {
            AtResponse::Ok(_) => Ok(()),
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => {
                Err(BridgeError::Discovery(format!("CHUP failed: {e}")))
            }
        }
    }
}
