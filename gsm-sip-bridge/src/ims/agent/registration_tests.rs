//! Registration and call dispatch share the real client reader in these tests.

use super::*;
use crate::ims::registration_auth::RegistrationAuth;
use crate::ims::session::SentSubscribe;
use crate::ims::sip_client::{SipMessage, SipTransport};
use std::net::{Ipv4Addr, UdpSocket};

struct Fixture {
    cfg: ImsRegisterConfig,
    status: Arc<Mutex<crate::ims::RegistrationStatus>>,
    dedupe: Arc<Mutex<crate::volte::sms::Dedupe>>,
    reassembly: Arc<Mutex<crate::volte::sms::Reassembly>>,
    obs: observability::AgentObservability,
    progress: Arc<watchdog::Progress>,
}

fn attachment_must_not_be_rebuilt() -> Result<(), String> {
    panic!("a periodic refresh must not rebuild the attachment")
}

impl Fixture {
    fn new(peer: SocketAddr) -> Self {
        let now = SystemTime::now();
        let reporter = Reporter::spawn(
            "/tmp/gsm-ims-registration-test-no-daemon.sock".into(),
            AgentKind::Ims,
            "registration-test".into(),
            Duration::from_secs(60),
        );
        Self {
            cfg: ImsRegisterConfig {
                modem_port: "/must-not-open-a-modem-for-refresh".into(),
                pcsc_reader: false,
                pcscf_addr: peer.ip(),
                pcscf_port: peer.port(),
                mcc: "001".into(),
                mnc: "01".into(),
                imsi: None,
                imei: None,
                use_tcp: false,
                sec_agree: false,
                msisdn: None,
                access_network_info: crate::ims::ACCESS_NETWORK_WLAN.into(),
                register_uri_home_domain: true,
                gm_auth_alg: None,
                gm_cipher_alg: None,
            },
            status: Arc::new(Mutex::new(crate::ims::RegistrationStatus {
                state: crate::ims::RegistrationState::Registered,
                registered_at: Some(now - Duration::from_secs(3300)),
                expires_at: Some(now + Duration::from_secs(300)),
                ..Default::default()
            })),
            dedupe: Arc::new(Mutex::new(Default::default())),
            reassembly: Arc::new(Mutex::new(Default::default())),
            obs: observability::AgentObservability::new(
                reporter,
                "registration-test".into(),
                None,
                "sip:pbx@example.test".into(),
                crate::store::Transport::Vowifi,
            ),
            progress: Arc::new(watchdog::Progress::new("registration-test")),
        }
    }

    fn params(&self) -> DispatchParams<'_> {
        DispatchParams {
            reg_cfg: &self.cfg,
            status: &self.status,
            control_addr: "127.0.0.1:9".parse().unwrap(),
            veth_local_ip: Ipv4Addr::LOCALHOST.into(),
            wideband: false,
            respond_on_client: false,
            answer_preference: sdp::AnswerPreference::Legacy,
            veth_sip_port: 0,
            pre_renewal: Some(&attachment_must_not_be_rebuilt),
            attachment_check: None,
            modem_lock: None,
            dedupe: &self.dedupe,
            reassembly: &self.reassembly,
            sms_delivery_report: false,
            sms_report_pcscf_route: false,
            originating_headers: Default::default(),
            respect_caller_privacy: true,
            amr_passthrough: false,
            gm_options_keepalive: false,
            amr_mode_set: Default::default(),
            pbx_registered: None,
            obs: &self.obs,
            progress: &self.progress,
            reg_rejected_cooldown: Duration::from_secs(60),
            pcscf_source: None,
        }
    }
}

