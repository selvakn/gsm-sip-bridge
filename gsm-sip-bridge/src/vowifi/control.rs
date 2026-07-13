//! Newline-JSON control protocol between Agent A (`crate::ims::agent`) and
//! Agent B (`crate::vowifi`), carried over the dedicated veth link. See
//! `specs/011-vowifi-sip-bridge/contracts/agent-control-protocol.md`.
//!
//! Unlike `crate::control::protocol` (the CLI↔daemon `ControlCmd`/`ControlResp`
//! pair, which models synchronous request→single-response operations), this
//! protocol is event-driven in both directions: Agent A pushes
//! `IncomingCall`/`CallEnded` unprompted, Agent B pushes
//! `BridgeReady`/`BridgeFailed`/`HangupAck` unprompted. It therefore gets its
//! own small message type rather than overloading `ControlCmd`/`ControlResp`,
//! though the wire framing (newline-terminated JSON) and the
//! read/write-helper shape follow `control::protocol::read_cmd`/`write_resp`
//! exactly.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

/// One lifecycle event exchanged between the two agents. `call_id` always
/// correlates to the carrier-side SIP `Call-ID` for the call in question, so
/// log lines on both agents can be joined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ControlMessage {
    /// Agent A → Agent B. Sent the moment an inbound `INVITE` is parsed;
    /// Agent A blocks its own SIP response to the carrier until it gets a
    /// `BridgeReady`/`BridgeFailed` reply.
    IncomingCall { call_id: String, caller: String },
    /// Either direction. Whichever agent sees its own leg drop first sends
    /// this; the receiver tears its side down and does not echo it back.
    /// Agent A → Agent B when the carrier sends a `BYE` (or the caller
    /// `CANCEL`s while ringing); Agent B → Agent A when the PBX extension hangs
    /// up, which Agent A turns into a `BYE` toward the carrier.
    CallEnded { call_id: String, reason: String },
    /// Agent B → Agent A. Both the PBX-side and veth-side legs are placed
    /// and conference-bridged. The PBX leg is *ringing*, not yet answered —
    /// Agent A must keep the carrier in the ringing state (its `180 Ringing`
    /// is what makes the network play ringback to the caller) and wait for
    /// `CallAnswered` before sending `200 OK`.
    BridgeReady { call_id: String, veth_rtp_port: u16 },
    /// Agent B → Agent A. A human picked up the PBX extension (the PBX leg
    /// reached `Confirmed`). Only now may Agent A answer the carrier —
    /// answering any earlier cuts the caller's ringback off and leaves them
    /// listening to dead air while the extension is still ringing.
    CallAnswered { call_id: String },
    /// Agent B → Agent A. The PBX-side or veth-side leg could not be
    /// established; Agent A must decline the inbound INVITE (486 Busy Here).
    BridgeFailed { call_id: String, reason: String },
    /// Agent B → Agent A. Confirms both of Agent B's legs have been torn
    /// down in response to a `CallEnded`.
    HangupAck { call_id: String },
    /// `vowifi-status` → either agent. Not part of the call-signaling
    /// sequence above — a one-off query answered with whichever of
    /// `RegistrationStatusReply` (Agent A) / `CallHistoryReply` (Agent B)
    /// the receiving agent actually has to report (FR-008, User Story 3).
    StatusQuery,
    /// Agent A → `vowifi-status`. Current IMS/VoWiFi registration health
    /// (`ims::RegistrationStatus`, restated as wire-friendly types — unix
    /// timestamps rather than `SystemTime`, which isn't `Serialize`).
    RegistrationStatusReply {
        state: String,
        registered_at: Option<u64>,
        expires_at: Option<u64>,
        last_failure: Option<(u64, String)>,
    },
    /// Agent B → `vowifi-status`. Recent call outcomes, newest first.
    CallHistoryReply { calls: Vec<CallRecord> },
    /// Agent A → Agent B. An inbound SIP `MESSAGE` (RFC 3428) — the carrier's
    /// transport for SMS over VoWiFi/IMS, the counterpart to `AT+CMTI`/
    /// `AT+CMGR` in the circuit-switched bridge (`modules::mod::handle_cmti`).
    /// Not scoped to any call, so it carries no `call_id`. Agent A has
    /// already acknowledged the carrier (`200 OK`) by the time this is sent;
    /// Agent B forwards it to Discord using the same `[sms]` webhook config
    /// and embed format as the AT-command flow, since Agent B — unlike
    /// Agent A, confined to the IMS tunnel netns — has both that config and
    /// LAN/Internet reachability.
    SmsReceived {
        sender: String,
        body: String,
        received_at: String,
    },
}

