//! Multipart delivery tests through the production IMS MESSAGE handler.

use super::*;
use crate::ims::sip_client::SipTransport;
use crate::store::Transport;
use crate::volte::sms::{Dedupe, Disposition, InboundMessage, MessageRoute, Reassembly};
use std::io::ErrorKind;
use std::net::{TcpListener, UdpSocket};

const SENDER: &str = "+12345";
const DEFAULT_SCTS: [u8; 7] = [0; 7];
const EARLIER_SCTS: [u8; 7] = [0x62, 0x11, 0x51, 0x41, 0x10, 0x00, 0x00];
const LATER_SCTS: [u8; 7] = [0x62, 0x11, 0x51, 0x41, 0x20, 0x00, 0x00];

struct SmsHarness {
    session: crate::ims::RegisteredSession,
    sink: SipSink,
    peer: UdpSocket,
    reports: ReportsInFlight,
    reg_cfg: ImsRegisterConfig,
    status: Arc<Mutex<crate::ims::RegistrationStatus>>,
    dedupe: Arc<Mutex<Dedupe>>,
    reassembly: Arc<Mutex<Reassembly>>,
    obs: observability::AgentObservability,
    progress: Arc<watchdog::Progress>,
}

impl SmsHarness {
    fn new() -> Self {
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let transport = SipTransport::connect(peer.local_addr().unwrap(), false).unwrap();
        let sink = transport.sink().unwrap();
        let session =
            crate::ims::test_session_with_sa(Some(transport), peer.local_addr().unwrap(), None);
        let obs = observability::AgentObservability::new(
            Reporter::spawn(
                "/nonexistent/ims-sms-test-no-daemon.sock".into(),
                AgentKind::Ims,
                "sms-test".into(),
                Duration::from_secs(3600),
            ),
            "sms-test".into(),
            None,
            "sip:pbx@example.test".into(),
            Transport::Vowifi,
        );
        Self {
            session,
            sink,
            peer,
            reports: ReportsInFlight::default(),
            reg_cfg: crate::ims::test_register_config(),
            status: Arc::new(Mutex::new(crate::ims::RegistrationStatus::default())),
            dedupe: Arc::new(Mutex::new(Dedupe::default())),
            reassembly: Arc::new(Mutex::new(Reassembly::default())),
            obs,
            progress: Arc::new(watchdog::Progress::new("sms-test")),
        }
    }

    fn with_dispatch_params(
        &mut self,
        control_addr: SocketAddr,
        operation: impl FnOnce(
            &mut crate::ims::RegisteredSession,
            &DispatchParams,
            &mut ReportsInFlight,
            &SipSink,
        ),
    ) {
        let params = DispatchParams {
            reg_cfg: &self.reg_cfg,
            status: &self.status,
            control_addr,
            veth_local_ip: IpAddr::from([127, 0, 0, 1]),
            wideband: false,
            respond_on_client: false,
            answer_preference: sdp::AnswerPreference::legacy(),
            veth_sip_port: 5060,
            pre_renewal: None,
            attachment_check: None,
            modem_lock: None,
            dedupe: &self.dedupe,
            reassembly: &self.reassembly,
            sms_delivery_report: true,
            sms_report_pcscf_route: true,
            originating_headers: Default::default(),
            respect_caller_privacy: true,
            amr_passthrough: false,
            rtp_reorder_wait: Duration::ZERO,
            gm_options_keepalive: false,
            amr_mode_set: Default::default(),
            pbx_registered: None,
            obs: &self.obs,
            progress: &self.progress,
            reg_rejected_cooldown: Duration::from_secs(300),
            pcscf_source: None,
        };
        operation(&mut self.session, &params, &mut self.reports, &self.sink);
    }

    fn receive(&mut self, control_addr: SocketAddr, req: &SipRequest) {
        self.with_dispatch_params(control_addr, |session, params, reports, sink| {
            handle_message(session, params, reports, req, sink);
        });
    }

