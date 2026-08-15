//! US3: an inbound call the bridge sends must be accepted regardless of
//! which source port it arrives from — the registrar and the telephony
//! agent that actually rings the phone are different processes on different
//! ports (research.md R2, contracts/sip-flows.md C-3). Validating on source
//! *port* would reproduce the "Accept SIP Trust Server Only" trap real
//! handsets hit against this bridge.
//!
//! This test drives `siptest`'s production inbound path — `daemon::inbound_listener_loop`
//! plus `call::execute_inbound_call` — directly against a hand-built
//! `SharedState`, with a plain `UdpSocket` standing in for the bridge's
//! telephony agent. No mocking of siptest's own logic: every response
//! observed here is built by the real code under test.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use gsm_sip_bridge::config::secret::Secret;

use siptest::api::events::EventBus;
use siptest::api::state::{Counters, InboundMode, InboundPolicy, SharedState};
use siptest::config::{
    ApiConfig, CallConfig, Config, InboundConfig, LoggingConfig, MediaConfig, RetentionConfig,
    SipConfig,
};
use siptest::safety::{CallAttemptHistory, SafetyPolicy};
use siptest::sip::registration::{RegistrationCredentials, RegistrationStatus};
use siptest::sip::socket::SipSocket;

fn test_config() -> Config {
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
            rtp_port_min: 41000,
            rtp_port_max: 41100,
            tone_plan: "grid8".to_string(),
            recording_dir: std::env::temp_dir().join("siptest-inbound-test"),
            record: false,
        },
        call: CallConfig {
            default_duration_secs: 1,
            ring_timeout_secs: 3,
            require: "packets".to_string(),
        },
        safety: SafetyPolicy {
            allowed_destinations: vec![],
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
            duration_secs: 1,
        },
        api: ApiConfig {
            bind: "127.0.0.1:0".to_string(),
        },
        logging: LoggingConfig {
            level: "info".to_string(),
        },
    }
}

fn build_state(config: Config, sip_socket: Arc<SipSocket>) -> Arc<SharedState> {
    let inbound_policy = InboundPolicy {
        mode: config.inbound.mode.parse().unwrap_or(InboundMode::Answer),
        answer_delay_ms: config.inbound.answer_delay_ms,
        reject_status: config.inbound.reject_status,
        duration_secs: config.inbound.duration_secs,
    };
    Arc::new(SharedState {
        registration: Mutex::new(RegistrationStatus::default()),
        calls: Mutex::new(siptest::api::state::CallRegistry::new(
            config.retention.max_calls_retained,
        )),
        attempt_history: Mutex::new(CallAttemptHistory::new()),
        counters: Mutex::new(Counters::default()),
        local_sip_addr: sip_socket.local_addr(),
        bridge_registrar: "127.0.0.1:1".parse().unwrap(),
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
        inbound_policy: Mutex::new(inbound_policy),
        manual_decisions: Mutex::new(std::collections::HashMap::new()),
    })
}

fn recv_line(socket: &UdpSocket) -> String {
    let mut buf = [0u8; 4096];
    let (n, _src) = socket.recv_from(&mut buf).expect("expected a response");
    String::from_utf8_lossy(&buf[..n]).to_string()
}

#[test]
fn an_inbound_invite_from_an_unexpected_source_port_is_accepted_and_answered() {
    let config = test_config();
    std::fs::create_dir_all(&config.media.recording_dir).unwrap();

    let sip_socket = Arc::new(
        SipSocket::bind(
            Some("127.0.0.1".parse().unwrap()),
            0,
            "127.0.0.1:1".parse().unwrap(),
        )
        .unwrap(),
    );
    let siptest_addr = sip_socket.local_addr();
    let state = build_state(config, sip_socket);

    let stop = Arc::new(AtomicBool::new(false));
    let listener = {
        let state = state.clone();
        let stop = stop.clone();
        thread::spawn(move || siptest::daemon::inbound_listener_loop(state, stop))
    };

    // The "caller" — standing in for the bridge's telephony agent — binds an
    // ephemeral port. It is deliberately NOT the port siptest would have
    // registered to (there is no registrar in this test at all): the whole
    // point is that inbound must work regardless of source port.
    let caller = UdpSocket::bind("127.0.0.1:0").unwrap();
    caller
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let caller_addr = caller.local_addr().unwrap();

    let sdp_body = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 42000 RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\n";
    let invite = format!(
        "INVITE sip:1002@{siptest_addr} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {caller_addr};branch=z9hG4bKcallerbranch\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:+919000000000@127.0.0.1:5060>;tag=callertag\r\n\
         P-Asserted-Identity: <sip:+919000000000@ims.example.org>\r\n\
         X-GSM-Caller-ID: +919000000000\r\n\
         To: <sip:1002@{siptest_addr}>\r\n\
         Call-ID: inbound-test-call-1\r\n\
         CSeq: 1 INVITE\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {}\r\n\r\n\
         {sdp_body}",
        sdp_body.len(),
    );
    caller.send_to(invite.as_bytes(), siptest_addr).unwrap();

    let trying = recv_line(&caller);
    assert!(
        trying.starts_with("SIP/2.0 100"),
        "expected 100 Trying, got: {trying}"
    );

    let ringing = recv_line(&caller);
    assert!(
        ringing.starts_with("SIP/2.0 180"),
        "expected 180 Ringing, got: {ringing}"
    );

    let ok = recv_line(&caller);
    assert!(ok.starts_with("SIP/2.0 200"), "expected 200 OK, got: {ok}");
    assert!(
        ok.contains("a=rtpmap:0 PCMU/8000"),
        "expected our SDP answer to offer PCMU: {ok}"
    );

    // Extract the To-tag siptest minted so the ACK is a well-formed in-dialog request.
    let to_tag = ok
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("to:"))
        .and_then(|l| l.split("tag=").nth(1))
        .map(|s| s.trim().to_string())
        .expect("200 OK must carry a To tag");

    let ack = format!(
        "ACK sip:1002@{siptest_addr} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {caller_addr};branch=z9hG4bKackbranch\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:+919000000000@127.0.0.1:5060>;tag=callertag\r\n\
         To: <sip:1002@{siptest_addr}>;tag={to_tag}\r\n\
         Call-ID: inbound-test-call-1\r\n\
         CSeq: 1 ACK\r\n\
         Content-Length: 0\r\n\r\n"
    );
    caller.send_to(ack.as_bytes(), siptest_addr).unwrap();

    // Let the call run its (1s configured) duration and report itself.
    thread::sleep(Duration::from_millis(1500));

    let calls = state.calls.lock().unwrap();
    let recent = calls.recent(5);
    assert_eq!(recent.len(), 1, "expected exactly one call recorded");
    let call = &recent[0];
    assert_eq!(call.direction, siptest::call::Direction::Inbound);
    assert_eq!(
        call.caller_id.p_asserted_identity.as_deref(),
        Some("<sip:+919000000000@ims.example.org>")
    );
    assert_eq!(
        call.caller_id.x_gsm_caller_id.as_deref(),
        Some("+919000000000")
    );
    drop(calls);

    stop.store(true, Ordering::Relaxed);
    let _ = listener.join();
}