impl ControlMessage {
    /// `None` for messages not scoped to one call (`StatusQuery` and its
    /// replies).
    pub fn call_id(&self) -> Option<&str> {
        match self {
            ControlMessage::IncomingCall { call_id, .. }
            | ControlMessage::CallEnded { call_id, .. }
            | ControlMessage::BridgeReady { call_id, .. }
            | ControlMessage::CallAnswered { call_id, .. }
            | ControlMessage::BridgeFailed { call_id, .. }
            | ControlMessage::HangupAck { call_id, .. } => Some(call_id),
            ControlMessage::StatusQuery
            | ControlMessage::RegistrationStatusReply { .. }
            | ControlMessage::CallHistoryReply { .. }
            | ControlMessage::SmsReceived { .. } => None,
        }
    }
}

/// One entry in Agent B's recent-call-outcome history
/// (`specs/011-vowifi-sip-bridge/data-model.md`'s "Bridged Call" entity,
/// the subset relevant to status reporting rather than live call state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallRecord {
    pub call_id: String,
    pub caller: String,
    /// Free-form outcome summary, e.g. `"answered"`, `"declined:busy"`,
    /// `"failed:pbx_unreachable"` — mirrors `reason`'s free-form-string
    /// convention rather than a closed enum, since new failure modes
    /// shouldn't require a wire-format change to report.
    pub outcome: String,
    pub started_at: u64,
    pub ended_at: Option<u64>,
}

/// `bridge_failed` reasons — kept as `&'static str` constants (rather than a
/// separate enum) since the field is a free-form diagnostic string on the
/// wire, per `contracts/agent-control-protocol.md`.
pub mod reason {
    pub const PBX_UNREACHABLE: &str = "pbx_unreachable";
    pub const PBX_REJECTED: &str = "pbx_rejected";
    /// Nobody picked up the PBX extension before the ring timeout.
    pub const PBX_NO_ANSWER: &str = "pbx_no_answer";
    /// The caller gave up (`CANCEL`) while the PBX extension was still ringing.
    pub const CALLER_CANCELLED: &str = "caller_cancelled";
    pub const VETH_LEG_FAILED: &str = "veth_leg_failed";
    pub const CALLER_HANGUP: &str = "caller_hangup";
    /// The PBX/SIP side hung up first. Agent A turns this into a `BYE` toward
    /// the carrier — either side dropping must end the whole bridged call.
    pub const PBX_HANGUP: &str = "pbx_hangup";
    pub const TRANSPORT_ERROR: &str = "transport_error";
}

/// Read one newline-terminated JSON `ControlMessage` from `reader`, blocking
/// until a full line is available. Mirrors
/// `crate::control::protocol::read_cmd`.
pub fn read_msg<R: BufRead>(reader: &mut R) -> Result<ControlMessage, String> {
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .map_err(|e| format!("read error: {e}"))?;
    if n == 0 {
        return Err("connection closed".to_string());
    }
    serde_json::from_str(line.trim()).map_err(|e| format!("parse error: {e}"))
}