fn client_session(peer: &UdpSocket) -> crate::ims::RegisteredSession {
    let transport = SipTransport::connect(peer.local_addr().unwrap(), false).unwrap();
    let local = transport.local_addr().unwrap();
    let mut session =
        crate::ims::test_session_with_sa(Some(transport), peer.local_addr().unwrap(), None);
    session.local_addr = local;
    session.contact_addr = local;
    session.use_tcp = false;
    session.registration_auth = Some(RegistrationAuth {
        impi: "user@ims.example.test".into(),
        authorization: "Digest nonce=\"cached\",response=\"cached-response\"".into(),
    });
    session
}

fn peer() -> UdpSocket {
    let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    peer
}

fn receive_request(peer: &UdpSocket) -> SipRequest {
    let mut bytes = [0; 8192];
    let n = peer.recv(&mut bytes).unwrap();
    SipRequest::try_parse(&bytes[..n]).unwrap().unwrap().0
}

fn answer_registration(
    peer: &UdpSocket,
    local: SocketAddr,
    request: &SipRequest,
    inbound: &Inbound,
) -> SipResponse {
    let wire = format!(
        "SIP/2.0 200 OK\r\nVia: {}\r\nCall-ID: {}\r\nCSeq: {}\r\nContact: <sip:user@{}>;expires=1200\r\nExpires: 3600\r\nContent-Length: 0\r\n\r\n",
        request.header("Via").unwrap(), request.header("Call-ID").unwrap(),
        request.header("CSeq").unwrap(), local,
    );
    peer.send_to(wire.as_bytes(), local).unwrap();
    let (SipMessage::Response(response), _) =
        inbound.rx.recv_timeout(Duration::from_secs(2)).unwrap()
    else {
        panic!("the existing reader must route the REGISTER response");
    };
    response
}

fn with_active_call() -> LoopState {
    let mut state = LoopState::new();
    state.active_call = Some(test_active_call(
        "c1",
        "our-tag",
        "<sip:caller@example.net>;tag=abc",
    ));
    let now = Instant::now();
    state.reg_sub.on_sent(
        &SentSubscribe {
            call_id: "existing-subscription".into(),
            local_tag: "subscription-tag".into(),
            cseq: 7,
            expires: 3600,
        },
        SubKind::Initial,
        now,
    );
    state.reg_sub.on_response(
        &SipResponse {
            status: 200,
            reason: "OK".into(),
            headers: vec![
                ("Call-ID".into(), "existing-subscription".into()),
                ("CSeq".into(), "7 SUBSCRIBE".into()),
                (
                    "To".into(),
                    "<sip:user@ims.example.test>;tag=notifier".into(),
                ),
                ("Contact".into(), "<sip:notifier@ims.example.test>".into()),
                ("Expires".into(), "3600".into()),
            ],
            body: String::new(),
        },
        now,
    );
    state
}

#[test]
fn a_due_registration_refreshes_during_a_call_and_preserves_its_subscription() {
    let peer = peer();
    let mut session = client_session(&peer);
    let mut inbound = start_inbound(&session).unwrap();
    let fixture = Fixture::new(peer.local_addr().unwrap());
    let p = fixture.params();
    let mut state = with_active_call();
    let transport_addr = session.transport().unwrap().local_addr().unwrap();
    let stop = state.active_call.as_ref().unwrap().stop.clone();

    state.on_idle_tick(&mut session, &mut inbound, &p).unwrap();
    let request = receive_request(&peer);
    assert_eq!(request.method, "REGISTER");
    assert!(state.registration_refresh.is_some());
    assert!(state.active_call.is_some());
    assert!(fixture.status.lock().unwrap().health().registered);
    let response = answer_registration(&peer, session.local_addr, &request, &inbound);
    assert!(state
        .handle_registration_response(&mut session, &inbound, &p, &response)
        .unwrap());

    assert!(state.registration_refresh.is_none());
    assert_eq!(state.active_call.as_ref().unwrap().call_id, "c1");
    assert!(!stop.load(Ordering::Relaxed));
    assert_eq!(
        session.transport().unwrap().local_addr().unwrap(),
        transport_addr
    );
    assert!(inbound.client_reader_is_alive());
    let dialog = state
        .reg_sub
        .refresh_dialog()
        .expect("the subscription dialog survives refresh");
    assert_eq!(dialog.call_id, "existing-subscription");
    assert_eq!(dialog.remote_tag, "notifier");
    let status = fixture.status.lock().unwrap();
    assert_eq!(
        status
            .expires_at
            .unwrap()
            .duration_since(status.registered_at.unwrap())
            .unwrap(),
        Duration::from_secs(1200)
    );
    session.transport().unwrap().shutdown();
}