    fn flush_reassembly(&mut self, control_addr: SocketAddr) {
        self.with_dispatch_params(control_addr, |_, params, _, _| {
            flush_expired_reassembly(params);
        });
    }

    fn assert_claim(&self, sequence: u8, total: u8, text: &str, admitted: bool, confirmed: bool) {
        self.assert_timestamped_claim(sequence, total, text, DEFAULT_SCTS, admitted, confirmed);
    }

    fn assert_timestamped_claim(
        &self,
        sequence: u8,
        total: u8,
        text: &str,
        scts: [u8; 7],
        admitted: bool,
        confirmed: bool,
    ) {
        // Independent of the production formatter: assert the original
        // per-segment identity rather than the joined message's text key.
        let key = segment_key(sequence, total, text, scts);
        let dedupe = self.dedupe.lock().unwrap();
        assert_eq!(
            dedupe.contains(&key),
            admitted,
            "claim for {sequence}/{total}"
        );
        assert_eq!(
            dedupe.is_confirmed(&key),
            confirmed,
            "confirmation for {sequence}/{total}"
        );
    }

    fn expect_ack(&self, request: &SipRequest, rp_mr: u8) {
        let mut buf = [0u8; 4096];
        let n = self.peer.recv(&mut buf).expect("expected SIP 200 OK");
        let Some(SipMessage::Response(response)) =
            crate::ims::sip_client::parse_datagram(std::str::from_utf8(&buf[..n]).unwrap())
                .unwrap()
        else {
            panic!("expected a SIP response");
        };
        assert_eq!(response.status, 200);
        assert_eq!(response.header("Call-ID"), request.header("Call-ID"));

        let n = self.peer.recv(&mut buf).expect("expected an RP-ACK report");
        let (report, _) = SipRequest::try_parse(&buf[..n]).unwrap().unwrap();
        assert_eq!(report.method, "MESSAGE");
        assert_eq!(report.header("In-Reply-To"), request.header("Call-ID"));
        assert_eq!(report.body_bytes, [0x02, rp_mr, 0x41, 0x02, 0x00, 0x00]);
    }

    fn expect_no_ack(&self) {
        self.peer.set_nonblocking(true).unwrap();
        let result = self.peer.recv(&mut [0u8; 4096]);
        self.peer.set_nonblocking(false).unwrap();
        assert_eq!(result.unwrap_err().kind(), ErrorKind::WouldBlock);
    }
}

