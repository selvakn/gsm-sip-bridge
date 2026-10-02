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
use gsm_sip_bridge::config::{SipServerAccount, SipServerConfig, SipServerDialMode};
use gsm_sip_bridge::ims::sip_client::{
    build_100_trying, build_180_ringing, build_200_ok_invite, build_uas_response_with_headers,
    parse_datagram, SipMessage, SipRequest,
};
use gsm_sip_bridge::sip::server::{OutboundDial, Registrar};

use siptest::media::codec::{resolve_codec, PCMU};
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
    /// The SDP body of the last INVITE this stub received — lets a test
    /// assert on what codec was actually offered on the wire, not just
    /// whether the call was answered.
    last_offer: Arc<std::sync::Mutex<Option<String>>>,
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
        let last_offer = Arc::new(std::sync::Mutex::new(None));

        let sip_handle = {
            let stop = stop.clone();
            let last_offer = last_offer.clone();
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
                    *last_offer.lock().unwrap() = Some(req.body.clone());
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
            last_offer,
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
    let _registrar = Registrar::start_on_with_outbound(
        registrar_socket,
        &server_config(),
        OutboundDial {
            port: stub.sip_port,
            mode: SipServerDialMode::Redirect,
        },
    )
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
            tone_enabled: false,
            play: None,
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

/// The default (proxy) path, end to end: siptest INVITEs the real registrar,
/// which relays to the stub UAS; the answer and a direct ACK/BYE follow with
/// the registrar out of the dialog, and audio flows both ways. Covers what the
/// `302` test above does for redirect mode.
#[test]
fn siptest_places_a_call_through_the_proxy_and_carries_bothways_audio() {
    let stub = StubUas::start();

    let registrar_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let registrar_addr = registrar_socket.local_addr().unwrap();
    let _registrar = Registrar::start_on_with_outbound(
        registrar_socket,
        &server_config(),
        OutboundDial {
            port: stub.sip_port,
            mode: SipServerDialMode::Proxy,
        },
    )
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
        call_id: "reg-call-id-proxy".to_string(),
        from_tag: "reg-from-tag-proxy".to_string(),
        cached_nonce: None,
        nc: 0,
    };
    let status = register(&sip_socket, &reg_config, &mut creds).unwrap();
    assert_eq!(
        status.state,
        siptest::sip::registration::RegState::Registered
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
        "expected the relayed answer: {:?}",
        outcome.final_status
    );
    assert_eq!(outcome.redirect_port, None, "no redirect in proxy mode");
    assert!(
        outcome.invite_to_180_ms.is_some(),
        "the relayed 180 must be timed"
    );
    // The stub's Contact is `sip:agentb@...`: in-dialog requests use that
    // user, not the dialled number.
    assert_eq!(
        outcome.dialog.as_ref().map(|d| d.target_user.as_str()),
        Some("agentb")
    );
    // The ACK/BYE target is the stub's own Contact, not the registrar.
    assert_eq!(
        outcome.remote_target.map(|a| a.port()),
        Some(stub.sip_port),
        "the dialog must go direct to the dial-out account"
    );
    let sdp_answer = outcome.sdp_answer.as_ref().expect("SDP answer");
    assert_eq!(sdp_answer.remote_rtp.port(), stub.rtp_port);

    let stop = Arc::new(AtomicBool::new(false));
    let result = siptest::media::session::run(
        siptest::media::session::MediaSessionConfig {
            local_rtp: "127.0.0.1:0".parse().unwrap(),
            remote_rtp: sdp_answer.remote_rtp,
            codec: PCMU,
            duration: Duration::from_millis(600),
            sent_wav_path: None,
            received_wav_path: None,
            tone_enabled: false,
            play: None,
        },
        stop,
    )
    .unwrap();
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
        siptest::sip::outbound::send_bye(&sip_socket, dialog).expect("BYE goes direct");
    }
}

