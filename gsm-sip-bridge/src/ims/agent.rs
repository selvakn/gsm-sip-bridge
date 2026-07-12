//! Agent A: the IMS/VoWiFi-facing half of the inbound VoWiFi bridge (see
//! `specs/011-vowifi-sip-bridge/`). Runs inside the ePDG tunnel's `ims`
//! network namespace, keeps a persistent IMS-AKA registration alive
//! (`super::register_session`, kept alive rather than torn down), answers
//! inbound `INVITE`s from the carrier, and relays RTP between the carrier
//! side and a veth link to `crate::vowifi` (Agent B).
//!
//! Agent A is a SIP UAS on *two* fronts for a single call: the carrier's
//! Gm-protected IMS transport (`session.transport`, established by IMS-AKA
//! registration) and a second, unauthenticated plain-SIP link on the veth
//! (`VETH_SIP_PORT`) that Agent B's PJSIP `Call::make` dials into once it
//! decides to bridge — see `crate::vowifi::bridge_call`. Both fronts reuse
//! the same `SipRequest`/`build_*` primitives from `super::sip_client`;
//! only the carrier-facing one needs IMS-AKA/Gm-IPsec, since the veth link
//! is a private, trusted point-to-point connection between the two agents.

use crate::config::VowifiConfig;
use crate::error::{BridgeError, BridgeResult};
use crate::ims::sdp::{self, NegotiatedCodec};
use crate::ims::sip_client::{
    build_100_trying, build_180_ringing, build_200_ok_bye, build_200_ok_invite,
    build_486_busy_here, format_sip_addr, random_hex, SipMessage, SipRequest,
};
use crate::ims::ImsRegisterConfig;
use crate::vowifi::control::{read_msg, reason, write_msg, ControlMessage};
use crate::vowifi::VETH_SIP_PORT;
use std::io::BufReader;
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime};

/// How long Agent A waits for Agent B to reply to `IncomingCall` (place its
/// two legs and either pair them or fail) before giving up and declining the
/// carrier's INVITE — must leave headroom under SC-001's 5s answer target,
/// since it sits directly in that critical path.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(4);
/// How long Agent A waits for Agent B's veth-side `INVITE` to arrive after
/// signaling `IncomingCall` — Agent B places its veth call as part of
/// reaching `BridgeReady`, so this should resolve well within
/// `CONTROL_TIMEOUT` in the success case; this is the ceiling for the
/// separate thread that's listening for it.
const VETH_INVITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the RTP relay's blocking `recv` wakes up to check whether it
/// should stop — bounds how quickly a hangup actually silences the relay.
const RELAY_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// How often the main dispatch loop wakes up (when idle) to check whether
/// the registration needs renewing — matches the existing project's
/// `[resilience].network_poll_interval_sec` default (feature 009) for
/// consistency, not a hard requirement of this feature.
const REGISTRATION_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// How far ahead of the registration's actual expiry Agent A starts trying
/// to renew it — SC-003's 90s recovery budget plus margin for the
/// renewal's own AKA-challenge round trip.
const RENEWAL_HEADROOM: Duration = Duration::from_secs(300);
const RETRY_INITIAL_BACKOFF: Duration = Duration::from_secs(5);
const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(120);

/// Entry point for the `vowifi-ims-agent` subcommand.
pub fn run(config: &VowifiConfig) -> ExitCode {
    match run_inner(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn read_pcscf(path: &str) -> BridgeResult<IpAddr> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| BridgeError::Ims(format!("failed to read P-CSCF address from {path}: {e}")))?;
    raw.trim()
        .parse()
        .map_err(|e| BridgeError::Ims(format!("invalid P-CSCF address in {path}: {e}")))
}

