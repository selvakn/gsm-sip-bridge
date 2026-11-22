//! In-dialog requests during a *connected* call: the far end hanging up
//! (`BYE`), trying to renegotiate (`re-INVITE`) or probing (`OPTIONS`) while
//! siptest's media session is running.
//!
//! Every one of these arrives on the single shared SIP socket. Before this
//! was handled, an outbound call never read the socket for requests at all —
//! the inbound listener saw the `BYE` as a stray and dropped it unanswered
//! (the peer retransmits for ~32 s) while the call ran out its configured
//! duration, and an inbound call's media phase left the `BYE` queued until the
//! call was over. Both directions are driven here through siptest's real
//! production paths (`call::execute_outbound_call`, `daemon::inbound_listener_loop`
//! + `call::execute_inbound_call`) against a plain `UdpSocket` peer.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gsm_sip_bridge::config::secret::Secret;
use gsm_sip_bridge::ims::sip_client::{
    build_uas_response, build_uas_response_with_headers, parse_datagram, SipMessage, SipRequest,
};

use siptest::api::events::EventBus;
use siptest::api::state::{Counters, InboundMode, InboundPolicy, SharedState};
use siptest::call::{Call, Direction, EndReason};
use siptest::config::{
    ApiConfig, CallConfig, Config, InboundConfig, LoggingConfig, MediaConfig, RetentionConfig,
    SipConfig,
};
use siptest::safety::{CallAttemptHistory, SafetyPolicy};
use siptest::sip::registration::{
    RegState, RegistrationConfig, RegistrationCredentials, RegistrationStatus,
};
use siptest::sip::socket::SipSocket;

const DESTINATION: &str = "+919000000000";
/// How long the call is configured to run. A remote hang-up must end it well
/// before this.
const CALL_SECS: u64 = 3;
/// When, after the call is connected, the peer sends its in-dialog request.
const PEER_ACTS_AFTER: Duration = Duration::from_millis(700);

const PEER_SDP: &str = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {PORT} RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\n";

fn test_config(rtp_min: u16, inbound_secs: u32) -> Config {
    Config {
        sip: SipConfig {
            bridge_host: "127.0.0.1".to_string(),
            registrar_port: 5060,
            outbound_port: 5072,
            local_ip: Some("127.0.0.1".to_string()),
            local_port: 0,
            username: "1002".to_string(),
            password: Secret::new("hunter2".to_string()),
            realm: "test-realm".to_string(),
            register_expires_secs: 300,
        },
        media: MediaConfig {
            codec: "auto".to_string(),
            rtp_port_min: rtp_min,
            rtp_port_max: rtp_min + 100,
            tone_plan: "grid8".to_string(),
            recording_dir: std::env::temp_dir().join("siptest-in-dialog-test"),
            record: false,
            play_file: None,
        },
        call: CallConfig {
            default_duration_secs: CALL_SECS as u32,
            ring_timeout_secs: 3,
            require: "packets".to_string(),
        },
        safety: SafetyPolicy {
            allowed_destinations: vec![DESTINATION.to_string()],
            min_call_interval_secs: 0,
            max_calls_per_hour: 1000,
        },
        retention: RetentionConfig {
            max_calls_retained: 50,
        },
        inbound: InboundConfig {
            mode: "answer".to_string(),
            answer_delay_ms: 0,
            reject_status: 486,
            duration_secs: inbound_secs,
        },
        api: ApiConfig {
            bind: "127.0.0.1:0".to_string(),
        },
        logging: LoggingConfig {
            level: "info".to_string(),
        },
    }
}

