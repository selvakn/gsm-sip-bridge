use crate::modules::at_commander::NetworkMode;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ControlCmd {
    CardRestart { slot: u32 },
    SetMode { slot: u32, mode: String },
    GetMode { slot: u32 },
    ListSlots,
}

#[derive(Debug, Clone)]
pub enum ControlResp {
    Ok,
    OkMode { mode: String },
    OkSlots { slots: Vec<SlotInfo> },
    Err { error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotInfo {
    pub slot: u32,
    pub state: String,
    pub phone: String,
    pub network: String,
}

pub fn read_cmd<R: BufRead>(reader: &mut R) -> Result<ControlCmd, String> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read error: {e}"))?;
    serde_json::from_str(line.trim()).map_err(|e| format!("parse error: {e}"))
}

pub fn write_resp<W: Write>(writer: &mut W, resp: &ControlResp) -> Result<(), String> {
    let mut json = serde_json::to_string(resp).map_err(|e| format!("serialize error: {e}"))?;
    json.push('\n');
    writer
        .write_all(json.as_bytes())
        .map_err(|e| format!("write error: {e}"))?;
    Ok(())
}

impl ControlResp {
    pub fn ok() -> Self {
        ControlResp::Ok
    }

    pub fn ok_mode(mode: NetworkMode) -> Self {
        ControlResp::OkMode {
            mode: mode.to_string(),
        }
    }

    pub fn ok_slots(slots: Vec<SlotInfo>) -> Self {
        ControlResp::OkSlots { slots }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        ControlResp::Err { error: msg.into() }
    }
}

impl Serialize for ControlResp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            ControlResp::Ok => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("ok", &true)?;
                map.end()
            }
            ControlResp::OkMode { mode } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("ok", &true)?;
                map.serialize_entry("mode", mode)?;
                map.end()
            }
            ControlResp::OkSlots { slots } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("ok", &true)?;
                map.serialize_entry("slots", slots)?;
                map.end()
            }
            ControlResp::Err { error } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("ok", &false)?;
                map.serialize_entry("error", error)?;
                map.end()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_cmd_card_restart_roundtrip() {
        let json = r#"{"cmd":"card_restart","slot":0}"#;
        let cmd: ControlCmd = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd, ControlCmd::CardRestart { slot: 0 }));
    }

    #[test]
    fn test_cmd_set_mode_roundtrip() {
        let json = r#"{"cmd":"set_mode","slot":1,"mode":"4g"}"#;
        let cmd: ControlCmd = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd, ControlCmd::SetMode { slot: 1, .. }));
    }

    #[test]
    fn test_cmd_get_mode_roundtrip() {
        let json = r#"{"cmd":"get_mode","slot":2}"#;
        let cmd: ControlCmd = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd, ControlCmd::GetMode { slot: 2 }));
    }

    #[test]
    fn test_cmd_list_slots_roundtrip() {
        let json = r#"{"cmd":"list_slots"}"#;
        let cmd: ControlCmd = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd, ControlCmd::ListSlots));
    }

    #[test]
    fn test_resp_ok_serializes() {
        let resp = ControlResp::ok();
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"ok":true}"#);
    }

    #[test]
    fn test_resp_ok_mode_serializes() {
        let resp = ControlResp::OkMode {
            mode: "4g".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"ok":true,"mode":"4g"}"#);
    }

    #[test]
    fn test_resp_err_serializes() {
        let resp = ControlResp::err("slot 5 not found");
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"ok":false,"error":"slot 5 not found"}"#);
    }

    #[test]
    fn test_read_cmd_and_write_resp() {
        let input = b"{ \"cmd\": \"list_slots\" }\n";
        let mut reader = Cursor::new(input.as_slice());
        let cmd = read_cmd(&mut reader).unwrap();
        assert!(matches!(cmd, ControlCmd::ListSlots));

        let mut output = Vec::new();
        write_resp(&mut output, &ControlResp::ok()).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "{\"ok\":true}\n");
    }
}
