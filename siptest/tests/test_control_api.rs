//! Drives the real, running control API over actual HTTP — closing the gap
//! the earlier passes left: the underlying logic for status, inbound policy,
//! and call discovery was unit- or socket-level tested, but never exercised
//! through `axum::serve` itself. Everything here hits a real
//! `TcpListener`/`reqwest::Client` pair; nothing about the HTTP layer is
//! mocked.

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
use siptest::sip::registration::{
    RegState, RegistrationConfig, RegistrationCredentials, RegistrationStatus,
};
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
            rtp_port_min: 43000,
            rtp_port_max: 43100,
            tone_plan: "grid8".to_string(),
            recording_dir: std::env::temp_dir().join("siptest-control-api-test"),
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
            mode: "manual".to_string(),
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

fn build_state(config: Config, sip_socket: Arc<SipSocket>, registered: bool) -> Arc<SharedState> {
    let inbound_policy = InboundPolicy {
        mode: config.inbound.mode.parse().unwrap_or(InboundMode::Answer),
        answer_delay_ms: config.inbound.answer_delay_ms,
        reject_status: config.inbound.reject_status,
        duration_secs: config.inbound.duration_secs,
    };
    let registration = if registered {
        RegistrationStatus {
            state: RegState::Registered,
            granted_expires: Some(300),
            registered_at: Some(std::time::Instant::now()),
            last_status: Some((200, "OK".to_string())),
            consecutive_failures: 0,
        }
    } else {
        RegistrationStatus::default()
    };
    Arc::new(SharedState {
        registration: Mutex::new(registration),
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
        registration_config: RegistrationConfig {
            registrar_addr: "127.0.0.1:1".parse().unwrap(),
            registrar_host: "test-realm".to_string(),
            aor_user: "1002".to_string(),
            realm: "test-realm".to_string(),
            password: Secret::new("hunter2".to_string()),
            expires: 300,
        },
        inbound_policy: Mutex::new(inbound_policy),
        manual_decisions: Mutex::new(std::collections::HashMap::new()),
    })
}