fn build_state(
    config: Config,
    sip_socket: Arc<SipSocket>,
    registrar: SocketAddr,
) -> Arc<SharedState> {
    let inbound_policy = InboundPolicy {
        mode: config.inbound.mode.parse().unwrap_or(InboundMode::Answer),
        answer_delay_ms: config.inbound.answer_delay_ms,
        reject_status: config.inbound.reject_status,
        duration_secs: config.inbound.duration_secs,
    };
    let registration = RegistrationStatus {
        state: RegState::Registered,
        ..RegistrationStatus::default()
    };
    Arc::new(SharedState {
        registration: Mutex::new(registration),
        calls: Mutex::new(siptest::api::state::CallRegistry::new(
            config.retention.max_calls_retained,
        )),
        attempt_history: Mutex::new(CallAttemptHistory::new()),
        counters: Mutex::new(Counters::default()),
        local_sip_addr: sip_socket.local_addr(),
        bridge_registrar: registrar,
        last_outbound_observed: Mutex::new(None),
        events: EventBus::default(),
        safety: config.safety.clone(),
        config,
        next_call_seq: Mutex::new(0),
        sip_socket,
        registration_creds: Mutex::new(RegistrationCredentials {
            cseq: 0,
            call_id: "reg".to_string(),
            from_tag: "regtag".to_string(),
            cached_nonce: None,
            nc: 0,
        }),
        registration_config: RegistrationConfig {
            registrar_addr: registrar,
            registrar_host: "test-realm".to_string(),
            aor_user: "1002".to_string(),
            realm: "test-realm".to_string(),
            password: Secret::new("hunter2".to_string()),
            expires: 300,
        },
        inbound_policy: Mutex::new(inbound_policy),
        manual_decisions: Mutex::new(std::collections::HashMap::new()),
        dialog_route: Mutex::new(None),
    })
}

/// What the peer does once the call is connected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PeerAction {
    Bye,
    ReInvite,
    Update,
    /// An `OPTIONS` carrying its own Call-ID — a keepalive, not part of the
    /// dialog.
    Options,
}

impl PeerAction {
    fn method(self) -> &'static str {
        match self {
            PeerAction::Bye => "BYE",
            PeerAction::ReInvite => "INVITE",
            PeerAction::Update => "UPDATE",
            PeerAction::Options => "OPTIONS",
        }
    }
}

/// Everything the peer saw, as raw datagrams, in order.
#[derive(Clone, Default)]
struct PeerLog(Arc<Mutex<Vec<String>>>);

impl PeerLog {
    fn push(&self, text: String) {
        self.0.lock().unwrap().push(text);
    }

    fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    /// Requests of `method` that siptest itself sent the peer.
    fn requests(&self, method: &str) -> usize {
        self.all()
            .iter()
            .filter(|t| t.starts_with(&format!("{method} ")))
            .count()
    }

    /// Did siptest answer the peer's `method` request with `status`?
    fn answered(&self, status: u16, method: &str) -> bool {
        self.all().iter().any(|t| {
            t.starts_with(&format!("SIP/2.0 {status}")) && t.contains(&format!(" {method}\r\n"))
        })
    }
}

/// An in-dialog (or, for `Options`, out-of-dialog) request from the peer.
/// `from`/`to` are already complete header values with the dialog's tags.
#[allow(clippy::too_many_arguments)]
fn peer_request(
    action: PeerAction,
    siptest_addr: SocketAddr,
    peer_addr: SocketAddr,
    call_id: &str,
    from: &str,
    to: &str,
    cseq: u32,
    media_port: u16,
) -> String {
    let method = action.method();
    let (call_id, cseq_line) = if action == PeerAction::Options {
        ("peer-options-call", format!("{cseq} {method}"))
    } else {
        (call_id, format!("{cseq} {method}"))
    };
    let body = if action == PeerAction::ReInvite {
        PEER_SDP.replace("{PORT}", &media_port.to_string())
    } else {
        String::new()
    };
    let content = if body.is_empty() {
        String::new()
    } else {
        "Content-Type: application/sdp\r\n".to_string()
    };
    format!(
        "{method} sip:1002@{siptest_addr} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {peer_addr};branch=z9hG4bKpeer{method}{cseq}\r\n\
         Max-Forwards: 70\r\n\
         From: {from}\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq_line}\r\n\
         Contact: <sip:peer@{peer_addr}>\r\n\
         {content}Content-Length: {}\r\n\r\n\
         {body}",
        body.len(),
    )
}