fn run_inner(config: &VowifiConfig) -> BridgeResult<()> {
    let pcscf_addr = read_pcscf(&config.pcscf_source_path)?;
    let reg_cfg = ImsRegisterConfig {
        modem_port: PathBuf::from(&config.modem_port),
        pcscf_addr,
        pcscf_port: 5060,
        mcc: config.mcc.clone(),
        mnc: config.mnc.clone(),
        imsi: None,
        use_tcp: config.use_tcp,
        sec_agree: config.sec_agree,
        msisdn: None,
    };

    let veth_local_ip: IpAddr = config
        .veth_local_addr
        .parse()
        .map_err(|e| BridgeError::Ims(format!("invalid vowifi.veth_local_addr: {e}")))?;
    let control_addr: SocketAddr = format!("{}:{}", config.veth_peer_addr, config.control_port)
        .parse()
        .map_err(|e| BridgeError::Ims(format!("invalid vowifi control address: {e}")))?;

    let mut session = super::register_session(&reg_cfg)?;
    if session.status != 200 {
        let status = session.status;
        let reason = session.reason.clone();
        session.cleanup();
        return Err(BridgeError::Ims(format!(
            "IMS registration failed: {status} {reason}"
        )));
    }
    tracing::info!("vowifi-ims-agent registered, listening for inbound calls");

    let status = Arc::new(Mutex::new(super::RegistrationStatus {
        state: super::RegistrationState::Registered,
        registered_at: Some(SystemTime::now()),
        expires_at: Some(SystemTime::now() + Duration::from_secs(super::DEFAULT_EXPIRES as u64)),
        last_failure: None,
    }));

    {
        let status_for_listener = status.clone();
        std::thread::spawn(move || {
            if let Err(e) = run_status_listener(veth_local_ip, status_for_listener) {
                tracing::warn!(error = %e, "registration-status listener failed");
            }
        });
    }

    let result = dispatch_loop(&mut session, &reg_cfg, &status, control_addr, veth_local_ip);
    session.cleanup();
    result
}

/// Answers `vowifi-status` queries (`ControlMessage::StatusQuery` →
/// `RegistrationStatusReply`) on `crate::vowifi::AGENT_A_STATUS_PORT` for
/// as long as the agent runs. A separate, always-listening connection from
/// the main dispatch loop's own SIP transport, so a status query never
/// competes with call signaling.
fn run_status_listener(
    veth_local_ip: IpAddr,
    status: Arc<Mutex<super::RegistrationStatus>>,
) -> BridgeResult<()> {
    let listener = std::net::TcpListener::bind((veth_local_ip, crate::vowifi::AGENT_A_STATUS_PORT))
        .map_err(|e| BridgeError::Ims(format!("status listener bind failed: {e}")))?;
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "status listener accept failed");
                continue;
            }
        };
        let mut reader = match stream.try_clone() {
            Ok(s) => BufReader::new(s),
            Err(_) => continue,
        };
        match read_msg(&mut reader) {
            Ok(ControlMessage::StatusQuery) => {
                let snapshot = status.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let reply = ControlMessage::RegistrationStatusReply {
                    state: format!("{:?}", snapshot.state),
                    registered_at: snapshot.registered_at.and_then(to_unix),
                    expires_at: snapshot.expires_at.and_then(to_unix),
                    last_failure: snapshot
                        .last_failure
                        .map(|(t, msg)| (to_unix(t).unwrap_or(0), msg)),
                };
                let _ = write_msg(&mut stream, &reply);
            }
            Ok(other) => {
                tracing::debug!(message = ?other, "unexpected message on status port, ignoring");
            }
            Err(e) => {
                tracing::debug!(error = %e, "failed to read status query");
            }
        }
    }
    Ok(())
}

fn to_unix(t: SystemTime) -> Option<u64> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Doubling backoff for registration-renewal retry, capped at `max`. Pure
/// and testable without a real timer.
fn next_backoff(current: Duration, max: Duration) -> Duration {
    current.checked_mul(2).unwrap_or(max).min(max)
}

/// Re-runs the full IMS-AKA REGISTER flow (a fresh AT+CSIM challenge, same
/// as the initial registration — there is no cheaper incremental refresh in
/// this protocol) to get a new, live `RegisteredSession`. Does not touch
/// `session`/`status` itself; the caller swaps them in only on success, so a
/// failed attempt leaves the still-valid old session in place until it
/// actually expires or a later retry succeeds.
fn attempt_renewal(reg_cfg: &ImsRegisterConfig) -> BridgeResult<super::RegisteredSession> {
    let mut new_session = super::register_session(reg_cfg)?;
    if new_session.status != 200 {
        let status = new_session.status;
        let reason = new_session.reason.clone();
        new_session.cleanup();
        return Err(BridgeError::Ims(format!(
            "renewal REGISTER rejected: {status} {reason}"
        )));
    }
    Ok(new_session)
}

/// Holds what's needed to tear a bridged call down again once a `BYE`
/// arrives — the control connection Agent B expects `CallEnded` on, and the
/// flag that stops the background RTP relay threads.
struct ActiveCall {
    control: TcpStream,
    stop: Arc<AtomicBool>,
    call_id: String,
    to_tag: String,
}

