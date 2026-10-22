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

use crate::amr_forward::AmrLegParams;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

/// The P-CSCF part of a [`ControlMessage::RegistrationStatusReply`]
/// (specs/091 `contracts/control.md`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PcscfStatusReply {
    /// The P-CSCF the line registers through, or is attempting.
    pub active: String,
    /// The candidates, in selection order.
    #[serde(default)]
    pub pool: Vec<String>,
    /// Each P-CSCF currently marked unavailable, with the unix second its
    /// mark ends.
    #[serde(default)]
    pub unavailable: Vec<(String, u64)>,
}

/// One lifecycle event exchanged between the two agents. `call_id` always
/// correlates to the carrier-side SIP `Call-ID` for the call in question, so
/// log lines on both agents can be joined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ControlMessage {
    /// Agent A → Agent B. Sent the moment an inbound `INVITE` is parsed;
    /// Agent A blocks its own SIP response to the carrier until it gets a
    /// `BridgeReady`/`BridgeFailed` reply.
    IncomingCall {
        call_id: String,
        caller: String,
        /// The carrier's CNAP display name (confirmed live 2026-09-03: sent
        /// unprompted in `From`/`P-Asserted-Identity`), already withheld by
        /// Agent A when the INVITE carried `Privacy: id`/`user`. `None` when
        /// absent or withheld. `#[serde(default)]` so a message from an
        /// older Agent A that omits it still parses.
        #[serde(default)]
        caller_name: Option<String>,
        /// What the carrier leg negotiated, when it is AMR and Agent A has
        /// `amr_passthrough` on (specs/120). Absent otherwise, and from an
        /// older Agent A: Agent B then never attempts an upgrade.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        carrier_amr: Option<AmrLegParams>,
    },
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
    /// Agent B → Agent A (specs/120). The client answered and Agent B judged
    /// the call forwardable: forward the carrier's AMR frames to
    /// `<Agent B's veth address>:rtp_port` instead of transcoding. `client` is
    /// what the client negotiated, so Agent A can re-check the rule.
    ForwardMedia {
        call_id: String,
        rtp_port: u16,
        client: AmrLegParams,
    },
    /// Agent A → Agent B. Reply to `ForwardMedia`. `ok: false` carries a
    /// `Reason` string and leaves the call transcoded.
    ForwardAck {
        call_id: String,
        ok: bool,
        #[serde(default)]
        reason: Option<String>,
    },
    /// Agent B → Agent A. Abandon an acked forward (the client's re-INVITE
    /// failed, or the re-check failed): discard the pending forwarding socket
    /// and use the transcoding relay.
    ForwardCancel { call_id: String },
    /// Agent B → Agent A. The PBX-side or veth-side leg could not be
    /// established, or ended before anyone answered; Agent A must decline the
    /// inbound INVITE. `reason` decides how: only `reason::PBX_REJECTED` is
    /// answered `486 Busy Here`, everything else `480 Temporarily
    /// Unavailable` (specs/084, issue #108).
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
        /// Whether an inbound call could actually be answered right now
        /// (`ims::lifecycle::ServiceHealth::can_answer`). `#[serde(default)]`
        /// so a reply from an older peer that omits it still parses — it then
        /// reads `false`, and `blocked_reason` below carries the "why".
        #[serde(default)]
        can_answer: bool,
        /// Why the service cannot answer, when it cannot; `None` when it can or
        /// when the peer did not report health.
        #[serde(default)]
        blocked_reason: Option<String>,
        /// Rendered Gm signaling-connection health (specs/028): `up`,
        /// `reconnecting since <ts> (attempt N)`, or `failed since <ts>`.
        /// `#[serde(default)]` so a reply from an older peer that omits it
        /// still parses — it then reads empty, which the CLI prints as
        /// `unknown` rather than claiming health it was not told about.
        #[serde(default)]
        gm_connection: String,
        /// While a reg-event `rejected` holds registration off, when the next
        /// attempt goes out (unix seconds) — specs/089 FR-017. `None` when no
        /// cooldown is running, and from an older peer that omits it.
        #[serde(default)]
        rejection_cooldown_until: Option<u64>,
        /// Rendered reg-event subscription health (specs/090 FR-020), e.g.
        /// `confirmed, expires <ts>`. Empty from an older peer, which the CLI
        /// prints as `unknown`, as for `gm_connection`.
        #[serde(default)]
        reg_subscription: String,
        /// The most recent subscription failure: unix seconds and reason.
        /// `None` when there has been none, and from an older peer.
        #[serde(default)]
        reg_subscription_last_failure: Option<(u64, String)>,
        /// Which P-CSCF the line uses and which it avoids (specs/091
        /// FR-020). `None` from an older peer, printed `unknown`. Boxed: the
        /// lists would otherwise make this the one oversized variant.
        #[serde(default)]
        pcscf: Option<Box<PcscfStatusReply>>,
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
    /// Agent B → Agent A (specs/025-outbound-calling). Sent once Agent B has
    /// accepted an outbound-triggering INVITE (a PBX call or a registered
    /// phone dialling out, spec 025 US1/US3) and picked this line as an
    /// idle VoWiFi/VoLTE candidate. `destination` is verbatim from the
    /// originating request — no transformation (FR-010), same discipline as
    /// the circuit-switched path (`modules::ModuleCmd::Dial`).
    PlaceCall {
        call_id: String,
        destination: String,
    },
    /// Agent A → Agent B. Sent immediately once Agent A decides to attempt
    /// `PlaceCall` (i.e. it was not busy) — *before* touching the carrier
    /// transport at all. Lets Agent B tell "busy, try the next line" (an
    /// immediate `CallFailed`, no carrier round trip) apart from "committed,
    /// now placing the call for real", which can legitimately take as long
    /// as Agent A's own carrier-INVITE wait
    /// (`ims::agent::OUTBOUND_INVITE_TIMEOUT`). Found live
    /// (specs/025-outbound-calling T072): without this ack, Agent B had no
    /// way to distinguish the two cases and used one short timeout for both
    /// — it gave up and moved to the next line while the carrier was still
    /// ringing, and the carrier went on to answer a call nobody was
    /// listening for.
    CallAttempting { call_id: String },
    /// Agent A → Agent B. The carrier sent `180 Ringing` for an originated
    /// INVITE. Non-terminal — zero or more of these can arrive (sent at
    /// most once per call regardless of retransmission) before the real
    /// `CallPlaced`/`CallFailed`; Agent B answers the phone/PBX leg with
    /// `180` in response so the caller hears ringback instead of silence
    /// while the carrier call is still being set up (FR-012's progress
    /// table, `contracts/sip-dialout.md`). Found live
    /// (specs/025-outbound-calling review): without this, a caller heard
    /// nothing at all for up to `OUTBOUND_INVITE_TIMEOUT +
    /// OUTBOUND_RING_TIMEOUT` (75s) and then a sudden answer.
    CallRinging { call_id: String },
    /// Agent A → Agent B. The carrier's first SDP-bearing provisional
    /// response (`180`-`183`) for an originated INVITE has arrived — early
    /// media, e.g. a carrier announcement (specs/037-p-early-media).
    /// Non-terminal, sent at most once per call, independent of
    /// `CallRinging` (either, both in either order, or neither may fire for
    /// a given attempt). By the time this is sent, Agent A has already
    /// `connect()`-ed its carrier-facing RTP socket to the address the SDP
    /// named and has a veth UAS listener up and waiting — mirrors
    /// `CallPlaced`'s role, just earlier. Agent B places its veth leg and
    /// `pjsua_safe::Endpoint::pair_calls`s it to the already-accepted
    /// phone/PBX leg (the same steps `CallPlaced` triggers), then answers
    /// that leg with `183` instead of `180` so the caller hears the
    /// carrier's pre-answer audio. If `CallPlaced` later arrives for the
    /// same call, Agent B does not repeat the pairing — see
    /// `contracts/agent-outbound-protocol-delta-early-media.md`.
    CallEarlyMedia { call_id: String },
    /// Agent A → Agent B. The carrier leg is up (2xx received, ACK sent)
    /// and Agent A's veth-facing UAS listener is up and waiting — the
    /// outbound mirror of `IncomingCall`, direction reversed. No port is
    /// carried: exactly like the inbound direction, Agent B places a real
    /// `Call::make` toward Agent A's veth SIP listener (the same mechanism
    /// `bridge_call` already uses, `vowifi/mod.rs:1213`) and RTP addressing
    /// is negotiated through that real SIP/SDP exchange, not this message.
    /// Agent B conference-bridges the resulting veth call to its
    /// already-accepted phone/PBX leg via `pjsua_safe::Endpoint::pair_calls`.
    CallPlaced { call_id: String },
    /// Agent A → Agent B. The carrier declined, was unreachable, or the
    /// line could not otherwise place the call. Agent B answers the
    /// phone/PBX leg accordingly (`486`/`503`,
    /// `specs/025-outbound-calling/contracts/sip-dialout.md`) and does not
    /// attempt to bridge.
    CallFailed { call_id: String, reason: String },
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
            | ControlMessage::HangupAck { call_id, .. }
            | ControlMessage::PlaceCall { call_id, .. }
            | ControlMessage::CallAttempting { call_id, .. }
            | ControlMessage::CallRinging { call_id, .. }
            | ControlMessage::CallEarlyMedia { call_id, .. }
            | ControlMessage::ForwardMedia { call_id, .. }
            | ControlMessage::ForwardAck { call_id, .. }
            | ControlMessage::ForwardCancel { call_id, .. }
            | ControlMessage::CallPlaced { call_id, .. }
            | ControlMessage::CallFailed { call_id, .. } => Some(call_id),
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
    /// Audio codec negotiated on the PBX/phone leg (e.g. `"AMR-WB"`), once
    /// that leg's media went active; `None` for calls that never got there.
    /// `serde(default)` keeps an older agent's reply decodable (spec 085).
    #[serde(default)]
    pub phone_codec: Option<String>,
    /// How the call's audio crossed the bridge: `forwarded:amr-wb:oa/be` or
    /// `transcoded:<reason>` (specs/120 `contracts/logs.md`). `None` for calls
    /// that never reached the decision, and from an older agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_path: Option<String>,
}