#[test]
fn a_carrier_bye_is_dispatched_while_register_is_waiting_for_its_response() {
    let peer = peer();
    let mut session = client_session(&peer);
    let mut inbound = start_inbound(&session).unwrap();
    let fixture = Fixture::new(peer.local_addr().unwrap());
    let p = fixture.params();
    let mut state = with_active_call();
    state.on_idle_tick(&mut session, &mut inbound, &p).unwrap();
    let register = receive_request(&peer);

    peer.send_to(b"BYE sip:user@ims.example.test SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1;branch=z9hG4bKbye;rport\r\nFrom: <sip:caller@example.net>;tag=abc\r\nTo: <sip:user@ims.example.test>;tag=our-tag\r\nCall-ID: c1\r\nCSeq: 3 BYE\r\nContent-Length: 0\r\n\r\n", session.local_addr).unwrap();
    let (SipMessage::Request(bye), sink) = inbound.rx.recv_timeout(Duration::from_secs(2)).unwrap()
    else {
        panic!("the existing reader must dispatch BYE while REGISTER is pending");
    };
    state.handle_carrier_bye(&p, &bye, &sink);
    assert!(state.active_call.is_none());
    assert!(state.registration_refresh.is_some());
    let mut bytes = [0; 4096];
    let n = peer
        .recv(&mut bytes)
        .expect("the carrier must promptly receive its BYE response");
    assert!(String::from_utf8_lossy(&bytes[..n]).starts_with("SIP/2.0 200 OK"));

    let response = answer_registration(&peer, session.local_addr, &register, &inbound);
    state
        .handle_registration_response(&mut session, &inbound, &p, &response)
        .unwrap();
    assert!(state.registration_refresh.is_none());
    assert!(fixture.status.lock().unwrap().health().registered);
    session.transport().unwrap().shutdown();
}

#[test]
fn failed_refreshes_preserve_a_valid_binding_and_owe_budget_exit_until_call_ends() {
    let peer = peer();
    let fixture = Fixture::new(peer.local_addr().unwrap());
    let p = fixture.params();
    let mut state = with_active_call();
    for _ in 0..MAX_CONSECUTIVE_RENEWAL_FAILURES {
        state
            .fail_registration_refresh(
                &p,
                RegisterFailure {
                    error: BridgeError::Ims("refresh response timed out".into()),
                    status: None,
                    retry_after: Some(Duration::from_secs(60)),
                },
            )
            .expect("refresh retries must not exit during the active call");
    }
    assert!(state.active_call.is_some());
    assert!(fixture.status.lock().unwrap().health().registered);
    assert!(state.next_renewal_attempt.unwrap() > Instant::now() + Duration::from_secs(50));
    assert!(state.budget_exit_owed.is_some());
    assert!(state.take_owed_budget_exit().is_ok());
    state.active_call.take();
    assert!(state.take_owed_budget_exit().is_err());
}

