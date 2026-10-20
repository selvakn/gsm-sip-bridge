//! Spec 099, research R1: does a real PJSIP hand the disconnect callback the
//! PBX's failure response, with its `Reason: Q.850;cause=N` header readable?
//!
//! A fake PBX on a loopback UDP socket answers each INVITE with a failure
//! final response chosen by the request-URI user, and the test reads what
//! `Call::last_disconnect` captured. Needs a real linked PJSIP, so it is
//! `pjsip-linked`-gated. One test function only: PJSUA's endpoint is a
//! process-global singleton.
#![cfg(feature = "pjsip-linked")]

use pjsua_safe::{Account, Call, CallState, Disconnect, Endpoint, EndpointConfig, TransportType};
use std::net::UdpSocket;
use std::time::{Duration, Instant};

/// The endpoint's own port, distinct from the ones other linked tests in
/// this crate bind (15060-15092). The fake PBX takes an OS-assigned port, so
/// parallel test binaries (nextest) cannot collide with it.
const ENDPOINT_PORT: u16 = 15101;

fn ep_config() -> EndpointConfig {
    EndpointConfig {
        transport: TransportType::Udp,
        local_port: ENDPOINT_PORT,
        tls_verify: false,
        clock_rate: 16000,
        jb_init_ms: 20,
        jb_min_pre: 1,
        jb_max_ms: 40,
        vad_enabled: true,
        tx_level: 1.0,
        snd_rec_latency_ms: 150,
        snd_play_latency_ms: 150,
        public_addr: None,
    }
}

/// The failure final response (status line and extra headers) the fake PBX
/// sends for a request-URI user.
fn response_for(user: &str) -> (&'static str, &'static str) {
    match user {
        "freepbx" => ("403 Forbidden", "Reason: Q.850;cause=21\r\n"),
        "bare403" => ("403 Forbidden", ""),
        "busy-cause" => (
            "503 Service Unavailable",
            "Reason: Q.850;cause=17;text=\"User busy\"\r\n",
        ),
        // Several Reason headers: the Q.850 one is not the first.
        "two-reasons" => (
            "503 Service Unavailable",
            "Reason: SIP;cause=503;text=\"Service Unavailable\"\r\nReason: Q.850;cause=21\r\n",
        ),
        // Both reasons in one header line, SIP first (RFC 3326).
        "combined" => ("403 Forbidden", "Reason: SIP;cause=403, Q.850;cause=21\r\n"),
        "sip-only" => ("403 Forbidden", "Reason: SIP;cause=403\r\n"),
        "not-failure" => ("486 Busy Here", "Reason: Q.850;cause=16\r\n"),
        other => panic!("unknown scenario {other}"),
    }
}

fn header<'a>(request: &'a str, name: &str) -> Vec<&'a str> {
    request
        .lines()
        .filter_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.trim().eq_ignore_ascii_case(name).then_some(v.trim())
        })
        .collect()
}

/// Answer every INVITE with its scenario's failure; ACKs and the rest are
/// ignored. Runs until the process ends.
fn spawn_fake_pbx() -> u16 {
    let socket = UdpSocket::bind(("127.0.0.1", 0)).expect("bind fake pbx");
    let port = socket.local_addr().expect("fake pbx address").port();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok((n, from)) = socket.recv_from(&mut buf) {
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            if !request.starts_with("INVITE ") {
                continue;
            }
            let uri = request.split_whitespace().nth(1).unwrap_or("");
            let user = uri
                .trim_start_matches("sip:")
                .split('@')
                .next()
                .unwrap_or("");
            let (status, extra) = response_for(user);
            let mut reply = format!("SIP/2.0 {status}\r\n");
            for via in header(&request, "Via") {
                reply.push_str(&format!("Via: {via}\r\n"));
            }
            reply.push_str(&format!("From: {}\r\n", header(&request, "From")[0]));
            reply.push_str(&format!(
                "To: {};tag=fakepbx\r\n",
                header(&request, "To")[0]
            ));
            reply.push_str(&format!("Call-ID: {}\r\n", header(&request, "Call-ID")[0]));
            reply.push_str(&format!("CSeq: {}\r\n", header(&request, "CSeq")[0]));
            reply.push_str(extra);
            reply.push_str("Content-Length: 0\r\n\r\n");
            let _ = socket.send_to(reply.as_bytes(), from);
        }
    });
    port
}

fn disconnect_of(account: &Account, pbx_port: u16, user: &str) -> Disconnect {
    let call = Call::make(
        account,
        &format!("sip:{user}@127.0.0.1:{pbx_port}"),
        None,
        &[],
    )
    .expect("call make");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if call.poll_state() == CallState::Disconnected {
            return call
                .last_disconnect()
                .expect("a placed call that disconnected has a capture");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("{user}: call did not disconnect within 5s");
}

#[test]
fn a_failure_response_reason_cause_is_captured_from_a_real_pjsip_call() {
    let pbx = spawn_fake_pbx();
    let ep = Endpoint::create(ep_config()).expect("endpoint create");
    ep.set_null_sound_device().expect("null sound device");
    let account = Account::local(
        &ep,
        &format!("sip:bridge@127.0.0.1:{ENDPOINT_PORT}"),
        "Bridge",
    )
    .expect("account");

    let seen = |status, q850| Disconnect {
        status: Some(status),
        q850,
        response_seen: true,
    };
    // The reported FreePBX case.
    assert_eq!(disconnect_of(&account, pbx, "freepbx"), seen(403, Some(21)));
    assert_eq!(disconnect_of(&account, pbx, "bare403"), seen(403, None));
    // Parameters after the cause are tolerated.
    assert_eq!(
        disconnect_of(&account, pbx, "busy-cause"),
        seen(503, Some(17))
    );
    // A SIP-protocol Reason first, the Q.850 one second.
    assert_eq!(
        disconnect_of(&account, pbx, "two-reasons"),
        seen(503, Some(21))
    );
    assert_eq!(
        disconnect_of(&account, pbx, "combined"),
        seen(403, Some(21))
    );
    assert_eq!(disconnect_of(&account, pbx, "sip-only"), seen(403, None));
    assert_eq!(
        disconnect_of(&account, pbx, "not-failure"),
        seen(486, Some(16))
    );
}
