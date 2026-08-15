//! T050: `sip::registration::register` driven against a scripted UDP
//! registrar, covering the specific response sequences the real in-process
//! `Registrar` never produces on demand (`423 Interval Too Brief`, a second
//! `401` after the client has already authorised, an unrecognised final
//! status) — `test_against_registrar.rs` already covers the ordinary
//! 401-then-200 happy path against the real registrar, so it is not
//! repeated here.

use std::net::{SocketAddr, UdpSocket};
use std::thread;
use std::time::Duration;

use gsm_sip_bridge::config::secret::Secret;
use gsm_sip_bridge::ims::sip_client::{
    build_uas_response_with_headers, parse_datagram, SipMessage,
};

use siptest::sip::registration::{register, RegState, RegistrationConfig, RegistrationCredentials};
use siptest::sip::socket::SipSocket;

const USER: &str = "1002";
const PASSWORD: &str = "hunter2";
const REALM: &str = "test-realm";

const CHALLENGE: (&str, &str) = (
    "WWW-Authenticate",
    "Digest realm=\"test-realm\", nonce=\"n1\", qop=\"auth\", algorithm=MD5",
);

type ScriptedResponse = (u16, &'static str, Vec<(&'static str, &'static str)>);

/// Answers each REGISTER it receives with the next `(status, reason, extra
/// headers)` in `script`, in order, then stops responding once the script is
/// exhausted — `register()`'s own retry budget (3 attempts) is what bounds
/// how many of those unanswered follow-ups actually get sent.
fn scripted_registrar(script: Vec<ScriptedResponse>) -> (u16, thread::JoinHandle<()>) {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let port = socket.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        for (status, reason, extra) in script {
            loop {
                let Ok((n, src)) = socket.recv_from(&mut buf) else {
                    continue;
                };
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let Ok(Some(SipMessage::Request(req))) = parse_datagram(&text) else {
                    continue;
                };
                if req.method != "REGISTER" {
                    continue;
                }
                let resp =
                    build_uas_response_with_headers(status, reason, &req, None, None, None, &extra);
                let _ = socket.send_to(resp.as_bytes(), src);
                break;
            }
        }
    });
    (port, handle)
}

fn dial(port: u16) -> (SipSocket, RegistrationConfig, RegistrationCredentials) {
    let registrar_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let socket = SipSocket::bind(Some("127.0.0.1".parse().unwrap()), 0, registrar_addr).unwrap();
    let cfg = RegistrationConfig {
        registrar_addr,
        registrar_host: REALM.to_string(),
        aor_user: USER.to_string(),
        realm: REALM.to_string(),
        password: Secret::new(PASSWORD.to_string()),
        expires: 300,
    };
    let creds = RegistrationCredentials {
        cseq: 0,
        call_id: "edge-case-call-id".to_string(),
        from_tag: "edge-case-from-tag".to_string(),
        cached_nonce: None,
        nc: 0,
    };
    (socket, cfg, creds)
}

/// research.md / registration.rs's own doc comment: `423 Interval Too Brief`
/// means adopt the registrar's `Min-Expires` and retry with it, not fail —
/// and the retried REGISTER still needs to clear a normal digest challenge
/// before it succeeds.
#[test]
fn interval_too_brief_adopts_min_expires_and_then_completes_after_digest() {
    let (port, _handle) = scripted_registrar(vec![
        (423, "Interval Too Brief", vec![("Min-Expires", "60")]),
        (401, "Unauthorized", vec![CHALLENGE]),
        (200, "OK", vec![("Expires", "60")]),
    ]);
    let (socket, cfg, mut creds) = dial(port);

    let status = register(&socket, &cfg, &mut creds).unwrap();

    assert_eq!(status.state, RegState::Registered);
    assert_eq!(status.granted_expires, Some(60));
}

/// registration.rs's own contract: a second `401` on an already-authorised
/// REGISTER is a hard failure, never a retry loop — otherwise a registrar
/// stuck rejecting valid credentials would spin forever.
#[test]
fn a_second_401_after_authorising_is_a_hard_failure_not_a_retry_loop() {
    let (port, _handle) = scripted_registrar(vec![
        (401, "Unauthorized", vec![CHALLENGE]),
        (401, "Unauthorized", vec![CHALLENGE]),
    ]);
    let (socket, cfg, mut creds) = dial(port);

    let status = register(&socket, &cfg, &mut creds).unwrap();

    assert_eq!(status.state, RegState::Failed);
    assert_eq!(status.consecutive_failures, 1);
    assert_eq!(
        status.last_status.as_ref().map(|(code, _)| *code),
        Some(401)
    );
}

/// Any other final status (not 200/401/423) is reported as a failure
/// carrying that exact status and reason, not translated into something
/// generic.
#[test]
fn an_unrecognised_final_status_is_reported_verbatim() {
    let (port, _handle) = scripted_registrar(vec![(500, "Server Internal Error", vec![])]);
    let (socket, cfg, mut creds) = dial(port);

    let status = register(&socket, &cfg, &mut creds).unwrap();

    assert_eq!(status.state, RegState::Failed);
    assert_eq!(
        status.last_status,
        Some((500, "Server Internal Error".to_string()))
    );
}