/// Write one `ControlMessage` as a single newline-terminated JSON line.
/// Mirrors `crate::control::protocol::write_resp`.
pub fn write_msg<W: Write>(writer: &mut W, msg: &ControlMessage) -> Result<(), String> {
    let mut json = serde_json::to_string(msg).map_err(|e| format!("serialize error: {e}"))?;
    json.push('\n');
    writer
        .write_all(json.as_bytes())
        .map_err(|e| format!("write error: {e}"))?;
    writer.flush().map_err(|e| format!("flush error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip(msg: &ControlMessage) -> ControlMessage {
        let mut buf = Vec::new();
        write_msg(&mut buf, msg).unwrap();
        let mut cursor = Cursor::new(buf);
        read_msg(&mut cursor).unwrap()
    }

    #[test]
    fn incoming_call_roundtrips() {
        let msg = ControlMessage::IncomingCall {
            call_id: "a1b2c3".to_string(),
            caller: "+919789063708".to_string(),
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn call_ended_roundtrips() {
        let msg = ControlMessage::CallEnded {
            call_id: "a1b2c3".to_string(),
            reason: reason::CALLER_HANGUP.to_string(),
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn bridge_ready_roundtrips() {
        let msg = ControlMessage::BridgeReady {
            call_id: "a1b2c3".to_string(),
            veth_rtp_port: 40100,
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn call_answered_roundtrips() {
        let msg = ControlMessage::CallAnswered {
            call_id: "a1b2c3".to_string(),
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    /// `BridgeReady` and `CallAnswered` are distinct events and must stay
    /// distinguishable on the wire: conflating them is exactly the bug that
    /// made the caller hear dead air instead of ringback, because Agent A
    /// answered the carrier as soon as the PBX leg had been *placed* rather
    /// than when it was *answered*.
    #[test]
    fn bridge_ready_and_call_answered_are_distinct_events() {
        let mut ready = Vec::new();
        write_msg(
            &mut ready,
            &ControlMessage::BridgeReady {
                call_id: "c".to_string(),
                veth_rtp_port: 0,
            },
        )
        .unwrap();
        let mut answered = Vec::new();
        write_msg(
            &mut answered,
            &ControlMessage::CallAnswered {
                call_id: "c".to_string(),
            },
        )
        .unwrap();
        assert_ne!(ready, answered);
    }

    #[test]
    fn bridge_failed_roundtrips() {
        let msg = ControlMessage::BridgeFailed {
            call_id: "a1b2c3".to_string(),
            reason: reason::PBX_UNREACHABLE.to_string(),
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn hangup_ack_roundtrips() {
        let msg = ControlMessage::HangupAck {
            call_id: "a1b2c3".to_string(),
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn wire_format_matches_contract_shape() {
        let msg = ControlMessage::IncomingCall {
            call_id: "a1b2c3".to_string(),
            caller: "+919789063708".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"event":"incoming_call","call_id":"a1b2c3","caller":"+919789063708"}"#
        );
    }

    #[test]
    fn call_id_accessor_returns_correct_value_for_every_variant() {
        assert_eq!(
            ControlMessage::IncomingCall {
                call_id: "x".to_string(),
                caller: "y".to_string()
            }
            .call_id(),
            Some("x")
        );
        assert_eq!(
            ControlMessage::BridgeReady {
                call_id: "x".to_string(),
                veth_rtp_port: 1
            }
            .call_id(),
            Some("x")
        );
        assert_eq!(
            ControlMessage::HangupAck {
                call_id: "x".to_string()
            }
            .call_id(),
            Some("x")
        );
        assert_eq!(ControlMessage::StatusQuery.call_id(), None);
    }

    #[test]
    fn read_msg_reports_connection_closed_on_eof() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let err = read_msg(&mut cursor).unwrap_err();
        assert_eq!(err, "connection closed");
    }

    #[test]
    fn multiple_messages_can_be_read_sequentially_from_one_stream() {
        let mut buf = Vec::new();
        write_msg(
            &mut buf,
            &ControlMessage::IncomingCall {
                call_id: "1".to_string(),
                caller: "c".to_string(),
            },
        )
        .unwrap();
        write_msg(
            &mut buf,
            &ControlMessage::HangupAck {
                call_id: "1".to_string(),
            },
        )
        .unwrap();
        let mut cursor = Cursor::new(buf);
        let first = read_msg(&mut cursor).unwrap();
        let second = read_msg(&mut cursor).unwrap();
        assert!(matches!(first, ControlMessage::IncomingCall { .. }));
        assert!(matches!(second, ControlMessage::HangupAck { .. }));
    }

    #[test]
    fn sms_received_roundtrips() {
        let msg = ControlMessage::SmsReceived {
            sender: "+919789063708".to_string(),
            body: "hello over VoWiFi".to_string(),
            received_at: "2026-07-13T00:00:00+00:00".to_string(),
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn sms_received_has_no_call_id() {
        assert_eq!(
            ControlMessage::SmsReceived {
                sender: "+919789063708".to_string(),
                body: "hi".to_string(),
                received_at: "2026-07-13T00:00:00+00:00".to_string(),
            }
            .call_id(),
            None
        );
    }

    #[test]
    fn status_query_roundtrips() {
        assert_eq!(
            roundtrip(&ControlMessage::StatusQuery),
            ControlMessage::StatusQuery
        );
    }

    #[test]
    fn registration_status_reply_roundtrips_with_failure() {
        let msg = ControlMessage::RegistrationStatusReply {
            state: "Registered".to_string(),
            registered_at: Some(1_700_000_000),
            expires_at: Some(1_700_003_600),
            last_failure: Some((1_699_999_000, "timed out".to_string())),
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn registration_status_reply_roundtrips_when_never_registered() {
        let msg = ControlMessage::RegistrationStatusReply {
            state: "Unregistered".to_string(),
            registered_at: None,
            expires_at: None,
            last_failure: None,
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn call_history_reply_roundtrips() {
        let msg = ControlMessage::CallHistoryReply {
            calls: vec![
                CallRecord {
                    call_id: "1".to_string(),
                    caller: "+919789063708".to_string(),
                    outcome: "answered".to_string(),
                    started_at: 1_700_000_000,
                    ended_at: Some(1_700_000_300),
                },
                CallRecord {
                    call_id: "2".to_string(),
                    caller: "+919000000000".to_string(),
                    outcome: "declined:busy".to_string(),
                    started_at: 1_700_000_500,
                    ended_at: Some(1_700_000_500),
                },
            ],
        };
        assert_eq!(roundtrip(&msg), msg);
    }
}