fn dispatch_loop(
    session: &mut super::RegisteredSession,
    reg_cfg: &ImsRegisterConfig,
    status: &Arc<Mutex<super::RegistrationStatus>>,
    control_addr: SocketAddr,
    veth_local_ip: IpAddr,
) -> BridgeResult<()> {
    let mut active_call: Option<ActiveCall> = None;
    let mut backoff = RETRY_INITIAL_BACKOFF;
    loop {
        match session
            .transport
            .recv_message_deadline(REGISTRATION_POLL_INTERVAL)?
        {
            Some(SipMessage::Request(req)) if req.method == "INVITE" => {
                if active_call.is_some() {
                    tracing::info!("declining inbound call: another VoWiFi call is already active");
                    let _ = session
                        .transport
                        .send(&build_486_busy_here(&req, &random_hex(4)));
                    continue;
                }
                match handle_invite(session, &req, control_addr, veth_local_ip) {
                    Ok(call) => active_call = call,
                    Err(e) => tracing::warn!(error = %e, "failed to handle inbound INVITE"),
                }
            }
            Some(SipMessage::Request(req)) if req.method == "BYE" => match active_call.take() {
                Some(call) => handle_bye(session, &req, call),
                None => {
                    let _ = session
                        .transport
                        .send(&build_200_ok_bye(&req, &random_hex(4)));
                }
            },
            Some(SipMessage::Request(req)) if req.method == "ACK" => {
                tracing::debug!("received ACK, dialog confirmed");
            }
            Some(SipMessage::Request(req)) => {
                tracing::debug!(method = %req.method, "ignoring unsupported inbound request");
            }
            Some(SipMessage::Response(resp)) => {
                tracing::debug!(
                    status = resp.status,
                    "received response outside an active transaction, ignoring"
                );
            }
            None => {
                // Idle wake-up: nothing arrived within the poll interval.
                // Never renew mid-call — that would tear down the transport
                // a call's own signaling (e.g. the eventual BYE) still
                // needs; renewal is deferred until the call ends.
                if active_call.is_some() {
                    continue;
                }
                let Some(expires_at) = status.lock().unwrap_or_else(|e| e.into_inner()).expires_at
                else {
                    continue;
                };
                if !super::renewal_due(SystemTime::now(), expires_at, RENEWAL_HEADROOM) {
                    continue;
                }
                status.lock().unwrap_or_else(|e| e.into_inner()).state =
                    super::RegistrationState::Renewing;
                match attempt_renewal(reg_cfg) {
                    Ok(new_session) => {
                        session.cleanup();
                        *session = new_session;
                        let mut guard = status.lock().unwrap_or_else(|e| e.into_inner());
                        guard.state = super::RegistrationState::Registered;
                        guard.registered_at = Some(SystemTime::now());
                        guard.expires_at = Some(
                            SystemTime::now() + Duration::from_secs(super::DEFAULT_EXPIRES as u64),
                        );
                        drop(guard);
                        backoff = RETRY_INITIAL_BACKOFF;
                        tracing::info!("registration renewed");
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            retry_in_secs = backoff.as_secs(),
                            "registration renewal failed, retrying with backoff"
                        );
                        let mut guard = status.lock().unwrap_or_else(|e| e.into_inner());
                        guard.state = super::RegistrationState::Failed;
                        guard.last_failure = Some((SystemTime::now(), e.to_string()));
                        drop(guard);
                        std::thread::sleep(backoff);
                        backoff = next_backoff(backoff, RETRY_MAX_BACKOFF);
                    }
                }
            }
        }
    }
}

fn extract_caller(req: &SipRequest) -> String {
    req.header("From")
        .and_then(|f| f.split("sip:").nth(1))
        .and_then(|rest| rest.split(['@', ';', '>']).next())
        .unwrap_or("unknown")
        .to_string()
}

