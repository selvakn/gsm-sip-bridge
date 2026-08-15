//! siptest's production registration and outbound-call code, run against the
//! bridge's **real** embedded registrar in-process.
//!
//! Only one thing here stands in for a real component: a hand-rolled "Agent
//! B" stub UAS answers the re-INVITE the registrar's `302` redirects to,
//! instead of pjsua. That is the constitution's sanctioned carve-out — pjsua
//! lives entirely behind the `pjsip-linked` feature, which neither `make
//! test` nor CI ever compiles, so it cannot be "the real component" in this
//! suite regardless of how the test is written. The registrar itself is not
//! stubbed: it is the genuine `gsm_sip_bridge::sip::server::Registrar`,
//! started on a real loopback socket, and siptest's registration/outbound
//! code runs unmodified against it.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gsm_sip_bridge::config::secret::Secret;
use gsm_sip_bridge::config::{SipServerAccount, SipServerConfig};
use gsm_sip_bridge::ims::sip_client::{
    build_100_trying, build_180_ringing, build_200_ok_invite, parse_datagram, SipMessage,
};
use gsm_sip_bridge::sip::server::Registrar;

use siptest::media::codec::PCMU;
use siptest::sip::registration::{register, RegistrationConfig, RegistrationCredentials};
use siptest::sip::socket::SipSocket;

const USER: &str = "1002";
const PASSWORD: &str = "hunter2";
const REALM: &str = "test-realm";

/// A minimal Agent B stand-in: answers exactly one INVITE with `100`/`180`/
/// `200` and echoes RTP back with a fixed delay, so a test can assert on the
/// round-trip properties of a *real* answered call without pjsua.
struct StubUas {
    sip_port: u16,
    rtp_port: u16,
    stop: Arc<AtomicBool>,
    sip_handle: Option<thread::JoinHandle<()>>,
    rtp_handle: Option<thread::JoinHandle<()>>,
}

impl StubUas {
    fn start() -> Self {
        let sip_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        sip_socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let sip_port = sip_socket.local_addr().unwrap().port();

        let rtp_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        rtp_socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let rtp_port = rtp_socket.local_addr().unwrap().port();

        let stop = Arc::new(AtomicBool::new(false));

        let sip_handle = {
            let stop = stop.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while !stop.load(Ordering::Relaxed) {
                    let Ok((n, src)) = sip_socket.recv_from(&mut buf) else {
                        continue;
                    };
                    let text = String::from_utf8_lossy(&buf[..n]).to_string();
                    let Ok(Some(SipMessage::Request(req))) = parse_datagram(&text) else {
                        continue;
                    };
                    if req.method != "INVITE" {
                        continue;
                    }
                    let _ = sip_socket.send_to(build_100_trying(&req).as_bytes(), src);
                    let to_tag = "stubtag";
                    let contact = format!("sip:agentb@127.0.0.1:{sip_port}");
                    let _ = sip_socket
                        .send_to(build_180_ringing(&req, to_tag, &contact).as_bytes(), src);
                    thread::sleep(Duration::from_millis(20));
                    let sdp = format!(
                        "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {rtp_port} RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n"
                    );
                    let _ = sip_socket.send_to(
                        build_200_ok_invite(&req, to_tag, &contact, &sdp).as_bytes(),
                        src,
                    );
                    // Best-effort ACK drain so the socket doesn't accumulate it as an
                    // unrelated "INVITE" on the next loop iteration.
                    let _ = sip_socket.recv_from(&mut buf);
                }
            })
        };

        let rtp_handle = {
            let stop = stop.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 2048];
                while !stop.load(Ordering::Relaxed) {
                    if let Ok((n, src)) = rtp_socket.recv_from(&mut buf) {
                        // A fixed delay, deliberately, so a future RTT assertion has
                        // ground truth to check against.
                        thread::sleep(Duration::from_millis(20));
                        let _ = rtp_socket.send_to(&buf[..n], src);
                    }
                }
            })
        };

        Self {
            sip_port,
            rtp_port,
            stop,
            sip_handle: Some(sip_handle),
            rtp_handle: Some(rtp_handle),
        }
    }
}

impl Drop for StubUas {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.sip_handle.take() {
            let _ = h.join();
        }
        if let Some(h) = self.rtp_handle.take() {
            let _ = h.join();
        }
    }
}

fn server_config() -> SipServerConfig {
    SipServerConfig {
        enabled: true,
        listen_addr: "127.0.0.1".to_string(),
        listen_port: 0,
        realm: REALM.to_string(),
        ring_aor: USER.to_string(),
        min_expires: 60,
        max_expires: 3600,
        nonce_lifetime_sec: 120,
        accounts: vec![SipServerAccount {
            username: USER.to_string(),
            password: Secret::new(PASSWORD.to_string()),
        }],
    }
}

