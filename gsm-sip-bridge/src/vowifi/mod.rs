//! Agent B: the SIP/PBX-facing half of the inbound VoWiFi bridge (see
//! `specs/011-vowifi-sip-bridge/`). Runs in the container's default network
//! namespace (LAN-reachable to the PBX), receives `IncomingCall` events from
//! Agent A (`crate::ims::agent`, running in the tunnel's `ims` netns) over
//! the control channel defined in `control`, and bridges each call by
//! placing two PJSIP calls — one to the configured PBX destination, one back
//! to Agent A across the veth link — and conference-connecting them
//! (`pjsua_safe::Endpoint::pair_calls`, `specs/011-vowifi-sip-bridge`
//! Foundational T010).
//!
//! Deliberately builds its own `Endpoint`/`Account` here rather than reusing
//! `crate::sip::SipBridge`: `SipBridge` holds a single `active_call:
//! Option<Call>` (correct for the circuit-switched bridge, which only ever
//! has one call at a time) and has no accessor for its private `Endpoint` —
//! this feature needs to hold *two* concurrent `Call`s and pair them, which
//! doesn't fit that shape. Building a second `Endpoint`/`Account` here is a
//! few duplicated lines, not a new abstraction, and leaves `SipBridge`/the
//! existing circuit-switched call path completely untouched (FR-006).

pub mod control;

use crate::config::{AppConfig, SipTransport as ConfigSipTransport, TlsVerify, VowifiConfig};
use crate::error::{BridgeError, BridgeResult};
use control::{read_msg, write_msg, CallRecord, ControlMessage};
use pjsua_safe::{Account, AccountConfig, Call, Endpoint, EndpointConfig, TransportType};
use std::collections::VecDeque;
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Fixed port Agent A listens on for Agent B's inbound (veth-internal) SIP
/// call. Not user-configurable — this is a private implementation detail of
/// the link between the two agents, not something an operator ever points
/// at directly (unlike `[vowifi].control_port`, which is documented config
/// because it's part of the deployment's env/compose wiring).
pub const VETH_SIP_PORT: u16 = 5070;
/// Fixed port Agent A listens on for `vowifi-status` registration-health
/// queries (`ControlMessage::StatusQuery` → `RegistrationStatusReply`).
/// Same "private implementation detail" status as `VETH_SIP_PORT`.
pub const AGENT_A_STATUS_PORT: u16 = 5071;

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Bounded history of recent call outcomes (FR-008, User Story 3) — oldest
/// evicted once full so memory stays flat over an arbitrarily long uptime.
/// `capacity` is fixed at construction; not user-configurable, since this
/// is an operational diagnostic aid, not a feature an operator tunes.
pub struct RecentCalls {
    capacity: usize,
    records: VecDeque<CallRecord>,
}

impl RecentCalls {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            records: VecDeque::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, record: CallRecord) {
        if self.records.len() >= self.capacity {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }

    /// Newest first — the order an operator checking status wants to see.
    pub fn snapshot(&self) -> Vec<CallRecord> {
        self.records.iter().rev().cloned().collect()
    }
}

/// How many recent call outcomes to remember for `vowifi-status`.
const RECENT_CALLS_CAPACITY: usize = 20;