/// Answers (or declines) one inbound carrier `INVITE`. Returns `Some` with
/// the bookkeeping `handle_bye` will need once the call is actually
/// bridged; `None` if it was declined (busy line, no compatible codec, or
/// Agent B couldn't bridge it) — every decline path sends a fast, explicit
/// `486 Busy Here` per the spec's Clarifications answer, never silence or
/// unanswered ringing (FR-009/FR-010).
fn handle_invite(
    session: &mut super::RegisteredSession,
    req: &SipRequest,
    control_addr: SocketAddr,
    veth_local_ip: IpAddr,
) -> BridgeResult<Option<ActiveCall>> {
    let call_id = req.header("Call-ID").unwrap_or_default().to_string();
    let caller = extract_caller(req);
    tracing::info!(
        call_id = %call_id,
        caller = %caller,
        request_uri = %req.request_uri,
        "inbound VoWiFi call"
    );

    session.transport.send(&build_100_trying(req))?;

    let offer = sdp::parse_offer(&req.body)?;
    if !offer
        .offered
        .iter()
        .any(|c| c.codec == NegotiatedCodec::Pcmu)
    {
        // No transcode path yet (research.md item 3) — an AMR-WB-only offer
        // can't be bridged to Agent B's fixed-PCMU PJSIP leg.
        tracing::info!(call_id = %call_id, "offer has no PCMU; declining (no transcode path)");
        session
            .transport
            .send(&build_486_busy_here(req, &random_hex(4)))?;
        return Ok(None);
    }

    let veth_rx = spawn_veth_uas_listener(veth_local_ip)?;

    // Generated once and reused for every response in this dialog (180 and
    // 200 OK alike) — RFC 3261 requires the same To-tag across all
    // responses that establish/confirm one dialog.
    let to_tag = random_hex(4);
    let public_user = session
        .public_uri
        .split('@')
        .next()
        .unwrap_or(&session.public_uri)
        .to_string();
    let via_transport = if session.use_tcp { "TCP" } else { "UDP" };
    let contact = format!(
        "<sip:{public_user}@{};transport={via_transport}>",
        format_sip_addr(session.local_addr)
    );
    // Let the carrier know we're working on it while we wait on Agent B —
    // otherwise it hears nothing for up to CONTROL_TIMEOUT.
    session
        .transport
        .send(&build_180_ringing(req, &to_tag, &contact))?;

    let mut control = TcpStream::connect_timeout(&control_addr, CONTROL_TIMEOUT)
        .map_err(|e| BridgeError::Ims(format!("failed to reach Agent B control channel: {e}")))?;
    write_msg(
        &mut control,
        &ControlMessage::IncomingCall {
            call_id: call_id.clone(),
            caller,
        },
    )
    .map_err(BridgeError::Ims)?;
    let mut control_reader = BufReader::new(
        control
            .try_clone()
            .map_err(|e| BridgeError::Ims(format!("control connection clone failed: {e}")))?,
    );
    let reply = read_msg(&mut control_reader).map_err(BridgeError::Ims)?;

    match reply {
        ControlMessage::BridgeReady { .. } => {
            let veth = veth_rx.recv_timeout(VETH_INVITE_TIMEOUT).map_err(|_| {
                BridgeError::Ims("timed out waiting for Agent B's veth call".into())
            })??;

            let ims_rtp_socket = UdpSocket::bind((session.local_addr.ip(), 0))
                .map_err(|e| BridgeError::Ims(format!("IMS RTP socket bind failed: {e}")))?;
            let ims_rtp_port = ims_rtp_socket
                .local_addr()
                .map_err(|e| BridgeError::Ims(format!("IMS RTP local_addr failed: {e}")))?
                .port();
            ims_rtp_socket
                .connect(offer.remote_rtp)
                .map_err(|e| BridgeError::Ims(format!("IMS RTP connect failed: {e}")))?;

            let session_id: u64 = rand::random::<u32>() as u64;
            let (answer_sdp, _codec) = sdp::build_answer(
                session.local_addr.ip(),
                ims_rtp_port,
                session_id,
                &offer,
                false,
            )?;

            session
                .transport
                .send(&build_200_ok_invite(req, &to_tag, &contact, &answer_sdp))?;

            let stop = Arc::new(AtomicBool::new(false));
            spawn_relay(ims_rtp_socket, veth.rtp_socket, stop.clone());
            tracing::info!(call_id = %call_id, "call answered and bridged");

            Ok(Some(ActiveCall {
                control,
                stop,
                call_id,
                to_tag,
            }))
        }
        ControlMessage::BridgeFailed {
            reason: fail_reason,
            ..
        } => {
            tracing::info!(call_id = %call_id, reason = %fail_reason, "Agent B could not bridge the call, declining");
            session
                .transport
                .send(&build_486_busy_here(req, &random_hex(4)))?;
            Ok(None)
        }
        other => Err(BridgeError::Ims(format!(
            "unexpected control-channel reply to IncomingCall: {other:?}"
        ))),
    }
}