/// `bridge_failed` reasons — kept as `&'static str` constants (rather than a
/// separate enum) since the field is a free-form diagnostic string on the
/// wire, per `contracts/agent-control-protocol.md`.
pub mod reason {
    pub const PBX_UNREACHABLE: &str = "pbx_unreachable";
    /// The telephone side declined the call — it answered `486`, `600` or
    /// `603`. The only reason Agent A turns into `486 Busy Here`.
    pub const PBX_REJECTED: &str = "pbx_rejected";
    /// Nobody picked up the PBX extension: our ring timeout expired, or the
    /// telephone side gave up on its own no-answer timer (`480`/`408`).
    pub const PBX_NO_ANSWER: &str = "pbx_no_answer";
    /// The PBX leg ended before answer for any other reason — an unknown
    /// extension, a PBX error, a transport failure, or no status at all.
    /// Never reported to the carrier as busy (specs/084).
    pub const PBX_FAILED: &str = "pbx_failed";
    /// The caller gave up (`CANCEL`) while the PBX extension was still ringing.
    pub const CALLER_CANCELLED: &str = "caller_cancelled";
    pub const VETH_LEG_FAILED: &str = "veth_leg_failed";
    pub const CALLER_HANGUP: &str = "caller_hangup";
    /// The PBX/SIP side hung up first. Agent A turns this into a `BYE` toward
    /// the carrier — either side dropping must end the whole bridged call.
    pub const PBX_HANGUP: &str = "pbx_hangup";
    pub const TRANSPORT_ERROR: &str = "transport_error";
    /// The network attachment underneath the call was genuinely lost mid-call
    /// — distinct from the caller hanging up, because the two demand opposite
    /// responses (FR-011). LTE-only: the Wi-Fi path has no such attachment.
    pub const ATTACHMENT_LOST: &str = "attachment_lost";
    /// `CallFailed` (specs/025-outbound-calling): the carrier rejected,
    /// declined, or was unreachable for an outbound origination attempt.
    pub const CARRIER_REJECTED: &str = "carrier_rejected";
    /// `CallFailed`: no final response before giving up.
    pub const CARRIER_TIMEOUT: &str = "carrier_timeout";
    /// RFC 4028 session-timer refresh failed, or was never received from
    /// the carrier in time (specs/049) — distinct from
    /// [`ATTACHMENT_LOST`]/[`TRANSPORT_ERROR`] so it is diagnosable after
    /// the fact rather than looking like an ordinary hangup.
    pub const SESSION_TIMER_EXPIRED: &str = "session_timer_expired";
    /// A reg-event NOTIFY reported our registration `rejected`: the network
    /// has released every session bound to it (TS 24.229 §5.4.1.5), so a
    /// call in progress or being placed is ended (specs/089 FR-009).
    pub const REGISTRATION_REJECTED: &str = "registration_rejected";
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
            caller: "+919000000000".to_string(),
            caller_name: Some("Firstname Lastname".to_string()),
            carrier_amr: None,
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    fn wb_params() -> AmrLegParams {
        use crate::amr_forward::SdpLeg;
        use crate::ims::amr_rtp::AmrKind;
        AmrLegParams::from_sdp(&SdpLeg {
            flavour: AmrKind::Wb,
            fmtp: "octet-align=1; mode-set=0,1,2,3",
            payload_type: 104,
            dtmf_payload_type: Some(105),
            ptime_ms: Some(20),
            maxptime_ms: None,
        })
        .unwrap()
    }