pub fn run(config: &AppConfig) -> ExitCode {
    match run_inner(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner(config: &AppConfig) -> BridgeResult<()> {
    let transport = match config.sip.transport {
        ConfigSipTransport::Udp => TransportType::Udp,
        ConfigSipTransport::Tcp => TransportType::Tcp,
        ConfigSipTransport::Tls => TransportType::Tls,
    };
    let ep_config = EndpointConfig {
        transport,
        local_port: config.sip.local_port,
        tls_verify: config.sip.tls_verify == TlsVerify::Strict,
        jb_init_ms: config.audio.settings.jb_init_ms,
        jb_min_pre: config.audio.settings.jb_min_pre,
        jb_max_ms: config.audio.settings.jb_max_ms,
        vad_enabled: config.audio.vad,
        // No physical sound device in this process (null snd dev, below) —
        // tx_level only matters for the slot-0 sound-device path.
        tx_level: 1.0,
        snd_rec_latency_ms: config.audio.snd_rec_latency_ms,
        snd_play_latency_ms: config.audio.snd_play_latency_ms,
    };
    let endpoint = Endpoint::create(ep_config)
        .map_err(|e| BridgeError::Ims(format!("PJSIP endpoint creation failed: {e}")))?;
    endpoint
        .set_null_sound_device()
        .map_err(|e| BridgeError::Ims(format!("null sound device setup failed: {e}")))?;

    let acc_config = AccountConfig {
        sip_server: config.sip.server.clone(),
        sip_port: config.sip.port,
        username: config.sip.username.clone(),
        password: config.sip.password.expose_secret().clone(),
        display_name: config.sip.display_name.clone(),
    };
    let account = Account::register(&endpoint, acc_config, None)
        .map_err(|e| BridgeError::Ims(format!("SIP account registration failed: {e}")))?;
    tracing::info!(
        server = %config.sip.server,
        port = config.sip.port,
        "vowifi-sip-agent registered to PBX"
    );

    let listen_addr = (
        config.vowifi.veth_peer_addr.as_str(),
        config.vowifi.control_port,
    );
    let listener = TcpListener::bind(listen_addr)
        .map_err(|e| BridgeError::Ims(format!("control channel listen failed: {e}")))?;
    tracing::info!(
        addr = %listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default(),
        "vowifi-sip-agent listening for Agent A"
    );

    let recent_calls = Arc::new(Mutex::new(RecentCalls::new(RECENT_CALLS_CAPACITY)));

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "control channel accept failed");
                continue;
            }
        };
        if let Err(e) = handle_connection(stream, &endpoint, &account, config, &recent_calls) {
            tracing::warn!(error = %e, "error handling Agent A control connection");
        }
    }
    Ok(())
}

fn handle_connection(
    stream: TcpStream,
    endpoint: &Endpoint,
    account: &Account,
    config: &AppConfig,
    recent_calls: &Arc<Mutex<RecentCalls>>,
) -> BridgeResult<()> {
    let mut reader = std::io::BufReader::new(
        stream
            .try_clone()
            .map_err(|e| BridgeError::Ims(format!("failed to clone control connection: {e}")))?,
    );
    let mut writer = stream;

    let msg = read_msg(&mut reader).map_err(BridgeError::Ims)?;
    let (call_id, caller) = match msg {
        ControlMessage::IncomingCall { call_id, caller } => (call_id, caller),
        ControlMessage::StatusQuery => {
            let calls = recent_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .snapshot();
            write_msg(&mut writer, &ControlMessage::CallHistoryReply { calls })
                .map_err(BridgeError::Ims)?;
            return Ok(());
        }
        other => {
            return Err(BridgeError::Ims(format!(
                "expected IncomingCall or StatusQuery as the first message on a control connection, got {other:?}"
            )));
        }
    };
    tracing::info!(call_id = %call_id, caller = %caller, "incoming VoWiFi call signaled by Agent A");
    let started_at = now_unix();

    match bridge_call(endpoint, account, config, &caller) {
        Ok((mut pbx_call, mut veth_call)) => {
            write_msg(
                &mut writer,
                &ControlMessage::BridgeReady {
                    call_id: call_id.clone(),
                    // Informational only — the real RTP port exchange
                    // happens over the veth-internal SDP dialog
                    // (`ims::agent`'s UAS), not this control channel.
                    veth_rtp_port: 0,
                },
            )
            .map_err(BridgeError::Ims)?;

            // Block until Agent A reports the call ended, then tear down
            // both legs. One call at a time per the spec's single-line
            // assumption, so a blocking read here is correct.
            let end_reason = match read_msg(&mut reader) {
                Ok(ControlMessage::CallEnded { reason, .. }) => {
                    tracing::info!(call_id = %call_id, reason = %reason, "call ended, tearing down both legs");
                    reason
                }
                Ok(other) => {
                    tracing::warn!(call_id = %call_id, message = ?other, "unexpected message while waiting for CallEnded");
                    "unexpected_message".to_string()
                }
                Err(e) => {
                    tracing::warn!(call_id = %call_id, error = %e, "control connection lost before CallEnded; tearing down anyway");
                    "control_connection_lost".to_string()
                }
            };
            endpoint.unpair_call(pbx_call.call_id());
            let _ = pbx_call.hangup();
            let _ = veth_call.hangup();
            let _ = write_msg(
                &mut writer,
                &ControlMessage::HangupAck {
                    call_id: call_id.clone(),
                },
            );
            recent_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(CallRecord {
                    call_id,
                    caller,
                    outcome: format!("answered:{end_reason}"),
                    started_at,
                    ended_at: Some(now_unix()),
                });
            Ok(())
        }
        Err(e) => {
            tracing::warn!(call_id = %call_id, error = %e, "failed to bridge call");
            write_msg(
                &mut writer,
                &ControlMessage::BridgeFailed {
                    call_id: call_id.clone(),
                    reason: control::reason::PBX_UNREACHABLE.to_string(),
                },
            )
            .map_err(BridgeError::Ims)?;
            recent_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(CallRecord {
                    call_id,
                    caller,
                    outcome: format!("failed:{e}"),
                    started_at,
                    ended_at: Some(now_unix()),
                });
            Ok(())
        }
    }
}