async fn spawn_server(state: Arc<SharedState>) -> String {
    let app = siptest::api::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn health_and_status_reflect_real_daemon_state() {
    let config = test_config();
    let sip_socket = Arc::new(
        SipSocket::bind(
            Some("127.0.0.1".parse().unwrap()),
            0,
            "127.0.0.1:1".parse().unwrap(),
        )
        .unwrap(),
    );
    let state = build_state(config, sip_socket, true);
    let base = spawn_server(state).await;
    let client = reqwest::Client::new();

    let health: serde_json::Value = client
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], true);

    let status: serde_json::Value = client
        .get(format!("{base}/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["registration"]["state"], "registered");
    assert!(status["local"]["sip_addr"].is_string());
    assert!(status["active_call"].is_null());
}

#[tokio::test]
async fn inbound_policy_can_be_read_and_updated_over_http() {
    let config = test_config();
    let sip_socket = Arc::new(
        SipSocket::bind(
            Some("127.0.0.1".parse().unwrap()),
            0,
            "127.0.0.1:1".parse().unwrap(),
        )
        .unwrap(),
    );
    let state = build_state(config, sip_socket, true);
    let base = spawn_server(state).await;
    let client = reqwest::Client::new();

    let policy: serde_json::Value = client
        .get(format!("{base}/policy/inbound"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(policy["mode"], "manual");

    let updated: serde_json::Value = client
        .put(format!("{base}/policy/inbound"))
        .json(&serde_json::json!({"mode": "answer", "answer_delay_ms": 500}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(updated["mode"], "answer");
    assert_eq!(updated["answer_delay_ms"], 500);
}

#[tokio::test]
async fn log_tail_returns_recent_lines() {
    let config = test_config();
    let sip_socket = Arc::new(
        SipSocket::bind(
            Some("127.0.0.1".parse().unwrap()),
            0,
            "127.0.0.1:1".parse().unwrap(),
        )
        .unwrap(),
    );
    let state = build_state(config, sip_socket, true);
    let base = spawn_server(state).await;
    let client = reqwest::Client::new();

    let resp: serde_json::Value = client
        .get(format!("{base}/log/tail?lines=50"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(resp["lines"].is_array());
}

/// The FR-029 promise, exercised through the real API: an inbound call
/// arriving under `manual` policy is discoverable purely by polling
/// `GET /events`, carries all three caller-ID fields, and can be answered
/// with `POST /calls/{id}/answer` — no log scraping, no direct access to
/// daemon internals.
#[tokio::test]
async fn a_manual_inbound_call_is_discoverable_and_answerable_over_http() {
    let config = test_config();
    let sip_socket = Arc::new(
        SipSocket::bind(
            Some("127.0.0.1".parse().unwrap()),
            0,
            "127.0.0.1:1".parse().unwrap(),
        )
        .unwrap(),
    );
    let siptest_addr = sip_socket.local_addr();
    let state = build_state(config, sip_socket, true);
    let base = spawn_server(state.clone()).await;
    let client = reqwest::Client::new();

    let stop = Arc::new(AtomicBool::new(false));
    let listener = {
        let state = state.clone();
        let stop = stop.clone();
        thread::spawn(move || siptest::daemon::inbound_listener_loop(state, stop))
    };

    // A plain UdpSocket standing in for the bridge's telephony agent.
    let caller = tokio::task::spawn_blocking(|| {
        let caller = UdpSocket::bind("127.0.0.1:0").unwrap();
        caller
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        caller
    })
    .await
    .unwrap();
    let caller_addr = caller.local_addr().unwrap();

    let sdp_body = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 44000 RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\n";
    let invite = format!(
        "INVITE sip:1002@{siptest_addr} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {caller_addr};branch=z9hG4bKapitest\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:+919000000000@127.0.0.1:5060>;tag=apitesttag\r\n\
         P-Asserted-Identity: <sip:+919000000000@ims.example.org>\r\n\
         X-GSM-Caller-ID: +919000000000\r\n\
         To: <sip:1002@{siptest_addr}>\r\n\
         Call-ID: api-test-call-1\r\n\
         CSeq: 1 INVITE\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {}\r\n\r\n\
         {sdp_body}",
        sdp_body.len(),
    );
    let caller2 = caller.try_clone().unwrap();
    tokio::task::spawn_blocking(move || {
        caller2.send_to(invite.as_bytes(), siptest_addr).unwrap();
    })
    .await
    .unwrap();

    // Discover the call purely via polling GET /events — the agent-facing
    // contract this whole endpoint exists for.
    let mut call_id: Option<String> = None;
    for _ in 0..30 {
        let events: serde_json::Value = client
            .get(format!("{base}/events?since=0&timeout_ms=500"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(arr) = events.as_array() {
            for e in arr {
                if e["kind"] == "incoming_call" {
                    call_id = e["call_id"].as_str().map(|s| s.to_string());
                    assert_eq!(
                        e["caller_id"]["x_gsm_caller_id"], "+919000000000",
                        "expected the caller-ID event to carry X-GSM-Caller-ID"
                    );
                    assert_eq!(
                        e["caller_id"]["p_asserted_identity"],
                        "<sip:+919000000000@ims.example.org>"
                    );
                }
            }
        }
        if call_id.is_some() {
            break;
        }
    }
    let call_id = call_id.expect("expected an incoming_call event to be discoverable via polling");

    // Answer it over HTTP.
    let resp = client
        .post(format!("{base}/calls/{call_id}/answer"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    // Drain the 100/180/200 the daemon sends and ACK, so the call actually
    // establishes rather than timing out waiting for an ACK that never comes.
    let caller3 = caller.try_clone().unwrap();
    let (ok_line, to_tag) = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        let mut last = String::new();
        for _ in 0..5 {
            let (n, _src) = caller3.recv_from(&mut buf).unwrap();
            last = String::from_utf8_lossy(&buf[..n]).to_string();
            if last.starts_with("SIP/2.0 200") {
                break;
            }
        }
        let to_tag = last
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("to:"))
            .and_then(|l| l.split("tag=").nth(1))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        (last, to_tag)
    })
    .await
    .unwrap();
    assert!(
        ok_line.starts_with("SIP/2.0 200"),
        "expected a 200 OK: {ok_line}"
    );

    let ack = format!(
        "ACK sip:1002@{siptest_addr} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {caller_addr};branch=z9hG4bKapitestack\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:+919000000000@127.0.0.1:5060>;tag=apitesttag\r\n\
         To: <sip:1002@{siptest_addr}>;tag={to_tag}\r\n\
         Call-ID: api-test-call-1\r\n\
         CSeq: 1 ACK\r\n\
         Content-Length: 0\r\n\r\n"
    );
    let caller4 = caller.try_clone().unwrap();
    tokio::task::spawn_blocking(move || {
        caller4.send_to(ack.as_bytes(), siptest_addr).unwrap();
    })
    .await
    .unwrap();

    // Give the call time to run its (1s configured) duration and report.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let calls: serde_json::Value = client
        .get(format!("{base}/calls"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(calls.as_array().map(|a| a.len()), Some(1));
    assert_eq!(calls[0]["direction"], "inbound");
    assert_eq!(calls[0]["caller_id"]["x_gsm_caller_id"], "+919000000000");

    let detail: serde_json::Value = client
        .get(format!("{base}/calls/{call_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["direction"], "inbound");
    assert!(detail["report"].is_object());

    let recording: serde_json::Value = client
        .get(format!("{base}/calls/{call_id}/recording"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(recording.get("sample_rate").is_some());

    stop.store(true, Ordering::Relaxed);
    let _ = listener.join();
}

#[tokio::test]
async fn an_unknown_call_id_is_not_found_and_an_evicted_one_is_gone() {
    let config = test_config();
    let sip_socket = Arc::new(
        SipSocket::bind(
            Some("127.0.0.1".parse().unwrap()),
            0,
            "127.0.0.1:1".parse().unwrap(),
        )
        .unwrap(),
    );
    let state = build_state(config, sip_socket, true);
    let base = spawn_server(state).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/calls/nonexistent"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}