    #[test]
    fn incoming_call_carries_the_carrier_amr_params() {
        let msg = ControlMessage::IncomingCall {
            call_id: "a1b2c3".to_string(),
            caller: "+919000000000".to_string(),
            caller_name: None,
            carrier_amr: Some(wb_params()),
        };
        assert_eq!(roundtrip(&msg), msg);
        let json = serde_json::to_string(&msg).unwrap();
        assert!(
            json.contains(r#""flavour":"wb""#) && json.contains(r#""payload_type":104"#),
            "{json}"
        );
    }

    #[test]
    fn an_old_incoming_call_without_carrier_amr_parses_to_none() {
        let older = r#"{"event":"incoming_call","call_id":"a1","caller":"+919000000000","caller_name":null}"#;
        match serde_json::from_str::<ControlMessage>(older).unwrap() {
            ControlMessage::IncomingCall { carrier_amr, .. } => assert_eq!(carrier_amr, None),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn the_forward_messages_roundtrip_and_name_their_call() {
        let msgs = [
            ControlMessage::ForwardMedia {
                call_id: "a1".into(),
                rtp_port: 40010,
                client: wb_params(),
            },
            ControlMessage::ForwardAck {
                call_id: "a1".into(),
                ok: true,
                reason: None,
            },
            ControlMessage::ForwardAck {
                call_id: "a1".into(),
                ok: false,
                reason: Some("mode_set".into()),
            },
            ControlMessage::ForwardCancel {
                call_id: "a1".into(),
            },
        ];
        for msg in msgs {
            assert_eq!(roundtrip(&msg), msg);
            assert_eq!(msg.call_id(), Some("a1"));
        }
        let json = serde_json::to_string(&ControlMessage::ForwardCancel {
            call_id: "a1".into(),
        })
        .unwrap();
        assert_eq!(json, r#"{"event":"forward_cancel","call_id":"a1"}"#);
    }

    #[test]
    fn a_forward_ack_without_a_reason_parses() {
        let ack = r#"{"event":"forward_ack","call_id":"a1","ok":true}"#;
        assert_eq!(
            serde_json::from_str::<ControlMessage>(ack).unwrap(),
            ControlMessage::ForwardAck {
                call_id: "a1".into(),
                ok: true,
                reason: None
            }
        );
    }

    #[test]
    fn a_call_record_without_media_path_parses_and_one_with_it_roundtrips() {
        let older = r#"{"call_id":"a1","caller":"+919000000000","outcome":"answered","started_at":1,"ended_at":null}"#;
        let rec: CallRecord = serde_json::from_str(older).unwrap();
        assert_eq!(rec.media_path, None);
        let rec = CallRecord {
            media_path: Some("forwarded:amr-wb:oa/be".into()),
            ..rec
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert_eq!(serde_json::from_str::<CallRecord>(&json).unwrap(), rec);
    }

    #[test]
    fn incoming_call_with_no_name_from_an_older_peer_parses() {
        // A message serialised without `caller_name` (an older Agent A)
        // must still deserialise, defaulting the field to `None`.
        let older = r#"{"event":"incoming_call","call_id":"a1b2c3","caller":"+919000000000"}"#;
        let parsed: ControlMessage = serde_json::from_str(older).unwrap();
        match parsed {
            ControlMessage::IncomingCall { caller_name, .. } => {
                assert_eq!(caller_name, None);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
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

    #[test]
    fn place_call_roundtrips() {
        let msg = ControlMessage::PlaceCall {
            call_id: "out1".to_string(),
            destination: "+919000000000".to_string(),
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn call_attempting_roundtrips() {
        let msg = ControlMessage::CallAttempting {
            call_id: "out1".to_string(),
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn call_ringing_roundtrips() {
        let msg = ControlMessage::CallRinging {
            call_id: "out1".to_string(),
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn call_early_media_roundtrips() {
        let msg = ControlMessage::CallEarlyMedia {
            call_id: "out1".to_string(),
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn call_placed_roundtrips() {
        let msg = ControlMessage::CallPlaced {
            call_id: "out1".to_string(),
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn call_failed_roundtrips() {
        let msg = ControlMessage::CallFailed {
            call_id: "out1".to_string(),
            reason: reason::CARRIER_REJECTED.to_string(),
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn outbound_triad_call_ids_are_reachable_via_call_id() {
        assert_eq!(
            ControlMessage::PlaceCall {
                call_id: "x".to_string(),
                destination: "1".to_string()
            }
            .call_id(),
            Some("x")
        );
        assert_eq!(
            ControlMessage::CallAttempting {
                call_id: "x".to_string(),
            }
            .call_id(),
            Some("x")
        );
        assert_eq!(
            ControlMessage::CallRinging {
                call_id: "x".to_string(),
            }
            .call_id(),
            Some("x")
        );
        assert_eq!(
            ControlMessage::CallEarlyMedia {
                call_id: "x".to_string(),
            }
            .call_id(),
            Some("x")
        );
        assert_eq!(
            ControlMessage::CallPlaced {
                call_id: "x".to_string(),
            }
            .call_id(),
            Some("x")
        );
        assert_eq!(
            ControlMessage::CallFailed {
                call_id: "x".to_string(),
                reason: "r".to_string()
            }
            .call_id(),
            Some("x")
        );
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
            caller: "+919000000000".to_string(),
            caller_name: Some("Firstname Lastname".to_string()),
            carrier_amr: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            json,
            r#"{"event":"incoming_call","call_id":"a1b2c3","caller":"+919000000000","caller_name":"Firstname Lastname"}"#
        );
    }

    #[test]
    fn call_id_accessor_returns_correct_value_for_every_variant() {
        assert_eq!(
            ControlMessage::IncomingCall {
                call_id: "x".to_string(),
                caller: "y".to_string(),
                caller_name: None,
                carrier_amr: None,
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
                caller_name: None,
                carrier_amr: None,
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
            sender: "+919000000000".to_string(),
            body: "hello over VoWiFi".to_string(),
            received_at: "2026-07-13T00:00:00+00:00".to_string(),
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn sms_received_has_no_call_id() {
        assert_eq!(
            ControlMessage::SmsReceived {
                sender: "+919000000000".to_string(),
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
            can_answer: true,
            blocked_reason: None,
            gm_connection: "up".to_string(),
            rejection_cooldown_until: None,
            reg_subscription: "confirmed, expires 2026-10-14T11:02:11+00:00".to_string(),
            reg_subscription_last_failure: Some((
                1_699_999_500,
                "accepted but no NOTIFY within 128s".to_string(),
            )),
            pcscf: Some(Box::new(PcscfStatusReply {
                active: "10.0.1.10".to_string(),
                pool: vec!["10.0.1.10".to_string(), "10.0.0.10".to_string()],
                unavailable: vec![("10.0.0.10".to_string(), 1_700_000_330)],
            })),
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
            can_answer: false,
            blocked_reason: Some("not registered".to_string()),
            gm_connection: "reconnecting since 2026-08-07T10:14:03+00:00 (attempt 2)".to_string(),
            rejection_cooldown_until: Some(1_700_001_800),
            reg_subscription: "none".to_string(),
            reg_subscription_last_failure: None,
            pcscf: None,
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn registration_status_reply_from_an_older_peer_omitting_gm_connection_parses() {
        // A reply serialised without `gm_connection` (an older Agent A) must
        // still deserialise, defaulting the field to empty — the CLI then
        // prints "unknown" rather than claiming the connection is up (specs/028).
        let older = r#"{"event":"registration_status_reply","state":"Registered","registered_at":1700000000,"expires_at":1700003600,"last_failure":null,"can_answer":true,"blocked_reason":null}"#;
        let parsed: ControlMessage = serde_json::from_str(older).unwrap();
        match parsed {
            ControlMessage::RegistrationStatusReply {
                gm_connection,
                rejection_cooldown_until,
                reg_subscription,
                reg_subscription_last_failure,
                pcscf,
                ..
            } => {
                // specs/091: nor of the P-CSCF pool.
                assert_eq!(pcscf, None);
                assert_eq!(gm_connection, "");
                // specs/089: nor does it know of the rejection cooldown.
                assert_eq!(rejection_cooldown_until, None);
                // specs/090: nor of the reg-event subscription.
                assert_eq!(reg_subscription, "");
                assert_eq!(reg_subscription_last_failure, None);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn call_history_reply_roundtrips() {
        let msg = ControlMessage::CallHistoryReply {
            calls: vec![
                CallRecord {
                    call_id: "1".to_string(),
                    caller: "+919000000000".to_string(),
                    outcome: "answered".to_string(),
                    started_at: 1_700_000_000,
                    ended_at: Some(1_700_000_300),
                    phone_codec: Some("AMR-WB".to_string()),
                    media_path: None,
                },
                CallRecord {
                    call_id: "2".to_string(),
                    caller: "+919000000000".to_string(),
                    outcome: "declined:busy".to_string(),
                    started_at: 1_700_000_500,
                    ended_at: Some(1_700_000_500),
                    phone_codec: None,
                    media_path: None,
                },
            ],
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn call_record_without_phone_codec_still_deserializes() {
        // What an agent from before spec 085 sends.
        let old = r#"{"call_id":"1","caller":"+919000000000","outcome":"answered","started_at":1,"ended_at":2}"#;
        let rec: CallRecord = serde_json::from_str(old).unwrap();
        assert_eq!(rec.phone_codec, None);
    }
}