#[test]
fn an_explicit_zero_grant_invalidates_the_binding_even_before_its_previous_expiry() {
    let peer = peer();
    let mut session = client_session(&peer);
    let mut inbound = start_inbound(&session).unwrap();
    let fixture = Fixture::new(peer.local_addr().unwrap());
    let p = fixture.params();
    let mut state = with_active_call();
    state.on_idle_tick(&mut session, &mut inbound, &p).unwrap();
    let request = receive_request(&peer);
    let mut response = answer_registration(&peer, session.local_addr, &request, &inbound);
    response
        .headers
        .iter_mut()
        .find(|(name, _)| name == "Contact")
        .unwrap()
        .1 = format!("<sip:user@{}>;expires=0", session.contact_addr);
    state
        .handle_registration_response(&mut session, &inbound, &p, &response)
        .unwrap();
    assert!(!fixture.status.lock().unwrap().health().registered);
    assert!(fixture.status.lock().unwrap().health().registration_expired);
    assert!(state.force_renewal);
    assert!(state.active_call.is_some());
    session.transport().unwrap().shutdown();
}

#[test]
fn a_late_register_response_cannot_confirm_an_options_probe_with_the_same_cseq() {
    let peer = peer();
    let mut session = client_session(&peer);
    session.registration_cseq = session.cseq + 1;
    let mut inbound = start_inbound(&session).unwrap();
    let fixture = Fixture::new(peer.local_addr().unwrap());
    let mut p = fixture.params();
    p.gm_options_keepalive = true;
    let mut state = LoopState::new();
    state.ping.periodic = p.gm_options_keepalive;

    state.on_idle_tick(&mut session, &mut inbound, &p).unwrap();
    let options = receive_request(&peer);
    let register = receive_request(&peer);
    assert_eq!(options.method, "OPTIONS");
    assert_eq!(register.method, "REGISTER");
    assert_eq!(
        options.header("CSeq").and_then(parse_cseq_number),
        register.header("CSeq").and_then(parse_cseq_number)
    );
    state.cancel_registration_refresh();
    state.gm_conn = crate::ims::GmConnectionState::Reconnecting {
        since: SystemTime::now(),
        attempts: 1,
    };
    state.reconnect_attempts = 1;

    let mut response = SipResponse {
        status: 200,
        reason: "OK".into(),
        headers: vec![
            ("Call-ID".into(), session.call_id.clone()),
            ("CSeq".into(), register.header("CSeq").unwrap().into()),
        ],
        body: String::new(),
    };
    state.handle_carrier_response(&mut session, &p, &response);
    assert!(!state.gm_conn.is_up());
    assert_eq!(state.reconnect_attempts, 1);

    response.headers[1].1 = options.header("CSeq").unwrap().into();
    response.status = 100;
    state.handle_carrier_response(&mut session, &p, &response);
    assert!(!state.gm_conn.is_up());
    response.status = 200;
    state.handle_carrier_response(&mut session, &p, &response);
    assert!(state.gm_conn.is_up());
    assert_eq!(state.reconnect_attempts, 0);
    session.transport().unwrap().shutdown();
}

#[test]
fn cancelled_background_work_still_advances_registration_cseq_without_confirming_a_binding() {
    let peer = peer();
    let mut session = client_session(&peer);
    let inbound = start_inbound(&session).unwrap();
    let fixture = Fixture::new(peer.local_addr().unwrap());
    let mut state = with_active_call();
    let (result, result_rx) = mpsc::channel();
    result
        .send(Err(
            BridgeError::Ims("cancelled authentication".into()).into()
        ))
        .unwrap();
    let (responses, _) = mpsc::channel();
    state.registration_reauth = Some(RegistrationReauth {
        result: result_rx,
        responses,
        call_id: session.call_id.clone(),
        deadline: Instant::now() + Duration::from_secs(60),
        cancel: Arc::new(AtomicBool::new(true)),
        cancelled: true,
        next_cseq: Arc::new(AtomicU32::new(10)),
    });
    state
        .poll_registration_refresh(&mut session, &inbound, &fixture.params())
        .unwrap();
    assert_eq!(session.registration_cseq, 10);
    assert_eq!(state.consecutive_renewal_failures, 0);
    assert!(state.active_call.is_some());
    assert!(state.registration_reauth.is_none());
    session.transport().unwrap().shutdown();
}