/// A proxied call nobody answers must be CANCELled when siptest gives up, or
/// the dial-out account keeps ringing for a caller who has left.
#[test]
fn siptest_cancels_a_proxied_call_it_gives_up_on() {
    let account = UdpSocket::bind("127.0.0.1:0").unwrap();
    account
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let registrar_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let registrar_addr = registrar_socket.local_addr().unwrap();
    let _registrar = Registrar::start_on_with_outbound(
        registrar_socket,
        &server_config(),
        OutboundDial {
            port: account.local_addr().unwrap().port(),
            mode: SipServerDialMode::Proxy,
        },
    )
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
        call_id: "reg-call-id-cancel".to_string(),
        from_tag: "reg-from-tag-cancel".to_string(),
        cached_nonce: None,
        nc: 0,
    };
    register(&sip_socket, &reg_config, &mut creds).unwrap();

    let outcome = siptest::sip::outbound::place_call(
        &sip_socket,
        registrar_addr,
        REALM,
        USER,
        "+919000000000",
        PCMU,
        0,
        Duration::from_secs(1),
    )
    .expect("place_call should not error");
    assert!(!outcome.answered);

    let mut buf = [0u8; 4096];
    let mut methods = Vec::new();
    while let Ok((n, _)) = account.recv_from(&mut buf) {
        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
        methods.push(text.split(' ').next().unwrap_or("").to_string());
        if methods.last().map(String::as_str) == Some("CANCEL") {
            break;
        }
    }
    assert_eq!(methods, ["INVITE", "CANCEL"], "got: {methods:?}");
}

/// T081: `resolve_codec("g722")` is not just a library-level lookup — its
/// result is what actually goes out on the wire. Places a real call through
/// the real registrar's 302 dance and asserts the INVITE the stub UAS
/// received named PT 9 / G722, not PCMU's PT 0.
#[test]
fn siptest_offers_g722_on_the_wire_when_the_g722_codec_is_selected() {
    let stub = StubUas::start();

    let registrar_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let registrar_addr = registrar_socket.local_addr().unwrap();
    let _registrar = Registrar::start_on_with_outbound(
        registrar_socket,
        &server_config(),
        OutboundDial {
            port: stub.sip_port,
            mode: SipServerDialMode::Redirect,
        },
    )
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
        call_id: "reg-call-id-g722".to_string(),
        from_tag: "reg-from-tag-g722".to_string(),
        cached_nonce: None,
        nc: 0,
    };
    let status = register(&sip_socket, &reg_config, &mut creds).unwrap();
    assert_eq!(
        status.state,
        siptest::sip::registration::RegState::Registered
    );

    let codec = resolve_codec("g722").expect("g722 is a known codec name");

    let outcome = siptest::sip::outbound::place_call(
        &sip_socket,
        registrar_addr,
        REALM,
        USER,
        "+919000000000",
        codec,
        0,
        Duration::from_secs(5),
    )
    .expect("place_call should not error");

    // The stub answers PCMU to this G.722 offer, which siptest now reports as a
    // negotiation failure; what this test is about is the offer on the wire.
    assert_eq!(outcome.refusal_reason, Some("codec_mismatch"));

    let offer = stub
        .last_offer
        .lock()
        .unwrap()
        .clone()
        .expect("stub UAS should have captured the re-INVITE's SDP body");
    assert!(
        offer.contains("RTP/AVP 9"),
        "expected the offer to name PT 9 for G.722, got: {offer}"
    );
    assert!(
        offer.contains("a=rtpmap:9 G722/8000"),
        "expected the offer's rtpmap to name G722/8000, got: {offer}"
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
    let _registrar = Registrar::start_on_with_outbound(
        registrar_socket,
        &server_config(),
        OutboundDial {
            port: stub.sip_port,
            mode: SipServerDialMode::Redirect,
        },
    )
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

/// T051: the registrar bouncing mid-session (a real restart, not merely a
/// dropped packet) must not leave siptest permanently deregistered — the
/// same `register()` call `daemon::registration_loop`'s refresh timer would
/// make on its next cycle has to succeed again once a registrar is back,
/// carrying the same credentials forward rather than needing a fresh dialog.
/// (The other half of T051 — advancing a fake clock to prove the *timer*
/// itself fires on schedule — would need an injectable clock seam that
/// doesn't exist anywhere in this crate; adding one is new production code,
/// not a test, so it is left out rather than faked.)
#[test]
fn registration_recovers_after_the_registrar_is_stopped_and_restarted_on_the_same_port() {
    let registrar_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let registrar_addr = registrar_socket.local_addr().unwrap();
    let mut registrar = Registrar::start_on(registrar_socket, &server_config()).unwrap();

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
        call_id: "reg-call-id-restart".to_string(),
        from_tag: "reg-from-tag-restart".to_string(),
        cached_nonce: None,
        nc: 0,
    };

    let first = register(&sip_socket, &reg_config, &mut creds).unwrap();
    assert_eq!(
        first.state,
        siptest::sip::registration::RegState::Registered,
        "expected the initial registration to succeed: {:?}",
        first.last_status
    );

    // A real restart: stop the old registrar (frees the port), then bind a
    // brand new one on the exact same address — a fresh `Registrar` with an
    // empty binding table, standing in for a process restart rather than a
    // network blip.
    registrar.stop();
    let restarted_socket = UdpSocket::bind(registrar_addr).unwrap();
    let _registrar2 = Registrar::start_on(restarted_socket, &server_config()).unwrap();

    let second = register(&sip_socket, &reg_config, &mut creds).unwrap();
    assert_eq!(
        second.state,
        siptest::sip::registration::RegState::Registered,
        "expected re-registration against the restarted registrar to succeed: {:?}",
        second.last_status
    );
}