#[test]
fn siptest_registers_places_a_call_through_a_302_redirect_and_carries_bothways_audio() {
    let stub = StubUas::start();

    let registrar_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let registrar_addr = registrar_socket.local_addr().unwrap();
    let _registrar =
        Registrar::start_on_with_outbound(registrar_socket, &server_config(), stub.sip_port)
            .expect("start registrar");

    let sip_socket =
        SipSocket::bind(Some("127.0.0.1".parse().unwrap()), 0, registrar_addr).unwrap();

    let reg_config = RegistrationConfig {
        registrar_addr,
        registrar_host: REALM.to_string(),
        aor_user: USER.to_string(),
        realm: REALM.to_string(),
        password: Secret::new(PASSWORD.to_string()),
        expires: 300,
    };
    let mut creds = RegistrationCredentials {
        cseq: 0,
        call_id: "reg-call-id".to_string(),
        from_tag: "reg-from-tag".to_string(),
        cached_nonce: None,
        nc: 0,
    };

    let status = register(&sip_socket, &reg_config, &mut creds).unwrap();
    assert_eq!(
        status.state,
        siptest::sip::registration::RegState::Registered,
        "expected registration to succeed against the real registrar: {:?}",
        status.last_status
    );

    let outcome = siptest::sip::outbound::place_call(
        &sip_socket,
        registrar_addr,
        REALM,
        USER,
        "+919000000000",
        PCMU,
        0,
        Duration::from_secs(5),
    )
    .expect("place_call should not error");

    assert!(
        outcome.answered,
        "expected the stub UAS to answer: {:?}",
        outcome.final_status
    );
    assert_eq!(
        outcome.redirect_port,
        Some(stub.sip_port),
        "expected the 302 to point at the stub UAS"
    );

    let sdp_answer = outcome
        .sdp_answer
        .as_ref()
        .expect("answered call carries an SDP answer");
    assert_eq!(sdp_answer.remote_rtp.port(), stub.rtp_port);

    // Now drive real media over the loopback echo and confirm the
    // packet-count verdict reads BothWays end to end.
    let local_rtp: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let result = siptest::media::session::run(
        siptest::media::session::MediaSessionConfig {
            local_rtp,
            remote_rtp: sdp_answer.remote_rtp,
            codec: PCMU,
            duration: Duration::from_millis(600),
            sent_wav_path: None,
            received_wav_path: None,
        },
        stop,
    )
    .unwrap();

    assert!(
        result.sent_packets > 10,
        "expected several packets sent, got {}",
        result.sent_packets
    );
    assert!(
        result.receive_stats.received_packets > 0,
        "expected the stub's echo to produce received packets"
    );

    let verdict = gsm_sip_bridge::ims::media_stats::verdict(
        result.sent_packets,
        result.receive_stats.received_packets,
        gsm_sip_bridge::ims::media_stats::DEFAULT_ONE_WAY_THRESHOLD_PERCENT,
    );
    assert_eq!(
        verdict,
        gsm_sip_bridge::ims::media_stats::DirectionVerdict::BothWays
    );

    if let Some(dialog) = &outcome.dialog {
        let _ = siptest::sip::outbound::send_bye(&sip_socket, dialog);
    }
}

/// research.md R2 / contracts/sip-flows.md C-0: an INVITE sent from a socket
/// other than the one that REGISTERed must be refused, because the registrar
/// authorises outbound dialling by matching the request's source address
/// against the binding created by REGISTER.
#[test]
fn an_invite_from_a_different_socket_than_the_registered_one_is_refused() {
    let stub = StubUas::start();

    let registrar_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let registrar_addr = registrar_socket.local_addr().unwrap();
    let _registrar =
        Registrar::start_on_with_outbound(registrar_socket, &server_config(), stub.sip_port)
            .expect("start registrar");

    let registering_socket =
        SipSocket::bind(Some("127.0.0.1".parse().unwrap()), 0, registrar_addr).unwrap();
    let reg_config = RegistrationConfig {
        registrar_addr,
        registrar_host: REALM.to_string(),
        aor_user: USER.to_string(),
        realm: REALM.to_string(),
        password: Secret::new(PASSWORD.to_string()),
        expires: 300,
    };
    let mut creds = RegistrationCredentials {
        cseq: 0,
        call_id: "reg-call-id-2".to_string(),
        from_tag: "reg-from-tag-2".to_string(),
        cached_nonce: None,
        nc: 0,
    };
    let status = register(&registering_socket, &reg_config, &mut creds).unwrap();
    assert_eq!(
        status.state,
        siptest::sip::registration::RegState::Registered
    );

    // A second, never-registered socket attempts the INVITE.
    let other_socket =
        SipSocket::bind(Some("127.0.0.1".parse().unwrap()), 0, registrar_addr).unwrap();
    let outcome = siptest::sip::outbound::place_call(
        &other_socket,
        registrar_addr,
        REALM,
        USER,
        "+919000000000",
        PCMU,
        0,
        Duration::from_secs(5),
    )
    .expect("place_call should not error");

    assert!(!outcome.answered);
    assert_eq!(outcome.final_status, 403);
    assert_eq!(outcome.refusal_reason, Some("untrusted_source"));
}