struct Finished {
    call: Call,
    log: PeerLog,
    /// How long the call took from the moment it was placed/answered to the
    /// moment it was recorded as ended.
    elapsed: Duration,
    state: Arc<SharedState>,
}

/// Outbound: siptest dials; the peer (standing in for the registrar *and*
/// the callee) answers at once and sends `action` shortly after the ACK.
fn run_outbound(action: PeerAction, act_after: Duration, rtp_min: u16) -> Finished {
    let config = test_config(rtp_min, CALL_SECS as u32);
    std::fs::create_dir_all(&config.media.recording_dir).unwrap();

    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let rtp_sink = UdpSocket::bind("127.0.0.1:0").unwrap();
    let media_port = rtp_sink.local_addr().unwrap().port();

    let sip_socket =
        Arc::new(SipSocket::bind(Some("127.0.0.1".parse().unwrap()), 0, peer_addr).unwrap());
    let state = build_state(config, sip_socket, peer_addr);

    let stop = Arc::new(AtomicBool::new(false));
    let listener = {
        let state = state.clone();
        let stop = stop.clone();
        thread::spawn(move || siptest::daemon::inbound_listener_loop(state, stop))
    };

    let log = PeerLog::default();
    let peer_thread = {
        let log = log.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            let _keep_rtp_sink = rtp_sink;
            let contact = format!("<sip:callee@{peer_addr}>");
            let mut buf = [0u8; 4096];
            let mut dialog: Option<(SipRequest, SocketAddr)> = None;
            let mut act_at: Option<Instant> = None;
            let mut acted = false;
            while !stop.load(Ordering::Relaxed) {
                if let (Some(at), false, Some((invite, src))) = (act_at, acted, dialog.as_ref()) {
                    if Instant::now() >= at {
                        acted = true;
                        let from = format!("{};tag=callee", invite.header("To").unwrap());
                        let to = invite.header("From").unwrap().to_string();
                        let msg = peer_request(
                            action,
                            *src,
                            peer_addr,
                            invite.header("Call-ID").unwrap(),
                            &from,
                            &to,
                            1,
                            media_port,
                        );
                        let _ = peer.send_to(msg.as_bytes(), src);
                    }
                }
                let Ok((n, src)) = peer.recv_from(&mut buf) else {
                    continue;
                };
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                log.push(text.clone());
                if let Ok(Some(SipMessage::Request(req))) = parse_datagram(&text) {
                    match req.method.as_str() {
                        "INVITE" if dialog.is_none() => {
                            let _ = peer.send_to(
                                build_uas_response(100, "Trying", &req, None, None, None)
                                    .as_bytes(),
                                src,
                            );
                            let ok = build_uas_response_with_headers(
                                200,
                                "OK",
                                &req,
                                Some("callee"),
                                Some(&contact),
                                Some(&PEER_SDP.replace("{PORT}", &media_port.to_string())),
                                &[],
                            );
                            let _ = peer.send_to(ok.as_bytes(), src);
                            dialog = Some((req, src));
                        }
                        "ACK" if act_at.is_none() => act_at = Some(Instant::now() + act_after),
                        "BYE" => {
                            let _ = peer.send_to(
                                build_uas_response(200, "OK", &req, None, None, None).as_bytes(),
                                src,
                            );
                        }
                        _ => {}
                    }
                }
            }
        })
    };

    let started = Instant::now();
    let call = siptest::call::execute_outbound_call(
        &state,
        DESTINATION.to_string(),
        Duration::from_secs(CALL_SECS),
        Duration::from_secs(3),
        "pcmu",
    )
    .expect("the outbound call should complete");
    let elapsed = started.elapsed();

    // Let any late datagram (our own BYE, a retransmitted reply) arrive.
    thread::sleep(Duration::from_millis(500));
    stop.store(true, Ordering::Relaxed);
    let _ = peer_thread.join();
    let _ = listener.join();
    Finished {
        call,
        log,
        elapsed,
        state,
    }
}