fn handle_bye(session: &mut super::RegisteredSession, req: &SipRequest, mut call: ActiveCall) {
    call.stop.store(true, Ordering::Relaxed);
    if let Err(e) = write_msg(
        &mut call.control,
        &ControlMessage::CallEnded {
            call_id: call.call_id.clone(),
            reason: reason::CALLER_HANGUP.to_string(),
        },
    ) {
        tracing::warn!(call_id = %call.call_id, error = %e, "failed to notify Agent B of hangup");
    }
    if let Err(e) = session.transport.send(&build_200_ok_bye(req, &call.to_tag)) {
        tracing::warn!(call_id = %call.call_id, error = %e, "failed to send 200 OK to BYE");
    }
    tracing::info!(call_id = %call.call_id, "call ended");
}

/// Result of Agent A's veth-facing UAS answering Agent B's inbound call.
struct VethUasResult {
    rtp_socket: UdpSocket,
}

/// Starts a background thread listening for Agent B's veth-side `INVITE`
/// (a single UDP datagram is expected — PJSIP's default offer is well under
/// any MTU), answers it, and delivers the resulting RTP socket (already
/// `connect()`-ed to Agent B's advertised RTP address) over the returned
/// channel. Started *before* signaling Agent B over the control channel so
/// the listener is guaranteed to be up by the time Agent B's `Call::make`
/// actually reaches it.
fn spawn_veth_uas_listener(
    veth_local_ip: IpAddr,
) -> BridgeResult<mpsc::Receiver<BridgeResult<VethUasResult>>> {
    let sip_socket = UdpSocket::bind((veth_local_ip, VETH_SIP_PORT))
        .map_err(|e| BridgeError::Ims(format!("veth SIP socket bind failed: {e}")))?;
    sip_socket
        .set_read_timeout(Some(VETH_INVITE_TIMEOUT))
        .map_err(|e| BridgeError::Ims(format!("veth SIP socket set_read_timeout failed: {e}")))?;

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(accept_veth_invite(&sip_socket, veth_local_ip));
    });
    Ok(rx)
}

fn accept_veth_invite(
    sip_socket: &UdpSocket,
    veth_local_ip: IpAddr,
) -> BridgeResult<VethUasResult> {
    let mut buf = [0u8; 4096];
    let (n, peer) = sip_socket
        .recv_from(&mut buf)
        .map_err(|e| BridgeError::Ims(format!("veth INVITE recv failed: {e}")))?;
    let text = String::from_utf8_lossy(&buf[..n]);
    let (req, _consumed) = SipRequest::try_parse(&text)?
        .ok_or_else(|| BridgeError::Ims("incomplete veth INVITE datagram".into()))?;
    if req.method != "INVITE" {
        return Err(BridgeError::Ims(format!(
            "expected INVITE on the veth SIP link, got {}",
            req.method
        )));
    }

    let offer = sdp::parse_offer(&req.body)?;
    let rtp_socket = UdpSocket::bind((veth_local_ip, 0))
        .map_err(|e| BridgeError::Ims(format!("veth RTP socket bind failed: {e}")))?;
    let rtp_port = rtp_socket
        .local_addr()
        .map_err(|e| BridgeError::Ims(format!("veth RTP local_addr failed: {e}")))?
        .port();

    let session_id: u64 = rand::random::<u32>() as u64;
    // No AMR-WB fallback on this internal leg — Agent B's PJSIP always
    // offers PCMU (fixed 8kHz media config, pjsua-safe/src/endpoint.rs).
    let (answer_sdp, _codec) =
        sdp::build_answer(veth_local_ip, rtp_port, session_id, &offer, false)?;
    let to_tag = random_hex(4);
    let contact = format!("<sip:agent-a@{veth_local_ip}:{VETH_SIP_PORT}>");
    let response = build_200_ok_invite(&req, &to_tag, &contact, &answer_sdp);
    sip_socket
        .send_to(response.as_bytes(), peer)
        .map_err(|e| BridgeError::Ims(format!("veth 200 OK send failed: {e}")))?;

    rtp_socket
        .connect(offer.remote_rtp)
        .map_err(|e| BridgeError::Ims(format!("veth RTP connect failed: {e}")))?;

    Ok(VethUasResult { rtp_socket })
}

fn spawn_relay(a: UdpSocket, b: UdpSocket, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || relay_rtp(a, b, stop));
}