// ---------------------------------------------------------------------------
// A scripted dial-out account, for the signalling edge cases: what siptest
// does when the far end answers late, refuses, sends odd provisionals, or
// advertises a Contact with URI parameters. Everything on the registrar side
// is the real `Registrar` in proxy mode.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Behavior {
    /// Answers `status` (an error) with a To tag, and logs the ACK.
    Reject(u16),
    /// Rings, then — only once CANCELled — answers the INVITE with a `200`
    /// anyway: the answer that crosses the CANCEL on the wire.
    AnswerCrossingCancel,
    /// `181`, `183` at once, `180` after 300 ms, `200` after another 300 ms.
    SlowProgress,
    /// `200` straight away with a Contact carrying URI parameters.
    ParamContact,
}

struct ScriptedUas {
    port: u16,
    /// Method of every request received, in order.
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

const SCRIPT_SDP: &str = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 40000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

impl ScriptedUas {
    fn start(behavior: Behavior) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let port = socket.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let seen = seen.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let mut invite: Option<(SipRequest, std::net::SocketAddr)> = None;
                let contact = match behavior {
                    Behavior::ParamContact => {
                        format!("<sip:agentb@127.0.0.1:{port};transport=udp>")
                    }
                    _ => format!("<sip:agentb@127.0.0.1:{port}>"),
                };
                let reply = |status: u16,
                             reason: &str,
                             req: &SipRequest,
                             to: std::net::SocketAddr,
                             body: Option<&str>| {
                    let msg = build_uas_response_with_headers(
                        status,
                        reason,
                        req,
                        Some("scripted"),
                        Some(&contact),
                        body,
                        &[],
                    );
                    let _ = socket.send_to(msg.as_bytes(), to);
                };
                while !stop.load(Ordering::Relaxed) {
                    let Ok((n, src)) = socket.recv_from(&mut buf) else {
                        continue;
                    };
                    let text = String::from_utf8_lossy(&buf[..n]).to_string();
                    let Ok(Some(SipMessage::Request(req))) = parse_datagram(&text) else {
                        continue;
                    };
                    seen.lock().unwrap().push(req.method.clone());
                    match req.method.as_str() {
                        "INVITE" => {
                            reply(100, "Trying", &req, src, None);
                            match behavior {
                                Behavior::Reject(code) => reply(code, "Refused", &req, src, None),
                                Behavior::AnswerCrossingCancel => {
                                    reply(180, "Ringing", &req, src, None)
                                }
                                Behavior::SlowProgress => {
                                    reply(181, "Call Is Being Forwarded", &req, src, None);
                                    reply(183, "Session Progress", &req, src, None);
                                    thread::sleep(Duration::from_millis(300));
                                    reply(180, "Ringing", &req, src, None);
                                    thread::sleep(Duration::from_millis(300));
                                    reply(200, "OK", &req, src, Some(SCRIPT_SDP));
                                }
                                Behavior::ParamContact => {
                                    reply(200, "OK", &req, src, Some(SCRIPT_SDP))
                                }
                            }
                            invite = Some((req, src));
                        }
                        "CANCEL" => {
                            if let Some((inv, inv_src)) = &invite {
                                if matches!(behavior, Behavior::AnswerCrossingCancel) {
                                    reply(200, "OK", inv, *inv_src, Some(SCRIPT_SDP));
                                }
                            }
                            reply(200, "OK", &req, src, None);
                        }
                        "BYE" => reply(200, "OK", &req, src, None),
                        _ => {}
                    }
                }
            })
        };
        Self {
            port,
            seen,
            stop,
            handle: Some(handle),
        }
    }

    /// Waits up to 5 s for `method` to have been received.
    fn wait_for(&self, method: &str) -> bool {
        for _ in 0..50 {
            if self.seen.lock().unwrap().iter().any(|m| m == method) {
                return true;
            }
            thread::sleep(Duration::from_millis(100));
        }
        false
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for ScriptedUas {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// A real proxy-mode registrar in front of `uas`, with a phone registered.
fn proxy_rig(uas: &ScriptedUas) -> (Registrar, SipSocket, std::net::SocketAddr) {
    let registrar_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let registrar_addr = registrar_socket.local_addr().unwrap();
    let registrar = Registrar::start_on_with_outbound(
        registrar_socket,
        &server_config(),
        OutboundDial {
            port: uas.port,
            mode: SipServerDialMode::Proxy,
        },
    )
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
        call_id: "reg-call-id-scripted".to_string(),
        from_tag: "reg-from-tag-scripted".to_string(),
        cached_nonce: None,
        nc: 0,
    };
    let status = register(&sip_socket, &reg_config, &mut creds).unwrap();
    assert_eq!(
        status.state,
        siptest::sip::registration::RegState::Registered
    );
    (registrar, sip_socket, registrar_addr)
}

fn dial(
    socket: &SipSocket,
    registrar_addr: std::net::SocketAddr,
    ring_timeout: Duration,
) -> siptest::sip::outbound::OutboundCallOutcome {
    siptest::sip::outbound::place_call(
        socket,
        registrar_addr,
        REALM,
        USER,
        "+919000000000",
        PCMU,
        0,
        ring_timeout,
    )
    .expect("place_call should not error")
}

/// The answer that crosses the CANCEL: siptest must ACK it and hang it up, or
/// a real carrier call stays connected to nobody.
#[test]
fn a_200_that_crosses_our_cancel_is_acked_and_hung_up() {
    let uas = ScriptedUas::start(Behavior::AnswerCrossingCancel);
    let (_registrar, socket, registrar_addr) = proxy_rig(&uas);

    let outcome = dial(&socket, registrar_addr, Duration::from_secs(1));
    assert!(!outcome.answered, "the caller had already given up");

    assert!(
        uas.wait_for("BYE"),
        "late answer never hung up: {:?}",
        uas.seen()
    );
    let seen = uas.seen();
    let pos = |m: &str| seen.iter().position(|x| x == m).unwrap();
    assert!(
        pos("CANCEL") < pos("ACK") && pos("ACK") < pos("BYE"),
        "expected CANCEL, then ACK, then BYE: {seen:?}"
    );
}

/// A refusal is ACKed (hop-by-hop, through the relay) — otherwise the far end
/// retransmits it for ~32 s.
#[test]
fn a_486_is_acked() {
    let uas = ScriptedUas::start(Behavior::Reject(486));
    let (_registrar, socket, registrar_addr) = proxy_rig(&uas);

    let outcome = dial(&socket, registrar_addr, Duration::from_secs(5));
    assert!(!outcome.answered);
    assert_eq!(outcome.final_status, 486);
    assert!(uas.wait_for("ACK"), "refusal never ACKed: {:?}", uas.seen());
}

/// 181 and 183 are provisional, not failures, and only a 180 is "ringing".
#[test]
fn provisional_responses_are_not_failures_and_only_a_180_counts_as_ringing() {
    let uas = ScriptedUas::start(Behavior::SlowProgress);
    let (_registrar, socket, registrar_addr) = proxy_rig(&uas);

    let outcome = dial(&socket, registrar_addr, Duration::from_secs(5));
    assert!(
        outcome.answered,
        "181/183 ended the call: status {}",
        outcome.final_status
    );
    let ringing = outcome.invite_to_180_ms.expect("a 180 arrived");
    let answered = outcome.invite_to_200_ms.expect("answered");
    assert!(
        ringing >= 250,
        "ringing timed from the 183 sent at t=0, not the 180: {ringing} ms"
    );
    assert!(answered > ringing);
    if let Some(dialog) = &outcome.dialog {
        let _ = siptest::sip::outbound::send_bye(&socket, dialog);
    }
}

/// `Contact: <sip:agentb@host:port;transport=udp>` is a valid Contact: the ACK
/// and BYE must reach the account itself, not the registrar.
#[test]
fn a_contact_with_uri_parameters_still_routes_the_ack_and_bye_to_the_account() {
    let uas = ScriptedUas::start(Behavior::ParamContact);
    let (_registrar, socket, registrar_addr) = proxy_rig(&uas);

    let outcome = dial(&socket, registrar_addr, Duration::from_secs(5));
    assert!(outcome.answered);
    assert_eq!(outcome.remote_target.map(|a| a.port()), Some(uas.port));
    assert!(
        uas.wait_for("ACK"),
        "ACK missed the account: {:?}",
        uas.seen()
    );

    let dialog = outcome.dialog.expect("dialog");
    siptest::sip::outbound::send_bye(&socket, &dialog).unwrap();
    assert!(
        uas.wait_for("BYE"),
        "BYE missed the account: {:?}",
        uas.seen()
    );
}

/// An answer in a different codec than offered is a negotiation failure, said
/// as such — and the call is hung up, not left connected.
#[test]
fn an_answer_in_a_codec_we_did_not_offer_fails_the_call_and_hangs_up() {
    let uas = ScriptedUas::start(Behavior::ParamContact); // answers PCMU (PT 0)
    let (_registrar, socket, registrar_addr) = proxy_rig(&uas);

    let outcome = siptest::sip::outbound::place_call(
        &socket,
        registrar_addr,
        REALM,
        USER,
        "+919000000000",
        resolve_codec("g722").unwrap(),
        0,
        Duration::from_secs(5),
    )
    .expect("place_call should not error");

    assert!(!outcome.answered);
    assert_eq!(outcome.refusal_reason, Some("codec_mismatch"));
    assert!(outcome.dialog.is_none(), "the call is already hung up");
    assert!(uas.wait_for("BYE"), "never hung up: {:?}", uas.seen());
}

/// De-registering must answer the registrar's digest challenge: an
/// authenticating registrar ignores an unauthenticated `Expires: 0`, so the
/// binding used to survive a "deregister".
#[test]
fn deregistering_actually_removes_the_binding_from_an_authenticating_registrar() {
    let registrar_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let registrar_addr = registrar_socket.local_addr().unwrap();
    let registrar = Registrar::start_on(registrar_socket, &server_config()).unwrap();
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
        call_id: "reg-call-id-dereg".to_string(),
        from_tag: "reg-from-tag-dereg".to_string(),
        cached_nonce: None,
        nc: 0,
    };
    register(&sip_socket, &reg_config, &mut creds).unwrap();
    let now = std::time::Instant::now();
    assert!(registrar.bindings().get_live(USER, now).is_some());

    let status =
        siptest::sip::registration::deregister(&sip_socket, &reg_config, &mut creds).unwrap();
    assert_eq!(
        status.state,
        siptest::sip::registration::RegState::Unregistered
    );
    assert!(
        registrar
            .bindings()
            .get_live(USER, std::time::Instant::now())
            .is_none(),
        "the binding survived the de-registration"
    );
}
