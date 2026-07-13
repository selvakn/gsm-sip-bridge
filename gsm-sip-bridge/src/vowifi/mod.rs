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
use pjsua_safe::{
    Account, AccountConfig, Call, CallState, Endpoint, EndpointConfig, TransportType,
};
use std::collections::VecDeque;
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
/// Agent B's own local SIP port for its PJSIP endpoint — deliberately NOT
/// `[sip].local_port`. Both the circuit-switched daemon and Agent B share
/// one config file and, in the merged deployment (`docker/entrypoint.sh`),
/// one network namespace (host networking) — reusing `[sip].local_port` for
/// both means two independent `pjsua_create`/transport-bind calls racing for
/// the same UDP port, which fails outright for whichever one starts second.
/// Same "private implementation detail" status as `VETH_SIP_PORT`/
/// `AGENT_A_STATUS_PORT`.
pub const AGENT_B_SIP_LOCAL_PORT: u16 = 5072;

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
        local_port: AGENT_B_SIP_LOCAL_PORT,
        tls_verify: config.sip.tls_verify == TlsVerify::Strict,
        // Everything crossing PJMEDIA's conference bridge is resampled to this
        // rate, so it is the ceiling on what the PBX leg can carry: at 8000, a
        // carrier's 16 kHz AMR-WB would be squeezed through 8 kHz here even if
        // the PBX had happily agreed to G.722.
        clock_rate: if config.vowifi.wideband { 16000 } else { 8000 },
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
    if config.vowifi.wideband {
        prioritize_wideband_codecs(&endpoint);
    }

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

/// PJSIP's G.722 codec id. Wideband (16 kHz internally, whatever RFC 3551's
/// historical `G722/8000` rtpmap says), built into pjproject with no external
/// library, and understood by every mainstream PBX without an extra module —
/// which is why it, rather than Opus, is what the PBX leg reaches for.
const G722_CODEC_ID: &str = "G722/16000/1";
/// PJSIP's 16 kHz linear-PCM codec id — uncompressed, and used only on the
/// veth link to Agent A (see `ims::sdp::NegotiatedCodec::L16`).
const L16_16K_CODEC_ID: &str = "L16/16000/1";