fn control_listener() -> TcpListener {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

fn assert_no_control_message(listener: &TcpListener) {
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
}

fn expect_control_sms(listener: &TcpListener, expected_body: &str) {
    take_control_sms(listener, expected_body);
    assert_no_control_message(listener);
}

fn take_control_sms(listener: &TcpListener, expected_body: &str) {
    let (stream, _) = listener.accept().expect("expected a control-channel relay");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let msg = read_msg(&mut BufReader::new(stream)).unwrap();
    match msg {
        ControlMessage::SmsReceived { sender, body, .. } => {
            assert_eq!(sender, SENDER);
            assert_eq!(body, expected_body);
        }
        other => panic!("expected an SMS relay, got {other:?}"),
    }
}

fn multipart_request(reference: u8, sequence: u8, total: u8, text: &str) -> SipRequest {
    multipart_request_with_scts(reference, sequence, total, text, DEFAULT_SCTS)
}

fn multipart_request_with_scts(
    reference: u8,
    sequence: u8,
    total: u8,
    text: &str,
    scts: [u8; 7],
) -> SipRequest {
    // UCS-2 avoids a test-only GSM7 encoder and preserves whitespace and
    // non-ASCII text in the original segment keys.
    let mut user_data = vec![0x05, 0x00, 0x03, reference, total, sequence];
    for code_unit in text.encode_utf16() {
        user_data.extend_from_slice(&code_unit.to_be_bytes());
    }
    // SMS-DELIVER, UDHI set; TP-OA +12345; PID 0; DCS UCS-2.
    let mut tpdu = vec![0x40, 5, 0x91, 0x21, 0x43, 0xf5, 0x00, 0x08];
    tpdu.extend_from_slice(&scts);
    tpdu.push(u8::try_from(user_data.len()).unwrap());
    tpdu.extend_from_slice(&user_data);
    let mut rp = vec![0x01, sequence, 0, 0];
    rp.push(u8::try_from(tpdu.len()).unwrap());
    rp.extend_from_slice(&tpdu);
    let timestamp: String = scts.iter().map(|byte| format!("{byte:02x}")).collect();

    let mut raw = format!(
        "MESSAGE sip:subscriber@example.test SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKsms-{reference}-{sequence}-{timestamp}\r\n\
         From: <sip:gateway@example.test>;tag=gateway\r\n\
         To: <sip:subscriber@example.test>\r\n\
         P-Asserted-Identity: <sip:gateway@example.test>\r\n\
         Call-ID: sms-{reference}-{sequence}-{timestamp}@example.test\r\n\
         CSeq: 1 MESSAGE\r\n\
         Content-Type: application/vnd.3gpp.sms\r\n\
         Content-Length: {}\r\n\r\n",
        rp.len()
    )
    .into_bytes();
    raw.extend_from_slice(&rp);
    SipRequest::try_parse(&raw).unwrap().unwrap().0
}

fn segment_key(sequence: u8, total: u8, text: &str, scts: [u8; 7]) -> String {
    let timestamp: String = scts.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{timestamp}\u{1}{SENDER}\u{1}[{sequence}/{total}] {text}")
}

#[test]
fn sms_handler_confirms_all_parts_only_after_the_joined_relay_succeeds() {
    for arrival_order in [
        &[1u8, 2][..],
        &[3u8, 1, 2][..],
        &[1u8, 2, 3, 4, 5, 6, 7, 8][..],
        &[8u8, 1, 7, 2, 6, 3, 5, 4][..],
    ] {
        let mut h = SmsHarness::new();
        let listener = control_listener();
        let control_addr = listener.local_addr().unwrap();
        let total = u8::try_from(arrival_order.len()).unwrap();
        let texts = [
            " first\n", "世界\t", " third ", "fourth ", "fifth ", "sixth ", "seventh ", " last ",
        ];
        let requests: Vec<_> = (1..=total)
            .map(|sequence| multipart_request(9, sequence, total, texts[usize::from(sequence - 1)]))
            .collect();
        h.dedupe.lock().unwrap().admit("unrelated-pending-claim");

        for (arrival, sequence) in arrival_order.iter().copied().enumerate() {
            let request = &requests[usize::from(sequence - 1)];
            h.receive(control_addr, request);
            // Even a Pending segment gets both network acknowledgments.
            h.expect_ack(request, sequence);
            if arrival + 1 < arrival_order.len() {
                for previous in &arrival_order[..=arrival] {
                    h.assert_claim(
                        *previous,
                        total,
                        texts[usize::from(*previous - 1)],
                        true,
                        false,
                    );
                }
                assert_no_control_message(&listener);
            }
        }

        expect_control_sms(&listener, &texts[..usize::from(total)].concat());
        for sequence in 1..=total {
            let text = texts[usize::from(sequence - 1)];
            h.assert_claim(sequence, total, text, true, true);
            let modem_copy = InboundMessage {
                route: MessageRoute::ThroughModem,
                sender: SENDER.into(),
                body: format!("[{sequence}/{total}] {text}"),
                modem_index: Some(u32::from(sequence)),
                scts: Some(DEFAULT_SCTS),
            };
            let mut dedupe = h.dedupe.lock().unwrap();
            assert_eq!(
                crate::volte::sms::decide(&mut dedupe, &modem_copy),
                Disposition::AcknowledgeOnly
            );
            assert!(dedupe.is_confirmed(&modem_copy.dedupe_key()));
        }
        let dedupe = h.dedupe.lock().unwrap();
        assert!(dedupe.contains("unrelated-pending-claim"));
        assert!(!dedupe.is_confirmed("unrelated-pending-claim"));
        drop(dedupe);

        // Removal of the completed buffer prevents a later expiry from
        // emitting already-relayed fragments.
        let mut reassembly = h.reassembly.lock().unwrap();
        reassembly.expire_due(Instant::now() + crate::volte::sms::REASSEMBLY_TIMEOUT);
        assert!(reassembly.ready_for_delivery().is_empty());
        drop(reassembly);

        // Retrying an earlier arrival must still receive its RP-ACK, and
        // must not produce another joined message or individual fragment.
        let first = arrival_order[0];
        let request = &requests[usize::from(first - 1)];
        h.receive(control_addr, request);
        h.expect_ack(request, first);
        assert_no_control_message(&listener);
        h.expect_no_ack();
    }
}

#[test]
fn sms_handler_preserves_all_parts_until_a_failed_complete_relay_is_retried() {
    let mut h = SmsHarness::new();
    let listener = control_listener();
    let control_addr = listener.local_addr().unwrap();
    let first = multipart_request_with_scts(10, 1, 2, "first ", EARLIER_SCTS);
    let second = multipart_request_with_scts(10, 2, 2, "second", LATER_SCTS);
    let other_timestamp_key = segment_key(1, 2, "first ", LATER_SCTS);
    h.dedupe.lock().unwrap().admit(&other_timestamp_key);

    h.receive(control_addr, &first);
    h.expect_ack(&first, 1);
    h.assert_timestamped_claim(1, 2, "first ", EARLIER_SCTS, true, false);
    assert_no_control_message(&listener);

    // Port zero cannot host a listening TCP service. The real handler's
    // connect failure must release only the completing part's claim.
    h.receive("127.0.0.1:0".parse().unwrap(), &second);
    h.assert_timestamped_claim(1, 2, "first ", EARLIER_SCTS, true, false);
    h.assert_timestamped_claim(2, 2, "second", LATER_SCTS, false, false);
    assert_no_control_message(&listener);
    h.expect_no_ack();

    // Only part 2 is resent: completion proves part 1 survived the failed
    // relay, and success must finally confirm both original identities.
    h.receive(control_addr, &second);
    h.expect_ack(&second, 2);
    expect_control_sms(&listener, "first second");
    h.assert_timestamped_claim(1, 2, "first ", EARLIER_SCTS, true, true);
    h.assert_timestamped_claim(2, 2, "second", LATER_SCTS, true, true);
    assert!(!h.dedupe.lock().unwrap().is_confirmed(&other_timestamp_key));
}

#[test]
fn sms_handler_completion_preserves_another_pending_assembly_from_the_same_sender() {
    let mut h = SmsHarness::new();
    let listener = control_listener();
    let control_addr = listener.local_addr().unwrap();
    let first = multipart_request(12, 1, 2, "first ");
    let second = multipart_request(12, 2, 2, "second");
    let other_first = multipart_request(13, 1, 2, "other ");
    let other_second = multipart_request(13, 2, 2, "message");

    for request in [&first, &other_first] {
        h.receive(control_addr, request);
        h.expect_ack(request, 1);
    }
    assert_no_control_message(&listener);

    h.receive(control_addr, &second);
    h.expect_ack(&second, 2);
    expect_control_sms(&listener, "first second");
    h.assert_claim(1, 2, "first ", true, true);
    h.assert_claim(2, 2, "second", true, true);
    h.assert_claim(1, 2, "other ", true, false);

    h.receive(control_addr, &other_second);
    h.expect_ack(&other_second, 2);
    expect_control_sms(&listener, "other message");
    h.assert_claim(1, 2, "other ", true, true);
    h.assert_claim(2, 2, "message", true, true);
}

#[test]
fn sms_handler_does_not_reinsert_evicted_constituent_claims_when_confirming_completion() {
    let mut h = SmsHarness::new();
    h.dedupe = Arc::new(Mutex::new(Dedupe::new(2)));
    let listener = control_listener();
    let control_addr = listener.local_addr().unwrap();
    let first = multipart_request(11, 1, 2, "first ");
    let second = multipart_request(11, 2, 2, "second");

    h.receive(control_addr, &first);
    h.expect_ack(&first, 1);
    {
        let mut dedupe = h.dedupe.lock().unwrap();
        dedupe.admit("eviction-filler");
        dedupe.admit("unrelated-pending-claim");
    }
    h.assert_claim(1, 2, "first ", false, false);

    h.receive(control_addr, &second);
    h.expect_ack(&second, 2);
    expect_control_sms(&listener, "first second");
    h.assert_claim(1, 2, "first ", false, false);
    h.assert_claim(2, 2, "second", true, true);
    let dedupe = h.dedupe.lock().unwrap();
    assert_eq!(dedupe.len(), 2);
    assert!(dedupe.contains("unrelated-pending-claim"));
    assert!(!dedupe.is_confirmed("unrelated-pending-claim"));
}

#[test]
fn sms_handler_delivers_identical_multipart_text_again_with_new_scts_and_reused_reference() {
    let mut h = SmsHarness::new();
    let listener = control_listener();
    let control_addr = listener.local_addr().unwrap();
    let texts = ["same ", "multipart ", "text"];

    for timestamp in [EARLIER_SCTS, LATER_SCTS] {
        let requests: Vec<_> = (1..=3u8)
            .map(|sequence| {
                multipart_request_with_scts(
                    20,
                    sequence,
                    3,
                    texts[usize::from(sequence - 1)],
                    timestamp,
                )
            })
            .collect();
        for (index, request) in requests.iter().enumerate() {
            let sequence = u8::try_from(index + 1).unwrap();
            h.receive(control_addr, request);
            h.expect_ack(request, sequence);
            if sequence < 3 {
                h.assert_timestamped_claim(sequence, 3, texts[index], timestamp, true, false);
                assert_no_control_message(&listener);
            }
        }
        expect_control_sms(&listener, "same multipart text");
        for (index, request) in requests.iter().enumerate() {
            let sequence = u8::try_from(index + 1).unwrap();
            h.assert_timestamped_claim(sequence, 3, texts[index], timestamp, true, true);
            // Keeping TP-SCTS unchanged identifies a redelivery, even after
            // the completed assembly has been removed.
            h.receive(control_addr, request);
            h.expect_ack(request, sequence);
            assert_no_control_message(&listener);
        }
    }
    // The second delivery must not overwrite or forget the first one's
    // confirmations merely because sender, text and concat reference match.
    for (index, text) in texts.iter().enumerate() {
        let sequence = u8::try_from(index + 1).unwrap();
        for timestamp in [EARLIER_SCTS, LATER_SCTS] {
            h.assert_timestamped_claim(sequence, 3, text, timestamp, true, true);
        }
    }
}

#[test]
fn sms_handler_assembles_new_parts_despite_old_modem_claims_for_identical_text() {
    let mut h = SmsHarness::new();
    let listener = control_listener();
    let control_addr = listener.local_addr().unwrap();
    let texts = ["first ", "second ", "third"];
    let mut second_timestamp = LATER_SCTS;
    second_timestamp[5] = 0x10;
    let mut third_timestamp = LATER_SCTS;
    third_timestamp[5] = 0x20;
    let timestamps = [LATER_SCTS, second_timestamp, third_timestamp];

    // Model the historical failure: parts 1 and 2 of an older SMS were
    // already delivered through the modem before an identical new SMS.
    for sequence in 1..=2u8 {
        let old_copy = InboundMessage {
            route: MessageRoute::ThroughModem,
            sender: SENDER.into(),
            body: format!("[{sequence}/3] {}", texts[usize::from(sequence - 1)]),
            modem_index: Some(u32::from(sequence)),
            scts: Some(EARLIER_SCTS),
        };
        let mut dedupe = h.dedupe.lock().unwrap();
        assert_eq!(
            crate::volte::sms::decide(&mut dedupe, &old_copy),
            Disposition::Handle
        );
        dedupe.confirm(&old_copy.dedupe_key());
    }

    // Different TP-SCTS values on this message's individual segments must
    // distinguish deliveries without becoming the reassembly group key.
    for (arrival, sequence) in [3u8, 1, 2].into_iter().enumerate() {
        let index = usize::from(sequence - 1);
        let request = multipart_request_with_scts(21, sequence, 3, texts[index], timestamps[index]);
        h.receive(control_addr, &request);
        h.expect_ack(&request, sequence);
        if arrival < 2 {
            h.assert_timestamped_claim(sequence, 3, texts[index], timestamps[index], true, false);
            assert_no_control_message(&listener);
        }
    }
    expect_control_sms(&listener, "first second third");
    for (index, text) in texts.iter().enumerate() {
        let sequence = u8::try_from(index + 1).unwrap();
        h.assert_timestamped_claim(sequence, 3, text, timestamps[index], true, true);
        if sequence < 3 {
            h.assert_timestamped_claim(sequence, 3, text, EARLIER_SCTS, true, true);
        }
    }
}

#[test]
fn sms_handler_expiry_flush_retries_and_confirms_the_original_timestamped_keys() {
    let mut h = SmsHarness::new();
    let listener = control_listener();
    let control_addr = listener.local_addr().unwrap();
    let first = multipart_request_with_scts(22, 1, 3, "first ", EARLIER_SCTS);
    let second = multipart_request_with_scts(22, 2, 3, "second", LATER_SCTS);
    let shadow_keys = [
        segment_key(1, 3, "first ", LATER_SCTS),
        segment_key(2, 3, "second", EARLIER_SCTS),
    ];
    for key in &shadow_keys {
        h.dedupe.lock().unwrap().admit(key);
    }

    for (request, sequence) in [(&first, 1), (&second, 2)] {
        h.receive(control_addr, request);
        h.expect_ack(request, sequence);
    }
    assert_no_control_message(&listener);
    h.reassembly
        .lock()
        .unwrap()
        .expire_due(Instant::now() + crate::volte::sms::REASSEMBLY_TIMEOUT);
    assert_eq!(h.reassembly.lock().unwrap().ready_for_delivery().len(), 2);

    // A failed expiry flush must keep both original identities queued.
    h.flush_reassembly("127.0.0.1:0".parse().unwrap());
    assert_eq!(h.reassembly.lock().unwrap().ready_for_delivery().len(), 2);
    h.assert_timestamped_claim(1, 3, "first ", EARLIER_SCTS, true, false);
    h.assert_timestamped_claim(2, 3, "second", LATER_SCTS, true, false);
    assert_no_control_message(&listener);

    h.flush_reassembly(control_addr);
    take_control_sms(&listener, "[1/3] first ");
    take_control_sms(&listener, "[2/3] second");
    assert_no_control_message(&listener);
    h.assert_timestamped_claim(1, 3, "first ", EARLIER_SCTS, true, true);
    h.assert_timestamped_claim(2, 3, "second", LATER_SCTS, true, true);
    assert!(h.reassembly.lock().unwrap().ready_for_delivery().is_empty());
    for key in &shadow_keys {
        let dedupe = h.dedupe.lock().unwrap();
        assert!(dedupe.contains(key));
        assert!(!dedupe.is_confirmed(key));
    }
    // Already-acknowledged expiry parts need no new network acknowledgment,
    // and a later flush must not repeat their control-channel relays.
    h.expect_no_ack();
    h.flush_reassembly(control_addr);
    assert_no_control_message(&listener);
}