/// Relays raw UDP payloads bidirectionally between `a` and `b` (both
/// already `connect()`-ed to their remote peer) until `stop` is set.
/// Forwards bytes verbatim rather than decoding/re-encoding: both legs
/// speak the same codec by construction — `handle_invite` only reaches this
/// point once the carrier offer negotiated PCMU, and Agent B's PJSIP leg is
/// always PCMU too — so the wire bytes (RTP header included: SSRC,
/// sequence, timestamp all stay whatever the real source generated) are
/// already correct for the other side without modification.
pub fn relay_rtp(a: UdpSocket, b: UdpSocket, stop: Arc<AtomicBool>) {
    let (a2, b2, stop2) = match (a.try_clone(), b.try_clone()) {
        (Ok(a2), Ok(b2)) => (a2, b2, stop.clone()),
        (Err(e), _) | (_, Err(e)) => {
            tracing::error!(error = %e, "RTP relay socket clone failed, aborting relay");
            return;
        }
    };
    let _ = a.set_read_timeout(Some(RELAY_POLL_INTERVAL));
    let _ = b.set_read_timeout(Some(RELAY_POLL_INTERVAL));

    let h1 = std::thread::spawn(move || forward(a, b2, stop));
    let h2 = std::thread::spawn(move || forward(b, a2, stop2));
    let _ = h1.join();
    let _ = h2.join();
}

fn forward(src: UdpSocket, dst: UdpSocket, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        match src.recv(&mut buf) {
            Ok(n) => {
                let _ = dst.send(&buf[..n]);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn loopback_socket() -> UdpSocket {
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap()
    }

    #[test]
    fn next_backoff_doubles_each_attempt() {
        let b1 = next_backoff(Duration::from_secs(5), Duration::from_secs(120));
        assert_eq!(b1, Duration::from_secs(10));
        let b2 = next_backoff(b1, Duration::from_secs(120));
        assert_eq!(b2, Duration::from_secs(20));
        let b3 = next_backoff(b2, Duration::from_secs(120));
        assert_eq!(b3, Duration::from_secs(40));
    }

    #[test]
    fn next_backoff_caps_at_max() {
        let b = next_backoff(Duration::from_secs(100), Duration::from_secs(120));
        assert_eq!(b, Duration::from_secs(120));
        // Already at (or past) the cap: stays capped, doesn't keep growing.
        let b2 = next_backoff(b, Duration::from_secs(120));
        assert_eq!(b2, Duration::from_secs(120));
    }

    #[test]
    fn next_backoff_never_overflows_on_pathological_input() {
        let b = next_backoff(Duration::MAX, Duration::from_secs(120));
        assert_eq!(b, Duration::from_secs(120));
    }

    #[test]
    fn relay_rtp_forwards_packets_in_both_directions_until_stopped() {
        // Simulate the two "legs": ims_side <-> veth_side, each with its own
        // peer socket standing in for the real remote endpoint.
        let ims_side = loopback_socket();
        let ims_peer = loopback_socket();
        ims_side.connect(ims_peer.local_addr().unwrap()).unwrap();
        ims_peer.connect(ims_side.local_addr().unwrap()).unwrap();

        let veth_side = loopback_socket();
        let veth_peer = loopback_socket();
        veth_side.connect(veth_peer.local_addr().unwrap()).unwrap();
        veth_peer.connect(veth_side.local_addr().unwrap()).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let handle = std::thread::spawn(move || relay_rtp(ims_side, veth_side, stop_clone));

        // ims_peer -> ims_side -> (relay) -> veth_side -> veth_peer
        ims_peer.send(b"hello-from-ims").unwrap();
        veth_peer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buf = [0u8; 64];
        let n = veth_peer.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello-from-ims");

        // veth_peer -> veth_side -> (relay) -> ims_side -> ims_peer
        veth_peer.send(b"hello-from-veth").unwrap();
        ims_peer
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let n = ims_peer.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello-from-veth");

        stop.store(true, Ordering::Relaxed);
        handle.join().unwrap();
    }

    #[test]
    fn extract_caller_pulls_the_user_part_from_a_quoted_from_header() {
        let raw = "INVITE sip:x SIP/2.0\r\n\
                    From: <sip:+919789063708@ims.mnc094.mcc404.3gppnetwork.org>;tag=abc\r\n\
                    Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw).unwrap().unwrap();
        assert_eq!(extract_caller(&req), "+919789063708");
    }

    #[test]
    fn extract_caller_falls_back_to_unknown_when_from_is_unparseable() {
        let raw = "INVITE sip:x SIP/2.0\r\nFrom: garbage\r\nCall-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
        let (req, _) = SipRequest::try_parse(raw).unwrap().unwrap();
        assert_eq!(extract_caller(&req), "unknown");
    }
}