/// Places both legs — the PBX-side call (reusing the same destination-URI
/// and caller-ID header logic as the circuit-switched bridge,
/// `crate::sip::SipBridge::compute_destination_uri`/`make_call`, FR-003/
/// FR-011) and the veth-side call back to Agent A's UAS
/// (`crate::ims::agent`, listening on `VETH_SIP_PORT`) — and pairs them via
/// `Endpoint::pair_calls` so their media bridges together once both reach
/// `PJSUA_CALL_MEDIA_ACTIVE` (see `pjsua-safe/src/endpoint.rs`'s
/// `on_call_media_state_cb`).
fn bridge_call(
    endpoint: &Endpoint,
    account: &Account,
    config: &AppConfig,
    caller: &str,
) -> BridgeResult<(Call, Call)> {
    let mut headers: Vec<(&str, &str)> = Vec::new();
    let pai_value;
    if !caller.is_empty() {
        pai_value = format!("\"{caller}\" <tel:{caller}>");
        headers.push(("P-Asserted-Identity", &pai_value));
        headers.push(("X-GSM-Caller-ID", caller));
    }

    let pbx_uri = pbx_dest_uri(config, caller);
    let pbx_call = Call::make(account, &pbx_uri, None, &headers)
        .map_err(|e| BridgeError::Ims(format!("PBX-side call failed: {e}")))?;

    let veth_uri = format!(
        "sip:agent-a@{}:{}",
        config.vowifi.veth_local_addr, VETH_SIP_PORT
    );
    let veth_call = Call::make(account, &veth_uri, None, &[])
        .map_err(|e| BridgeError::Ims(format!("veth-side call failed: {e}")))?;

    endpoint.pair_calls(pbx_call.call_id(), veth_call.call_id());
    tracing::info!(
        pbx_call_id = pbx_call.call_id(),
        veth_call_id = veth_call.call_id(),
        dest = %pbx_uri,
        "placed and paired both legs"
    );

    Ok((pbx_call, veth_call))
}

/// Mirrors `crate::sip::SipBridge::compute_destination_uri`: empty
/// `[bridge].sip_destination` means DID passthrough (dial the caller's own
/// number at the PBX), otherwise dial the configured fixed extension.
fn pbx_dest_uri(config: &AppConfig, caller_did: &str) -> String {
    let raw_dest = if config.bridge.sip_destination.is_empty() {
        caller_did
    } else {
        &config.bridge.sip_destination
    };
    let dest = raw_dest.trim_start_matches('+');
    format!("sip:{dest}@{}:{}", config.sip.server, config.sip.port)
}