/// Inbound: the peer (standing in for the bridge's telephony agent) rings
/// siptest, ACKs the answer, then sends `action` shortly after.
fn run_inbound(action: PeerAction, act_after: Duration, rtp_min: u16) -> Finished {
    let config = test_config(rtp_min, CALL_SECS as u32);
    std::fs::create_dir_all(&config.media.recording_dir).unwrap();

    let caller = UdpSocket::bind("127.0.0.1:0").unwrap();
    caller
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let caller_addr = caller.local_addr().unwrap();
    let rtp_sink = UdpSocket::bind("127.0.0.1:0").unwrap();
    let media_port = rtp_sink.local_addr().unwrap().port();

    let sip_socket =
        Arc::new(SipSocket::bind(Some("127.0.0.1".parse().unwrap()), 0, caller_addr).unwrap());
    let siptest_addr = sip_socket.local_addr();
    let state = build_state(config, sip_socket, caller_addr);

    let stop = Arc::new(AtomicBool::new(false));
    let listener = {
        let state = state.clone();
        let stop = stop.clone();
        thread::spawn(move || siptest::daemon::inbound_listener_loop(state, stop))
    };

    let log = PeerLog::default();
    let call_id = "inbound-in-dialog-call";
    let from = "<sip:+919000000000@127.0.0.1:5060>;tag=callertag".to_string();
    let sdp = PEER_SDP.replace("{PORT}", &media_port.to_string());
    let invite = format!(
        "INVITE sip:1002@{siptest_addr} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {caller_addr};branch=z9hG4bKcallerbranch\r\n\
         Max-Forwards: 70\r\n\
         From: {from}\r\n\
         To: <sip:1002@{siptest_addr}>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:agent@{caller_addr}>\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {}\r\n\r\n\
         {sdp}",
        sdp.len(),
    );
    caller.send_to(invite.as_bytes(), siptest_addr).unwrap();

    let mut buf = [0u8; 4096];
    let mut recv = |log: &PeerLog| -> Option<String> {
        let (n, _src) = caller.recv_from(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf[..n]).to_string();
        log.push(text.clone());
        Some(text)
    };

    // 100, 180, 200 — in that order — then our ACK.
    let deadline = Instant::now() + Duration::from_secs(5);
    let ok = loop {
        assert!(
            Instant::now() < deadline,
            "never saw a 200 OK to the INVITE"
        );
        if let Some(text) = recv(&log) {
            if text.starts_with("SIP/2.0 200") {
                break text;
            }
        }
    };
    let to_tag = ok
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("to:"))
        .and_then(|l| l.split("tag=").nth(1))
        .map(|s| s.trim().to_string())
        .expect("200 OK must carry a To tag");
    let to = format!("<sip:1002@{siptest_addr}>;tag={to_tag}");
    let ack = format!(
        "ACK sip:1002@{siptest_addr} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {caller_addr};branch=z9hG4bKackbranch\r\n\
         Max-Forwards: 70\r\n\
         From: {from}\r\n\
         To: {to}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 ACK\r\n\
         Content-Length: 0\r\n\r\n"
    );
    caller.send_to(ack.as_bytes(), siptest_addr).unwrap();
    let connected = Instant::now();

    let act_at = connected + act_after;
    let mut acted = false;
    let mut ended_at: Option<Instant> = None;
    let hard_stop = connected + Duration::from_secs(CALL_SECS + 4);
    loop {
        if !acted && Instant::now() >= act_at {
            acted = true;
            let msg = peer_request(
                action,
                siptest_addr,
                caller_addr,
                call_id,
                &from,
                &to,
                2,
                media_port,
            );
            caller.send_to(msg.as_bytes(), siptest_addr).unwrap();
        }
        if let Some(text) = recv(&log) {
            // Answer siptest's own BYE, if it sends one.
            if text.starts_with("BYE ") {
                if let Ok(Some(SipMessage::Request(req))) = parse_datagram(&text) {
                    let _ = caller.send_to(
                        build_uas_response(200, "OK", &req, None, None, None).as_bytes(),
                        siptest_addr,
                    );
                }
            }
        }
        if ended_at.is_none() && state.calls.lock().unwrap().active().is_none() {
            ended_at = Some(Instant::now());
        }
        if let Some(t) = ended_at {
            if t.elapsed() > Duration::from_millis(500) {
                break;
            }
        }
        if Instant::now() > hard_stop {
            break;
        }
    }
    let elapsed = ended_at.unwrap_or_else(Instant::now) - connected;

    stop.store(true, Ordering::Relaxed);
    let _keep = rtp_sink;
    let _ = listener.join();
    let call = state
        .calls
        .lock()
        .unwrap()
        .recent(5)
        .into_iter()
        .find(|c| c.direction == Direction::Inbound)
        .expect("the inbound call should have been recorded");
    Finished {
        call,
        log,
        elapsed,
        state,
    }
}

