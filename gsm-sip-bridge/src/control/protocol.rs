use crate::modules::at_commander::NetworkMode;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ControlCmd {
    CardRestart {
        slot: u32,
    },
    SetMode {
        slot: u32,
        mode: String,
    },
    GetMode {
        slot: u32,
    },
    ListSlots,
    /// A VoWiFi agent (`ims::agent` or `vowifi::mod`) reporting call/SMS
    /// events and current gauge state (specs/014-vowifi-metrics-restore).
    /// Routed straight to `metrics::ingest::apply_report` by
    /// `control::server::handle_connection`, never reaching `CardPool`'s
    /// mailbox — see contracts/observability-protocol.md.
    Observe {
        report: AgentReport,
    },
}

/// Which VoWiFi agent sent an `AgentReport`. Liveness (`AGENT_UP`) is
/// tracked per kind, independently of `module_id` — a card can be replaced
/// or fail to resolve, but the agent process identity is always one of
/// these two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    /// Agent A: `ims::agent`, runs inside the ePDG tunnel's `ims` netns.
    Ims,
    /// Agent B: `vowifi::mod`, runs in the default netns.
    Sip,
}

impl AgentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentKind::Ims => "ims",
            AgentKind::Sip => "sip",
        }
    }
}

/// One message an agent sends over the observability protocol: absolute
/// gauge state (always present, which is what makes an empty-`events`
/// report a heartbeat) plus zero or more counter deltas since the last
/// successfully delivered report.
///
/// `epoch`/`seq` make a report idempotent to replay: the reporter's
/// send/retry loop is single-threaded per agent (one report in flight at a
/// time), so if the daemon applies a report but the acknowledgement is lost
/// — a torn connection right as the response was written — the reporter
/// retries the *same* report rather than knowing it already landed. `seq` is
/// a per-agent-process counter assigned once when a report is enqueued and
/// kept across retries of that same report; `epoch` is a random value fixed
/// for the reporter's process lifetime, so a restarted agent's `seq`
/// resetting to 1 is never mistaken for a replay of a previous run's
/// already-applied `seq` values (`metrics::ingest` only compares `seq`
/// within a matching `epoch`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentReport {
    pub agent: AgentKind,
    pub module_id: String,
    pub epoch: u64,
    pub seq: u64,
    pub state: AgentState,
    #[serde(default)]
    pub events: Vec<ObservedEvent>,
    #[serde(default)]
    pub dropped: u64,
}

/// Absolute, latest-wins gauge state. `None` means "this agent does not
/// report this signal" — distinct from `Some(false)`, which means "reports
/// it, and it is currently down". The daemon never invents a value for
/// `None` (data-model.md §1).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct AgentState {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub active_calls: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub registered: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tunnel_up: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pbx_registered: Option<bool>,
}

/// A counter delta or histogram observation. Every enumerated field below is
/// a closed Rust enum rather than a free string — the mechanism that keeps
/// metric label cardinality bounded regardless of what an agent observes
/// (FR-014).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ObservedEvent {
    CallCompleted {
        status: CallStatus,
        duration_seconds: f64,
    },
    PbxLegCompleted {
        outcome: SmsOutcome,
    },
    BridgeFailed {
        reason: BridgeFailureReason,
    },
    SmsReceived,
    SmsForwarded {
        outcome: SmsOutcome,
    },
    RegistrationAttempt {
        status: RegistrationStatus,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallStatus {
    Answered,
    Missed,
    Failed,
}

impl CallStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            CallStatus::Answered => "answered",
            CallStatus::Missed => "missed",
            CallStatus::Failed => "failed",
        }
    }
}

/// Reused for both `PbxLegCompleted`'s outcome (`success`/`failed`, matching
/// `modules::mod`'s existing `SIP_CALLS_TOTAL` status values) and
/// `SmsForwarded`'s outcome (`sent`/`failed`, matching the existing
/// `SMS_FORWARDED_TOTAL` values) — same two-value shape, different label
/// vocabulary per call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmsOutcome {
    Sent,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeFailureReason {
    BridgeSetupFailed,
    RingTimeout,
    CallerCancelled,
    PbxDeclined,
    AgentUnreachable,
}

impl BridgeFailureReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            BridgeFailureReason::BridgeSetupFailed => "bridge_setup_failed",
            BridgeFailureReason::RingTimeout => "ring_timeout",
            BridgeFailureReason::CallerCancelled => "caller_cancelled",
            BridgeFailureReason::PbxDeclined => "pbx_declined",
            BridgeFailureReason::AgentUnreachable => "agent_unreachable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationStatus {
    Success,
    AuthFailed,
    Rejected,
    Timeout,
}

impl RegistrationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RegistrationStatus::Success => "success",
            RegistrationStatus::AuthFailed => "auth_failed",
            RegistrationStatus::Rejected => "rejected",
            RegistrationStatus::Timeout => "timeout",
        }
    }
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