/// Entry point for the `vowifi-status` subcommand: queries Agent A's
/// registration health (`AGENT_A_STATUS_PORT`) and Agent B's recent call
/// history (`[vowifi].control_port`) and prints both — FR-008/User Story 3.
/// Either query failing independently (e.g. one agent not running) is
/// reported, not fatal to reporting the other.
pub fn print_status(config: &VowifiConfig) -> ExitCode {
    let mut ok = true;

    println!("VoWiFi registration (Agent A):");
    match query_status(&format!("{}:{AGENT_A_STATUS_PORT}", config.veth_local_addr)) {
        Ok(ControlMessage::RegistrationStatusReply {
            state,
            registered_at,
            expires_at,
            last_failure,
        }) => {
            println!("  state: {state}");
            println!("  registered_at: {}", format_unix(registered_at));
            println!("  expires_at: {}", format_unix(expires_at));
            match last_failure {
                Some((t, msg)) => println!("  last_failure: {} {msg}", format_unix(Some(t))),
                None => println!("  last_failure: none"),
            }
        }
        Ok(other) => {
            println!("  unexpected reply: {other:?}");
            ok = false;
        }
        Err(e) => {
            println!("  unreachable: {e}");
            ok = false;
        }
    }

    println!("Recent calls (Agent B):");
    match query_status(&format!(
        "{}:{}",
        config.veth_peer_addr, config.control_port
    )) {
        Ok(ControlMessage::CallHistoryReply { calls }) if calls.is_empty() => {
            println!("  (none)");
        }
        Ok(ControlMessage::CallHistoryReply { calls }) => {
            for c in calls {
                println!(
                    "  {} caller={} outcome={} started={} ended={}",
                    c.call_id,
                    c.caller,
                    c.outcome,
                    format_unix(Some(c.started_at)),
                    format_unix(c.ended_at)
                );
            }
        }
        Ok(other) => {
            println!("  unexpected reply: {other:?}");
            ok = false;
        }
        Err(e) => {
            println!("  unreachable: {e}");
            ok = false;
        }
    }

    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn format_unix(t: Option<u64>) -> String {
    t.map(|t| t.to_string()).unwrap_or_else(|| "-".to_string())
}

/// Connects to `addr` (`host:port`), sends `StatusQuery`, and returns
/// whatever single reply comes back. Used against both Agent A's status
/// port and Agent B's control port — each answers with the reply variant
/// it actually has data for (`RegistrationStatusReply` /
/// `CallHistoryReply` respectively).
fn query_status(addr: &str) -> BridgeResult<ControlMessage> {
    let socket_addr: std::net::SocketAddr = addr
        .parse()
        .map_err(|e| BridgeError::Ims(format!("invalid address {addr}: {e}")))?;
    let mut stream = TcpStream::connect_timeout(&socket_addr, std::time::Duration::from_secs(3))
        .map_err(|e| BridgeError::Ims(format!("connect to {addr} failed: {e}")))?;
    write_msg(&mut stream, &ControlMessage::StatusQuery).map_err(BridgeError::Ims)?;
    let mut reader = std::io::BufReader::new(stream);
    read_msg(&mut reader).map_err(BridgeError::Ims)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str) -> CallRecord {
        CallRecord {
            call_id: id.to_string(),
            caller: "+919789063708".to_string(),
            outcome: "answered:caller_hangup".to_string(),
            started_at: 1_700_000_000,
            ended_at: Some(1_700_000_300),
        }
    }

    #[test]
    fn recent_calls_evicts_oldest_once_over_capacity() {
        let mut recent = RecentCalls::new(3);
        recent.push(record("1"));
        recent.push(record("2"));
        recent.push(record("3"));
        recent.push(record("4"));
        let snapshot = recent.snapshot();
        assert_eq!(snapshot.len(), 3);
        // Newest first; "1" was evicted.
        assert_eq!(snapshot[0].call_id, "4");
        assert_eq!(snapshot[1].call_id, "3");
        assert_eq!(snapshot[2].call_id, "2");
    }

    #[test]
    fn recent_calls_under_capacity_keeps_everything() {
        let mut recent = RecentCalls::new(5);
        recent.push(record("1"));
        recent.push(record("2"));
        let snapshot = recent.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].call_id, "2");
        assert_eq!(snapshot[1].call_id, "1");
    }

    #[test]
    fn recent_calls_empty_snapshot_when_nothing_pushed() {
        let recent = RecentCalls::new(5);
        assert!(recent.snapshot().is_empty());
    }
}