/// Make Agent B's two calls offer the codecs a wideband bridge needs: G.722
/// first (what the PBX should pick), and L16/16000 enabled so it appears in the
/// offer at all (what Agent A picks on the veth link, by name — so its low
/// priority here doesn't matter; it only keeps L16 out of the PBX's way).
///
/// Priorities are endpoint-global, and best-effort: a PJSIP build missing
/// either codec just logs a warning and carries on. Nothing here can fail a
/// call — without G.722 the PBX leg falls back to PCMU, and without L16 the
/// veth link does, which is exactly how this bridge behaved before wideband.
fn prioritize_wideband_codecs(endpoint: &Endpoint) {
    for (codec_id, priority) in [(G722_CODEC_ID, 200), (L16_16K_CODEC_ID, 1)] {
        if let Err(e) = endpoint.set_codec_priority(codec_id, priority) {
            tracing::warn!(
                codec = codec_id,
                error = %e,
                "could not set codec priority; this PJSIP build may not have the codec"
            );
        }
    }
    tracing::info!(
        codecs = ?endpoint
            .codecs()
            .iter()
            .filter(|c| c.priority > 0)
            .map(|c| (c.id.clone(), c.priority))
            .collect::<Vec<_>>(),
        "PJSIP codecs offered, in priority order"
    );
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

            // Read Agent A's messages on a thread from here on. While the PBX
            // rings we must still notice a `CallEnded` (the caller gave up) —
            // blocking on the PBX's state alone would leave the extension
            // ringing for the whole timeout after the caller had already hung
            // up.
            let ctrl_rx = spawn_control_reader(reader);

            // The PBX extension is only *ringing* at this point. Agent A holds
            // the carrier in the ringing state (so the network keeps playing
            // ringback to the caller) until we tell it a human actually picked
            // up — answering the carrier the moment the INVITE went out would
            // replace the caller's ringback with dead air.
            match wait_for_pbx_answer(&pbx_call, &ctrl_rx) {
                PbxOutcome::Answered => {
                    tracing::info!(call_id = %call_id, "PBX extension answered");
                    write_msg(
                        &mut writer,
                        &ControlMessage::CallAnswered {
                            call_id: call_id.clone(),
                        },
                    )
                    .map_err(BridgeError::Ims)?;
                }
                outcome => {
                    let reason = outcome.reason();
                    tracing::info!(call_id = %call_id, reason, "PBX leg never answered; declining");
                    let _ = write_msg(
                        &mut writer,
                        &ControlMessage::BridgeFailed {
                            call_id: call_id.clone(),
                            reason: reason.to_string(),
                        },
                    );
                    endpoint.unpair_call(pbx_call.call_id());
                    let _ = pbx_call.hangup();
                    let _ = veth_call.hangup();
                    recent_calls
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(CallRecord {
                            call_id,
                            caller,
                            outcome: format!("declined:{reason}"),
                            started_at,
                            ended_at: Some(now_unix()),
                        });
                    return Ok(());
                }
            }

            // A hangup can start on either side. Blocking on Agent A alone
            // would miss the PBX extension hanging up first, leaving the caller
            // on a line that is already dead — so watch our own leg too, and
            // tell Agent A when it drops so it can BYE the carrier.
            let end_reason = loop {
                match ctrl_rx.recv_timeout(PBX_RING_POLL_INTERVAL) {
                    Ok(ControlMessage::CallEnded { reason, .. }) => {
                        tracing::info!(call_id = %call_id, reason = %reason, "call ended, tearing down both legs");
                        break reason;
                    }
                    Ok(other) => {
                        tracing::warn!(call_id = %call_id, message = ?other, "unexpected message during an active call");
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        tracing::warn!(call_id = %call_id, "control connection lost mid-call; tearing down anyway");
                        break "control_connection_lost".to_string();
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                if pbx_call.poll_state() == CallState::Disconnected {
                    tracing::info!(call_id = %call_id, "PBX side hung up; telling Agent A to end the carrier leg");
                    let _ = write_msg(
                        &mut writer,
                        &ControlMessage::CallEnded {
                            call_id: call_id.clone(),
                            reason: control::reason::PBX_HANGUP.to_string(),
                        },
                    );
                    break control::reason::PBX_HANGUP.to_string();
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

/// How long to let the PBX extension ring before giving up. The caller hears
/// ringback for this whole window, so it wants to be a natural ring duration —
/// long enough for someone to walk to the phone, short enough that the carrier
/// doesn't time the call out from its own end first.
const PBX_RING_TIMEOUT: Duration = Duration::from_secs(45);
/// How often to re-check the PBX leg's state while it rings. PJSIP's state is
/// polled rather than pushed (see `Call::poll_state`); 100ms is imperceptible
/// against a human picking up a phone and costs nothing.
const PBX_RING_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// What became of the PBX leg while the caller listened to ringback.
enum PbxOutcome {
    /// A human picked up.
    Answered,
    /// The PBX hung up on us — busy, rejected, or the extension is gone.
    Rejected,
    /// It just rang out.
    NoAnswer,
    /// The caller gave up before anyone picked up. Agent A has already told
    /// the carrier; we only need to stop ringing the extension.
    CallerGone,
}

impl PbxOutcome {
    fn reason(&self) -> &'static str {
        match self {
            // Only ever called on the paths that didn't answer.
            PbxOutcome::Answered => "answered",
            PbxOutcome::Rejected => control::reason::PBX_REJECTED,
            PbxOutcome::NoAnswer => control::reason::PBX_NO_ANSWER,
            PbxOutcome::CallerGone => control::reason::CALLER_CANCELLED,
        }
    }
}

/// Ring the PBX extension until someone answers, the PBX gives up on us, the
/// caller hangs up, or `PBX_RING_TIMEOUT` elapses.
fn wait_for_pbx_answer(pbx_call: &Call, ctrl_rx: &mpsc::Receiver<ControlMessage>) -> PbxOutcome {
    let deadline = Instant::now() + PBX_RING_TIMEOUT;
    while Instant::now() < deadline {
        match pbx_call.poll_state() {
            CallState::Confirmed => return PbxOutcome::Answered,
            CallState::Disconnected => return PbxOutcome::Rejected,
            // Calling/Early — still ringing.
            _ => {}
        }
        // The caller may hang up mid-ring; stop ringing the extension at once
        // rather than making it ring on for the rest of the timeout.
        match ctrl_rx.recv_timeout(PBX_RING_POLL_INTERVAL) {
            Ok(ControlMessage::CallEnded { .. }) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return PbxOutcome::CallerGone
            }
            Ok(other) => {
                tracing::debug!(message = ?other, "ignoring control message while the PBX rings")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    PbxOutcome::NoAnswer
}

/// Reads Agent A's control messages on a thread so the ring loop can wait on
/// the PBX's state and the control channel at the same time. Mirrors
/// `ims::agent::spawn_control_reader`.
fn spawn_control_reader(
    mut reader: std::io::BufReader<TcpStream>,
) -> mpsc::Receiver<ControlMessage> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || loop {
        match read_msg(&mut reader) {
            Ok(msg) => {
                if tx.send(msg).is_err() {
                    return;
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "Agent A control connection reader stopped");
                return;
            }
        }
    });
    rx
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
