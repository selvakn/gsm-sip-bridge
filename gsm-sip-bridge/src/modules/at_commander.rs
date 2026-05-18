use crate::error::{BridgeError, BridgeResult};
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::str::FromStr;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkType {
    FourGLte,
    ThreeGUmts,
    TwoGEdge,
    NoSignal,
    NoSim,
    Unknown,
}

impl fmt::Display for NetworkType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetworkType::FourGLte => write!(f, "4G/LTE"),
            NetworkType::ThreeGUmts => write!(f, "3G/UMTS"),
            NetworkType::TwoGEdge => write!(f, "2G/EDGE"),
            NetworkType::NoSignal => write!(f, "No Signal"),
            NetworkType::NoSim => write!(f, "No SIM"),
            NetworkType::Unknown => write!(f, "Unknown"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkMode {
    Auto,
    Gsm,
    Wcdma,
    Lte,
}

impl NetworkMode {
    pub fn at_value(self) -> u8 {
        match self {
            NetworkMode::Auto => 0,
            NetworkMode::Gsm => 1,
            NetworkMode::Wcdma => 2,
            NetworkMode::Lte => 3,
        }
    }

    pub fn from_at_value(v: u8) -> Option<Self> {
        match v {
            0 => Some(NetworkMode::Auto),
            1 => Some(NetworkMode::Gsm),
            2 => Some(NetworkMode::Wcdma),
            3 => Some(NetworkMode::Lte),
            _ => None,
        }
    }
}

impl fmt::Display for NetworkMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetworkMode::Auto => write!(f, "auto"),
            NetworkMode::Gsm => write!(f, "2g"),
            NetworkMode::Wcdma => write!(f, "3g"),
            NetworkMode::Lte => write!(f, "4g"),
        }
    }
}