fn assert_nothing_left_registered(f: &Finished) {
    assert!(
        f.state.calls.lock().unwrap().active().is_none(),
        "the call slot must be free once the call has ended"
    );
    assert!(
        f.state.dialog_route.lock().unwrap().is_none(),
        "no dialog route may outlive its call"
    );
}

// ---------------------------------------------------------------------------
// Remote BYE
// ---------------------------------------------------------------------------

#[test]
fn outbound_remote_bye_is_answered_and_ends_the_call_early() {
    let f = run_outbound(PeerAction::Bye, PEER_ACTS_AFTER, 41200);

    assert!(
        f.log.answered(200, "BYE"),
        "the far end's BYE was never answered with 200: {:#?}",
        f.log.all()
    );
    assert_eq!(f.call.end_reason, Some(EndReason::RemoteBye));
    assert!(
        f.elapsed < Duration::from_secs(CALL_SECS - 1),
        "the call kept running for {:?} after the far end hung up",
        f.elapsed
    );
    assert_eq!(
        f.log.requests("BYE"),
        0,
        "siptest must not BYE a dialog the far end already ended"
    );
    assert!(
        f.call.report.is_some(),
        "an early hang-up still reports the media it carried"
    );
    assert_nothing_left_registered(&f);
}

#[test]
fn outbound_bye_straight_after_the_ack_is_not_lost() {
    let f = run_outbound(PeerAction::Bye, Duration::ZERO, 41300);

    assert!(
        f.log.answered(200, "BYE"),
        "a BYE racing the start of media was lost: {:#?}",
        f.log.all()
    );
    assert_eq!(f.call.end_reason, Some(EndReason::RemoteBye));
    assert_eq!(f.log.requests("BYE"), 0);
    assert_nothing_left_registered(&f);
}

#[test]
fn inbound_remote_bye_is_answered_and_ends_the_call_early() {
    let f = run_inbound(PeerAction::Bye, PEER_ACTS_AFTER, 41400);

    assert!(
        f.log.answered(200, "BYE"),
        "the far end's BYE was never answered with 200: {:#?}",
        f.log.all()
    );
    assert_eq!(f.call.end_reason, Some(EndReason::RemoteBye));
    assert!(
        f.elapsed < Duration::from_secs(CALL_SECS - 1),
        "the call kept running for {:?} after the far end hung up",
        f.elapsed
    );
    assert_eq!(f.log.requests("BYE"), 0);
    assert!(f.call.report.is_some());
    assert_nothing_left_registered(&f);
}

// ---------------------------------------------------------------------------
// re-INVITE / UPDATE are refused, and the call carries on
// ---------------------------------------------------------------------------