/// contracts/sip-flows.md C-3: a CANCEL arriving before we answer must get a
/// `200` for the CANCEL itself and a `487` for the original INVITE — and the
/// call must be recorded as caller-cancelled, not as a fault (spec.md edge
/// case: "the caller abandons the call before it is answered").
#[test]
fn a_cancel_before_answer_yields_200_and_487_and_is_recorded_as_caller_cancelled() {
    let mut config = test_config();
    // A long enough answer delay to leave a real window to send the CANCEL in.
    config.inbound.answer_delay_ms = 1500;
    std::fs::create_dir_all(&config.media.recording_dir).unwrap();

    let sip_socket = Arc::new(
        SipSocket::bind(
            Some("127.0.0.1".parse().unwrap()),
            0,
            "127.0.0.1:1".parse().unwrap(),
        )
        .unwrap(),
    );
    let siptest_addr = sip_socket.local_addr();
    let state = build_state(config, sip_socket);

    let stop = Arc::new(AtomicBool::new(false));
    let listener = {
        let state = state.clone();
        let stop = stop.clone();
        thread::spawn(move || siptest::daemon::inbound_listener_loop(state, stop))
    };

    let caller = UdpSocket::bind("127.0.0.1:0").unwrap();
    caller
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let caller_addr = caller.local_addr().unwrap();

    let sdp_body = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 42000 RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\n";
    let invite = format!(
        "INVITE sip:1002@{siptest_addr} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {caller_addr};branch=z9hG4bKcallerbranch2\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:+919000000000@127.0.0.1:5060>;tag=callertag2\r\n\
         To: <sip:1002@{siptest_addr}>\r\n\
         Call-ID: inbound-test-call-2\r\n\
         CSeq: 1 INVITE\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {}\r\n\r\n\
         {sdp_body}",
        sdp_body.len(),
    );
    caller.send_to(invite.as_bytes(), siptest_addr).unwrap();

    let trying = recv_line(&caller);
    assert!(trying.starts_with("SIP/2.0 100"));
    let ringing = recv_line(&caller);
    assert!(ringing.starts_with("SIP/2.0 180"));

    // Cancel well within the 1.5s answer delay.
    thread::sleep(Duration::from_millis(100));
    let cancel = format!(
        "CANCEL sip:1002@{siptest_addr} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {caller_addr};branch=z9hG4bKcallerbranch2\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:+919000000000@127.0.0.1:5060>;tag=callertag2\r\n\
         To: <sip:1002@{siptest_addr}>\r\n\
         Call-ID: inbound-test-call-2\r\n\
         CSeq: 1 CANCEL\r\n\
         Content-Length: 0\r\n\r\n"
    );
    caller.send_to(cancel.as_bytes(), siptest_addr).unwrap();

    let cancel_ok = recv_line(&caller);
    assert!(
        cancel_ok.starts_with("SIP/2.0 200"),
        "expected 200 for the CANCEL, got: {cancel_ok}"
    );
    let terminated = recv_line(&caller);
    assert!(
        terminated.starts_with("SIP/2.0 487"),
        "expected 487 for the original INVITE, got: {terminated}"
    );

    thread::sleep(Duration::from_millis(100));
    let calls = state.calls.lock().unwrap();
    let recent = calls.recent(5);
    assert_eq!(recent.len(), 1);
    assert_eq!(
        recent[0].end_reason,
        Some(siptest::call::EndReason::CallerCancelled)
    );
    drop(calls);

    stop.store(true, Ordering::Relaxed);
    let _ = listener.join();
}
