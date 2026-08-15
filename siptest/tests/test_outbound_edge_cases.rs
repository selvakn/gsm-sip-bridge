//! T035: `sip::outbound::place_call` driven against a scripted UDP registrar
//! standing in for the bridge's registrar stage, covering response shapes
//! the real in-process `Registrar` doesn't produce on demand — an explicit
//! refusal status, a malformed redirect, and a redirect target that never
//! answers. `test_against_registrar.rs` already covers the ordinary
//! 302-then-200 happy path (and the real registrar's own 403 for an
//! unregistered socket) against the real registrar, so neither is repeated
//! here.

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use gsm_sip_bridge::ims::sip_client::{build_uas_response, parse_datagram, SipMessage, SipRequest};

use siptest::error::SipTestError;
use siptest::media::codec::PCMU;
use siptest::sip::outbound::place_call;
use siptest::sip::socket::SipSocket;

const REALM: &str = "test-realm";
const USER: &str = "1002";
const DESTINATION: &str = "+919000000000";

fn caller_socket(registrar_addr: SocketAddr) -> SipSocket {
    SipSocket::bind(Some("127.0.0.1".parse().unwrap()), 0, registrar_addr).unwrap()
}

/// Answers exactly one INVITE with `status`/`reason` and no `Contact` —
/// stands in for the registrar's own documented refusals (403/484/503/400),
/// which `place_call` maps to a distinct named reason.
fn stub_refusing_registrar(status: u16, reason: &'static str) -> (u16, thread::JoinHandle<()>) {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let port = socket.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            let Ok((n, src)) = socket.recv_from(&mut buf) else {
                continue;
            };
            let text = String::from_utf8_lossy(&buf[..n]).to_string();
            let Ok(Some(SipMessage::Request(req))) = parse_datagram(&text) else {
                continue;
            };
            if req.method != "INVITE" {
                continue;
            }
            let resp = build_uas_response(status, reason, &req, None, None, None);
            let _ = socket.send_to(resp.as_bytes(), src);
            return;
        }
    });
    (port, handle)
}

/// Answers exactly one INVITE with a `302` carrying the given `Contact`
/// header value verbatim — lets a test hand it a well-formed redirect (to a
/// second stub) or a deliberately malformed one (no port).
fn stub_redirecting_registrar(contact: String) -> (u16, thread::JoinHandle<()>) {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let port = socket.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            let Ok((n, src)) = socket.recv_from(&mut buf) else {
                continue;
            };
            let text = String::from_utf8_lossy(&buf[..n]).to_string();
            let Ok(Some(SipMessage::Request(req))) = parse_datagram(&text) else {
                continue;
            };
            if req.method != "INVITE" {
                continue;
            }
            let resp = build_uas_response(
                302,
                "Moved Temporarily",
                &req,
                Some("redirtag"),
                Some(&contact),
                None,
            );
            let _ = socket.send_to(resp.as_bytes(), src);
            return;
        }
    });
    (port, handle)
}

/// A redirect target that never answers the re-INVITE, but records whatever
/// request it does receive (expected to be the `CANCEL` `place_call` sends
/// once its ring timeout fires) so the test can assert on it.
fn silent_redirect_target() -> (u16, Arc<Mutex<Option<SipRequest>>>, thread::JoinHandle<()>) {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let port = socket.local_addr().unwrap().port();
    let captured = Arc::new(Mutex::new(None));
    let captured2 = captured.clone();
    let handle = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let Ok((n, _src)) = socket.recv_from(&mut buf) else {
                continue;
            };
            let text = String::from_utf8_lossy(&buf[..n]).to_string();
            // Ignores the re-INVITE itself and any of its retransmissions —
            // never answers them, by design — and captures only the CANCEL
            // the ring timeout is expected to send afterwards.
            if let Ok(Some(SipMessage::Request(req))) = parse_datagram(&text) {
                if req.method == "CANCEL" {
                    *captured2.lock().unwrap() = Some(req);
                    return;
                }
            }
        }
    });
    (port, captured, handle)
}

#[test]
fn each_documented_registrar_refusal_maps_to_its_own_named_reason() {
    for (status, reason, expected) in [
        (403, "Forbidden", "untrusted_source"),
        (484, "Address Incomplete", "invalid_destination"),
        (503, "Service Unavailable", "no_idle_line"),
        (400, "Bad Request", "no_user_part"),
    ] {
        let (port, _handle) = stub_refusing_registrar(status, reason);
        let registrar_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let socket = caller_socket(registrar_addr);

        let outcome = place_call(
            &socket,
            registrar_addr,
            REALM,
            USER,
            DESTINATION,
            PCMU,
            0,
            Duration::from_secs(2),
        )
        .unwrap();

        assert!(
            !outcome.answered,
            "status {status} must not be treated as answered"
        );
        assert_eq!(outcome.final_status, status);
        assert_eq!(
            outcome.refusal_reason,
            Some(expected),
            "status {status} should map to {expected:?}, got {:?}",
            outcome.refusal_reason
        );
    }
}

/// The redirect target is taken **only** from the `302`'s `Contact` — a
/// `Contact` with no port cannot be dialled, so it must be refused as a
/// config error rather than falling back to the registrar's own port.
#[test]
fn a_302_whose_contact_carries_no_port_is_refused_not_dialled_on_the_registrars_port() {
    let (port, _handle) = stub_redirecting_registrar(format!("<sip:{DESTINATION}@127.0.0.1>"));
    let registrar_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let socket = caller_socket(registrar_addr);

    let result = place_call(
        &socket,
        registrar_addr,
        REALM,
        USER,
        DESTINATION,
        PCMU,
        0,
        Duration::from_secs(2),
    );

    match result {
        Err(SipTestError::Config(msg)) => {
            assert!(
                msg.contains("not parseable"),
                "expected a 'not parseable' config error, got: {msg}"
            );
        }
        Ok(_) => panic!("expected a Config error for the portless Contact, got Ok(..)"),
        Err(other) => panic!("expected a Config error for the portless Contact, got: {other}"),
    }
}

/// A redirect target that never answers must be abandoned at the configured
/// ring timeout with a `CANCEL` reusing the re-INVITE's own branch, and
/// reported as `487`/`ring_timeout` — not left hanging or reported as a
/// generic failure.
#[test]
fn a_ring_timeout_cancels_the_re_invite_and_is_reported_distinctly() {
    let (redirect_port, captured, _redirect_handle) = silent_redirect_target();
    let (registrar_port, _registrar_handle) =
        stub_redirecting_registrar(format!("<sip:{DESTINATION}@127.0.0.1:{redirect_port}>"));
    let registrar_addr: SocketAddr = format!("127.0.0.1:{registrar_port}").parse().unwrap();
    let socket = caller_socket(registrar_addr);

    let outcome = place_call(
        &socket,
        registrar_addr,
        REALM,
        USER,
        DESTINATION,
        PCMU,
        0,
        Duration::from_millis(500),
    )
    .unwrap();

    assert!(!outcome.answered);
    assert_eq!(outcome.final_status, 487);
    assert_eq!(outcome.refusal_reason, Some("ring_timeout"));
    assert_eq!(outcome.redirect_port, Some(redirect_port));

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let cancel = loop {
        if let Some(req) = captured.lock().unwrap().clone() {
            break req;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the redirect target should have received a CANCEL"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(cancel.method, "CANCEL");
}