impl FromStr for NetworkMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(NetworkMode::Auto),
            "2g" | "gsm" => Ok(NetworkMode::Gsm),
            "3g" | "wcdma" => Ok(NetworkMode::Wcdma),
            "4g" | "lte" => Ok(NetworkMode::Lte),
            _ => Err(format!("unknown network mode: {s}")),
        }
    }
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

    pub fn query_imei(&mut self) -> BridgeResult<String> {
        match self.send_command("AT+CGSN")? {
            AtResponse::Ok(lines) => lines
                .into_iter()
                .find(|l| !l.is_empty())
                .ok_or_else(|| BridgeError::Discovery("AT+CGSN: no IMEI in response".into())),
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => {
                Err(BridgeError::Discovery(format!("AT+CGSN failed: {e}")))
            }
        }
    }

    pub fn query_phone_number(&mut self) -> BridgeResult<String> {
        match self.send_command("AT+CNUM")? {
            AtResponse::Ok(lines) => {
                for line in &lines {
                    if let Some(rest) = line.strip_prefix("+CNUM:") {
                        // +CNUM: "","+91XXXXXXXXXX",145
                        let parts: Vec<&str> = rest.splitn(3, ',').collect();
                        if parts.len() >= 2 {
                            let num = parts[1].trim().trim_matches('"');
                            if !num.is_empty() {
                                return Ok(num.to_string());
                            }
                        }
                    }
                }
                Ok("Unknown".to_string())
            }
            AtResponse::Error(_) | AtResponse::CmeError(_, _) => Ok("Unknown".to_string()),
        }
    }

    pub fn query_network_type(&mut self) -> BridgeResult<NetworkType> {
        match self.send_command("AT+QNWINFO")? {
            AtResponse::Ok(lines) => {
                for line in &lines {
                    if let Some(rest) = line.strip_prefix("+QNWINFO:") {
                        // +QNWINFO: "FDD LTE","46001","LTE BAND 3",1825
                        let act = rest
                            .trim()
                            .split(',')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .trim_matches('"');
                        let nt = if act.contains("LTE") {
                            NetworkType::FourGLte
                        } else if act.contains("WCDMA")
                            || act.contains("UMTS")
                            || act.contains("HSPA")
                        {
                            NetworkType::ThreeGUmts
                        } else if act.contains("GSM")
                            || act.contains("GPRS")
                            || act.contains("EDGE")
                        {
                            NetworkType::TwoGEdge
                        } else {
                            NetworkType::NoSignal
                        };
                        return Ok(nt);
                    }
                }
                Ok(NetworkType::NoSignal)
            }
            AtResponse::Error(_) | AtResponse::CmeError(_, _) => Ok(NetworkType::NoSignal),
        }
    }

    pub fn query_network_mode(&mut self) -> BridgeResult<NetworkMode> {
        match self.send_command(r#"AT+QCFG="nwscanmode""#)? {
            AtResponse::Ok(lines) => {
                for line in &lines {
                    if let Some(rest) = line.strip_prefix(r#"+QCFG: "nwscanmode","#) {
                        let val: u8 = rest
                            .trim()
                            .split(',')
                            .next()
                            .unwrap_or("0")
                            .trim()
                            .parse()
                            .unwrap_or(0);
                        return Ok(NetworkMode::from_at_value(val).unwrap_or(NetworkMode::Auto));
                    }
                    // Some firmware omits the quotes around value:
                    if let Some(rest) = line.strip_prefix("+QCFG: \"nwscanmode\",") {
                        let val: u8 = rest
                            .trim()
                            .split(',')
                            .next()
                            .unwrap_or("0")
                            .trim()
                            .parse()
                            .unwrap_or(0);
                        return Ok(NetworkMode::from_at_value(val).unwrap_or(NetworkMode::Auto));
                    }
                }
                Ok(NetworkMode::Auto)
            }
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => Err(BridgeError::Discovery(
                format!("query network mode failed: {e}"),
            )),
        }
    }

    pub fn set_network_mode(&mut self, mode: NetworkMode) -> BridgeResult<NetworkMode> {
        let cmd = format!(r#"AT+QCFG="nwscanmode",{}"#, mode.at_value());
        match self.send_command(&cmd)? {
            AtResponse::Ok(_) => {}
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => {
                return Err(BridgeError::Discovery(format!(
                    "set network mode failed: {e}"
                )));
            }
        }
        // Verify the change took effect
        let confirmed = self.query_network_mode()?;
        if confirmed != mode {
            return Err(BridgeError::Discovery(format!(
                "network mode mismatch after set: expected {mode}, got {confirmed}"
            )));
        }
        Ok(confirmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Mock stream: reads from a fixed byte buffer, discards writes.
    struct MockStream {
        reader: Cursor<Vec<u8>>,
    }

    impl MockStream {
        fn new(response: &str) -> Self {
            Self {
                reader: Cursor::new(response.as_bytes().to_vec()),
            }
        }
    }

    impl Read for MockStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reader.read(buf)
        }
    }

    impl Write for MockStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn make_commander(response: &str) -> AtCommander {
        AtCommander::from_stream(MockStream::new(response), Duration::from_secs(1))
    }

    #[test]
    fn test_query_imei() {
        let mut at = make_commander("867584030123456\r\nOK\r\n");
        assert_eq!(at.query_imei().unwrap(), "867584030123456");
    }

    #[test]
    fn test_query_phone_number_present() {
        let mut at = make_commander("+CNUM: \"\",\"+91XXXXXXXXXX\",145\r\nOK\r\n");
        assert_eq!(at.query_phone_number().unwrap(), "+91XXXXXXXXXX");
    }

    #[test]
    fn test_query_phone_number_error() {
        let mut at = make_commander("ERROR\r\n");
        assert_eq!(at.query_phone_number().unwrap(), "Unknown");
    }

    #[test]
    fn test_query_network_type_lte() {
        let mut at =
            make_commander("+QNWINFO: \"FDD LTE\",\"46001\",\"LTE BAND 3\",1825\r\nOK\r\n");
        assert_eq!(at.query_network_type().unwrap(), NetworkType::FourGLte);
    }

    #[test]
    fn test_query_network_type_wcdma() {
        let mut at = make_commander("+QNWINFO: \"WCDMA\",\"46001\",\"WCDMA 850\",4400\r\nOK\r\n");
        assert_eq!(at.query_network_type().unwrap(), NetworkType::ThreeGUmts);
    }

    #[test]
    fn test_query_network_type_gsm() {
        let mut at = make_commander("+QNWINFO: \"GSM\",\"46001\",\"GSM 900\",80\r\nOK\r\n");
        assert_eq!(at.query_network_type().unwrap(), NetworkType::TwoGEdge);
    }

    #[test]
    fn test_query_network_type_no_signal() {
        let mut at = make_commander("ERROR\r\n");
        assert_eq!(at.query_network_type().unwrap(), NetworkType::NoSignal);
    }

    #[test]
    fn test_query_network_mode() {
        let mut at = make_commander("+QCFG: \"nwscanmode\",3\r\nOK\r\n");
        assert_eq!(at.query_network_mode().unwrap(), NetworkMode::Lte);
    }

    #[test]
    fn test_network_mode_from_str() {
        assert_eq!("4g".parse::<NetworkMode>().unwrap(), NetworkMode::Lte);
        assert_eq!("2g".parse::<NetworkMode>().unwrap(), NetworkMode::Gsm);
        assert_eq!("3g".parse::<NetworkMode>().unwrap(), NetworkMode::Wcdma);
        assert_eq!("auto".parse::<NetworkMode>().unwrap(), NetworkMode::Auto);
        assert!("5g".parse::<NetworkMode>().is_err());
    }

    #[test]
    fn test_network_mode_display() {
        assert_eq!(NetworkMode::Lte.to_string(), "4g");
        assert_eq!(NetworkMode::Gsm.to_string(), "2g");
        assert_eq!(NetworkMode::Wcdma.to_string(), "3g");
        assert_eq!(NetworkMode::Auto.to_string(), "auto");
    }
}