#[test]
fn outbound_re_invite_is_refused_with_488_and_the_call_carries_on() {
    let f = run_outbound(PeerAction::ReInvite, PEER_ACTS_AFTER, 41500);

    assert!(
        f.log.answered(488, "INVITE"),
        "a re-INVITE must be refused with 488: {:#?}",
        f.log.all()
    );
    assert_eq!(f.call.end_reason, Some(EndReason::DurationElapsed));
    assert!(
        f.elapsed >= Duration::from_secs(CALL_SECS - 1),
        "refusing a re-INVITE must not end the call (ran {:?})",
        f.elapsed
    );
    assert_eq!(f.log.requests("BYE"), 1, "we hang up at the end ourselves");
    assert_nothing_left_registered(&f);
}

#[test]
fn outbound_update_is_refused_with_488_and_the_call_carries_on() {
    let f = run_outbound(PeerAction::Update, PEER_ACTS_AFTER, 41600);

    assert!(
        f.log.answered(488, "UPDATE"),
        "an UPDATE must be refused with 488: {:#?}",
        f.log.all()
    );
    assert_eq!(f.call.end_reason, Some(EndReason::DurationElapsed));
    assert_nothing_left_registered(&f);
}

#[test]
fn inbound_re_invite_is_refused_with_488_and_the_call_carries_on() {
    let f = run_inbound(PeerAction::ReInvite, PEER_ACTS_AFTER, 41700);

    assert!(
        f.log.answered(488, "INVITE"),
        "a re-INVITE must be refused with 488: {:#?}",
        f.log.all()
    );
    assert_eq!(f.call.end_reason, Some(EndReason::DurationElapsed));
    assert!(f.elapsed >= Duration::from_secs(CALL_SECS - 1));
    assert_nothing_left_registered(&f);
}

// ---------------------------------------------------------------------------
// OPTIONS keepalives are answered mid-call, in both directions
// ---------------------------------------------------------------------------

#[test]
fn outbound_options_during_a_call_is_answered_and_does_not_disturb_it() {
    let f = run_outbound(PeerAction::Options, PEER_ACTS_AFTER, 41800);

    assert!(f.log.answered(200, "OPTIONS"), "{:#?}", f.log.all());
    assert_eq!(f.call.end_reason, Some(EndReason::DurationElapsed));
    assert_nothing_left_registered(&f);
}

#[test]
fn inbound_options_during_a_call_is_answered_and_does_not_disturb_it() {
    let f = run_inbound(PeerAction::Options, PEER_ACTS_AFTER, 41900);

    assert!(f.log.answered(200, "OPTIONS"), "{:#?}", f.log.all());
    assert_eq!(f.call.end_reason, Some(EndReason::DurationElapsed));
    assert_nothing_left_registered(&f);
}

// ---------------------------------------------------------------------------
// The route never outlives its call, whichever way the call ends
// ---------------------------------------------------------------------------

/// A registrar that never answers: the call fails without ever reaching the
/// media phase (where the watcher would have drained the route). The route
/// must still be gone.
#[test]
fn an_unanswered_outbound_call_leaves_no_dialog_route_behind() {
    let config = test_config(42000, CALL_SECS as u32);
    std::fs::create_dir_all(&config.media.recording_dir).unwrap();
    let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
    let silent_addr = silent.local_addr().unwrap();
    let sip_socket =
        Arc::new(SipSocket::bind(Some("127.0.0.1".parse().unwrap()), 0, silent_addr).unwrap());
    let state = build_state(config, sip_socket, silent_addr);

    let call = siptest::call::execute_outbound_call(
        &state,
        DESTINATION.to_string(),
        Duration::from_secs(CALL_SECS),
        Duration::from_secs(1),
        "pcmu",
    )
    .expect("an unanswered call is a result, not an error");

    assert!(
        matches!(call.end_reason, Some(EndReason::Failed { .. })),
        "a silent registrar is a failed call: {:?}",
        call.end_reason
    );
    assert!(state.dialog_route.lock().unwrap().is_none());
    assert!(state.calls.lock().unwrap().active().is_none());
}
