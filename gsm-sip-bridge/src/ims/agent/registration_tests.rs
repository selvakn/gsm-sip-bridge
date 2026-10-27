//! The dispatch loop's handling of an in-place registration refresh, driven
//! against a real UDP socket standing in for the P-CSCF. The agent as a whole
//! needs a modem and SIM, so these exercise `LoopState` with the same narrow
//! context the loop passes it.

use super::call::test_active_call;
use super::pcscf_pool::PcscfPool;
use super::registration_refresh::{GmLink, RefreshCtx, REJECTIONS_BEFORE_FALLBACK};
use super::*;
use crate::ims::observability::AgentObservability;
use crate::ims::sip_client::{SipMessage, SipResponse, SipSink, SipTransport};
use crate::ims::{RegistrationAuth, RegistrationState, RegistrationStatus};
use std::net::UdpSocket;
use std::sync::mpsc;

const CALLER_FROM: &str = "<sip:caller@example.net>;tag=abc";

/// The Gm connection for the tests: a queue the test can feed, and a repair that
/// either fails or puts a fresh connection to the same peer on the session.
struct FakeLink {
    tx: mpsc::Sender<(SipMessage, SipSink)>,
    rx: mpsc::Receiver<(SipMessage, SipSink)>,
    peer: std::net::SocketAddr,
    repairs: u32,
    repair_fails: bool,
    alive: bool,
}

impl FakeLink {
    fn new(peer: std::net::SocketAddr) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            peer,
            repairs: 0,
            repair_fails: false,
            alive: true,
        }
    }
}

impl GmLink for FakeLink {
    fn queue(&self) -> &mpsc::Receiver<(SipMessage, SipSink)> {
        &self.rx
    }

    fn client_alive(&self) -> bool {
        self.alive
    }

    fn repair(&mut self, session: &mut crate::ims::RegisteredSession) -> BridgeResult<()> {
        self.repairs += 1;
        self.alive = true;
        if self.repair_fails {
            return Err(BridgeError::Ims("reconnect refused".into()));
        }
        let transport = SipTransport::connect(self.peer, false)?;
        session.local_addr = transport.local_addr()?;
        session.contact_addr = session.local_addr;
        session.transport = Some(transport);
        Ok(())
    }
}

struct Rig {
    st: LoopState,
    session: crate::ims::RegisteredSession,
    status: Arc<Mutex<RegistrationStatus>>,
    obs: AgentObservability,
    cfg: ImsRegisterConfig,
    peer: UdpSocket,
    link: FakeLink,
}

impl Rig {
    fn new() -> Self {
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let transport = SipTransport::connect(peer.local_addr().unwrap(), false).unwrap();
        let local = transport.local_addr().unwrap();
        let mut session =
            crate::ims::test_session_with_sa(Some(transport), peer.local_addr().unwrap(), None);
        session.local_addr = local;
        session.contact_addr = local;
        session.use_tcp = false;
        session.registration_auth = Some(RegistrationAuth {
            authorization: "Digest nonce=\"cached\",response=\"last-response\"".into(),
        });
        let cfg = ImsRegisterConfig {
            modem_port: "/must-not-open-modem".into(),
            pcsc_reader: false,
            pcscf_addr: peer.local_addr().unwrap().ip(),
            pcscf_port: peer.local_addr().unwrap().port(),
            mcc: "001".into(),
            mnc: "01".into(),
            imsi: None,
            imei: None,
            use_tcp: false,
            sec_agree: false,
            msisdn: None,
            access_network_info: "3GPP-WLAN".into(),
            register_uri_home_domain: true,
            gm_auth_alg: None,
            gm_cipher_alg: None,
        };
        // Granted an hour, 3100 s ago: the refresh fell due at 3000 s.
        let now = SystemTime::now();
        let status = Arc::new(Mutex::new(RegistrationStatus {
            state: RegistrationState::Registered,
            registered_at: Some(now - Duration::from_secs(3100)),
            expires_at: Some(now + Duration::from_secs(500)),
            ..Default::default()
        }));
        let obs = AgentObservability::new(
            crate::observability::reporter::Reporter::spawn(
                "/nonexistent/no-daemon.sock".to_string(),
                crate::control::protocol::AgentKind::Ims,
                "refresh-test".into(),
                Duration::from_secs(3600),
            ),
            "refresh-test".into(),
            None,
            "sip:pbx@192.0.2.1".into(),
            crate::store::Transport::Vowifi,
        );
        Self {
            st: LoopState::new(),
            session,
            status,
            obs,
            cfg,
            link: FakeLink::new(peer.local_addr().unwrap()),
            peer,
        }
    }

    fn ctx(&self) -> RefreshCtx<'_> {
        RefreshCtx {
            status: &self.status,
            obs: &self.obs,
            reg_cfg: &self.cfg,
        }
    }

    fn drive(&mut self) -> BridgeResult<()> {
        let ctx = RefreshCtx {
            status: &self.status,
            obs: &self.obs,
            reg_cfg: &self.cfg,
        };
        self.st
            .drive_registration_refresh(&mut self.session, &ctx, &mut self.link)
    }

    fn answer(&mut self, response: &SipResponse) -> bool {
        let ctx = RefreshCtx {
            status: &self.status,
            obs: &self.obs,
            reg_cfg: &self.cfg,
        };
        self.st
            .handle_registration_answer(&mut self.session, &ctx, response)
            .unwrap()
    }

    fn received(&self) -> Option<String> {
        let mut bytes = [0; 8192];
        let size = self.peer.recv(&mut bytes).ok()?;
        Some(String::from_utf8(bytes[..size].to_vec()).unwrap())
    }

    /// The answer a P-CSCF would send to `request`.
    fn reply_to(&self, request: &str, status: u16, extra: &[(&str, &str)]) -> SipResponse {
        let header = |name: &str| {
            request
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{name}: ")))
                .unwrap_or_default()
                .to_string()
        };
        let mut headers = vec![
            ("Call-ID".to_string(), header("Call-ID")),
            ("CSeq".to_string(), header("CSeq")),
            ("Via".to_string(), header("Via")),
        ];
        headers.extend(extra.iter().map(|(n, v)| (n.to_string(), v.to_string())));
        SipResponse {
            status,
            reason: "test".into(),
            headers,
            body: String::new(),
        }
    }

    fn contact(&self, expires: u32) -> String {
        format!(
            "<sip:user@{};transport=udp>;expires={expires}",
            self.session.contact_addr
        )
    }

    fn pool(&mut self, other: &str) {
        let first = self.session.pcscf_addr.ip();
        let second: IpAddr = other.parse().unwrap();
        self.st.pool = PcscfPool::new(first, vec![first, second], None);
        // The registration is through `first`: the pool only attributes a
        // failure to the address it last selected.
        self.st.pool.select(SystemTime::now());
    }
}

fn expiry(rig: &Rig) -> SystemTime {
    rig.status.lock().unwrap().expires_at.unwrap()
}

#[test]
fn a_due_refresh_goes_out_in_place_on_the_live_connection() {
    let mut rig = Rig::new();
    rig.drive().unwrap();
    let wire = rig.received().expect("a REGISTER was sent");
    assert!(wire.starts_with("REGISTER "));
    assert!(wire.contains("Authorization: Digest nonce=\"cached\""));
    assert!(rig.st.registration_refresh.is_some());
}

#[test]
fn a_refresh_that_is_not_yet_due_is_not_sent() {
    let mut rig = Rig::new();
    let now = SystemTime::now();
    {
        let mut guard = rig.status.lock().unwrap();
        guard.registered_at = Some(now - Duration::from_secs(60));
        guard.expires_at = Some(now + Duration::from_secs(3540));
    }
    rig.drive().unwrap();
    assert!(rig.received().is_none());
    assert!(rig.st.registration_refresh.is_none());
}

#[test]
fn only_one_refresh_is_outstanding_at_a_time() {
    let mut rig = Rig::new();
    rig.drive().unwrap();
    rig.received().unwrap();
    rig.drive().unwrap();
    assert!(
        rig.received().is_none(),
        "no second REGISTER while one is pending"
    );
}

#[test]
fn a_refresh_goes_out_during_a_call_and_leaves_the_call_alone() {
    let mut rig = Rig::new();
    rig.st.active_call = Some(test_active_call("c1", "our-to-tag", CALLER_FROM));
    let cseq_before = rig.st.active_call.as_ref().unwrap().dialog.cseq;
    let shared_cseq = rig.session.cseq;

    rig.drive().unwrap();

    assert!(
        rig.received().is_some(),
        "the refresh must not wait for the call"
    );
    let call = rig.st.active_call.as_ref().expect("the call is still up");
    assert_eq!(call.call_id, "c1");
    assert_eq!(call.dialog.cseq, cseq_before);
    assert_eq!(
        rig.session.cseq, shared_cseq,
        "calls and probes keep their own counter"
    );
    assert_eq!(
        rig.st.maintenance.deferred(),
        None,
        "an ordinary refresh is never reported as deferred maintenance"
    );
}

#[test]
fn an_accepted_refresh_moves_the_expiry_and_keeps_the_session() {
    let mut rig = Rig::new();
    rig.st.active_call = Some(test_active_call("c1", "our-to-tag", CALLER_FROM));
    rig.drive().unwrap();
    let request = rig.received().unwrap();
    let contact = rig.contact(3000);
    let before = expiry(&rig);
    rig.st.refresh_rejections = 1;

    let reply = rig.reply_to(&request, 200, &[("Contact", &contact)]);
    assert!(rig.answer(&reply));

    assert!(rig.st.registration_refresh.is_none());
    assert_eq!(rig.st.refresh_rejections, 0);
    assert!(expiry(&rig) > before, "the expiry moved forward");
    let guard = rig.status.lock().unwrap();
    assert_eq!(guard.state, RegistrationState::Registered);
    let granted = guard
        .expires_at
        .unwrap()
        .duration_since(guard.registered_at.unwrap())
        .unwrap();
    assert_eq!(
        granted,
        Duration::from_secs(3000),
        "the lifetime the registrar granted"
    );
    drop(guard);
    assert!(rig.st.active_call.is_some());
    assert!(
        rig.session.transport().is_ok(),
        "the connection is the same one"
    );
}

#[test]
fn an_accepted_refresh_keeps_routing_the_answer_leaves_out() {
    let mut rig = Rig::new();
    rig.session.headers = vec![
        (
            "Service-Route".into(),
            "<sip:orig@ims.example.test;lr>".into(),
        ),
        (
            "P-Associated-URI".into(),
            "<sip:9000000000@ims.example.test>".into(),
        ),
    ];
    rig.drive().unwrap();
    let request = rig.received().unwrap();
    let contact = rig.contact(3600);
    let reply = rig.reply_to(
        &request,
        200,
        &[
            ("Contact", &contact),
            ("P-Associated-URI", "<sip:9000000001@ims.example.test>"),
        ],
    );
    assert!(rig.answer(&reply));
    let header = |name: &str| {
        rig.session
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    assert_eq!(
        header("Service-Route").as_deref(),
        Some("<sip:orig@ims.example.test;lr>")
    );
    assert_eq!(
        header("P-Associated-URI").as_deref(),
        Some("<sip:9000000001@ims.example.test>"),
        "what the answer does say replaces the old value"
    );
}

#[test]
fn an_answer_that_is_not_ours_does_not_confirm_the_refresh() {
    let mut rig = Rig::new();
    rig.drive().unwrap();
    let request = rig.received().unwrap();
    let before = expiry(&rig);

    // An OPTIONS probe's answer: same Call-ID, possibly the same number.
    let mut probe = rig.reply_to(&request, 200, &[]);
    probe.headers[1].1 = probe.headers[1].1.replace("REGISTER", "OPTIONS");
    assert!(!rig.answer(&probe));
    // An answer for a different Call-ID.
    let mut stranger = rig.reply_to(&request, 200, &[]);
    stranger.headers[0].1 = "someone-else".into();
    assert!(!rig.answer(&stranger));

    assert!(
        rig.st.registration_refresh.is_some(),
        "still waiting for the real answer"
    );
    assert_eq!(expiry(&rig), before);
}

#[test]
fn a_provisional_answer_keeps_the_refresh_pending() {
    let mut rig = Rig::new();
    rig.drive().unwrap();
    let request = rig.received().unwrap();
    let reply = rig.reply_to(&request, 100, &[]);
    assert!(rig.answer(&reply));
    assert!(rig.st.registration_refresh.is_some());
}

#[test]
fn a_late_answer_after_the_refresh_ended_is_ignored() {
    let mut rig = Rig::new();
    rig.drive().unwrap();
    let request = rig.received().unwrap();
    let contact = rig.contact(3000);
    let reply = rig.reply_to(&request, 200, &[("Contact", &contact)]);
    assert!(rig.answer(&reply));
    let after = expiry(&rig);
    assert!(!rig.answer(&reply), "nothing is pending any more");
    assert_eq!(expiry(&rig), after);
}

#[test]
fn a_zero_lifetime_is_a_lost_binding_that_needs_a_full_registration() {
    let mut rig = Rig::new();
    rig.drive().unwrap();
    let request = rig.received().unwrap();
    let contact = rig.contact(0);
    let reply = rig.reply_to(&request, 200, &[("Contact", &contact)]);
    assert!(rig.answer(&reply));
    assert!(rig.st.force_renewal);
}

fn refused(rig: &mut Rig, status: u16, extra: &[(&str, &str)]) {
    rig.st.registration_refresh = None;
    // Past any backoff a previous failure set.
    rig.st.next_renewal_attempt = None;
    rig.drive().unwrap();
    let request = rig.received().expect("a refresh was sent");
    let reply = rig.reply_to(&request, status, extra);
    assert!(rig.answer(&reply));
}

#[test]
fn statuses_that_need_the_initial_procedure_ask_for_a_full_registration() {
    for status in [403, 408, 500, 504, 401] {
        let mut rig = Rig::new();
        refused(&mut rig, status, &[]);
        assert!(
            rig.st.force_renewal,
            "{status} must lead to a full re-registration"
        );
        assert!(rig.st.registration_refresh.is_none());
    }
}

#[test]
fn a_rejected_refresh_does_not_discard_the_binding_or_end_the_call() {
    let mut rig = Rig::new();
    rig.st.active_call = Some(test_active_call("c1", "our-to-tag", CALLER_FROM));
    refused(&mut rig, 403, &[]);
    assert!(rig.st.active_call.is_some());
    let guard = rig.status.lock().unwrap();
    assert_eq!(
        guard.state,
        RegistrationState::Registered,
        "the earlier grant still holds until it expires"
    );
    assert!(guard.last_failure.is_some());
}

#[test]
fn a_recovery_asked_for_during_a_call_waits_for_the_call() {
    let mut rig = Rig::new();
    rig.st.active_call = Some(test_active_call("c1", "our-to-tag", CALLER_FROM));
    refused(&mut rig, 403, &[]);
    assert_eq!(
        rig.st
            .maintenance
            .decide(Maintenance::RegistrationRecovery, rig.st.busy()),
        MaintenanceDecision::Defer
    );
}

#[test]
fn a_long_service_unavailable_marks_the_proxy_and_fails_over() {
    let mut rig = Rig::new();
    rig.pool("127.0.0.2");
    refused(&mut rig, 503, &[("Retry-After", "3600")]);
    let now = SystemTime::now();
    assert!(
        rig.st.pool.is_marked(rig.session.pcscf_addr.ip(), now),
        "an hour-long hint must not be waited out on the same proxy"
    );
    assert!(
        rig.st.force_renewal,
        "the next registration goes through the other proxy"
    );
    assert!(rig.st.next_renewal_attempt.is_none(), "and at once");
}

#[test]
fn a_short_service_unavailable_retries_the_same_proxy_after_the_wait() {
    let mut rig = Rig::new();
    rig.pool("127.0.0.2");
    refused(&mut rig, 503, &[("Retry-After", "30")]);
    assert!(!rig
        .st
        .pool
        .is_marked(rig.session.pcscf_addr.ip(), SystemTime::now()));
    assert!(!rig.st.force_renewal);
    assert!(
        rig.st.next_renewal_attempt.is_some(),
        "a retry is scheduled on backoff"
    );
}

#[test]
fn a_service_unavailable_is_not_a_rejection_of_the_mechanism() {
    let mut rig = Rig::new();
    rig.pool("127.0.0.2");
    for _ in 0..3 {
        refused(&mut rig, 503, &[("Retry-After", "30")]);
    }
    assert_eq!(rig.st.refresh_rejections, 0);
    assert!(!rig.st.refresh_fallback);
}

#[test]
fn consecutive_rejections_switch_the_next_cycle_to_a_full_registration() {
    let mut rig = Rig::new();
    for n in 1..=REJECTIONS_BEFORE_FALLBACK {
        assert!(!rig.st.refresh_fallback, "not before rejection {n}");
        refused(&mut rig, 403, &[]);
        rig.st.force_renewal = false;
    }
    assert!(rig.st.refresh_fallback);
    assert!(
        !rig.st
            .in_place_refresh_possible(&rig.session, Some(expiry(&rig))),
        "the next cycle must not try in place"
    );
}

#[test]
fn a_successful_full_registration_ends_the_fallback_and_in_place_resumes() {
    let mut rig = Rig::new();
    rig.st.refresh_fallback = true;
    rig.st.refresh_rejections = REJECTIONS_BEFORE_FALLBACK;
    rig.st.on_renewal_success();
    assert!(!rig.st.refresh_fallback);
    assert_eq!(rig.st.refresh_rejections, 0);
    assert!(rig
        .st
        .in_place_refresh_possible(&rig.session, Some(expiry(&rig))));
}

#[test]
fn an_accepted_refresh_resets_the_rejection_count() {
    let mut rig = Rig::new();
    refused(&mut rig, 403, &[]);
    rig.st.force_renewal = false;
    assert_eq!(rig.st.refresh_rejections, 1);
    rig.st.registration_refresh = None;
    rig.st.next_renewal_attempt = None;
    rig.drive().unwrap();
    let request = rig.received().unwrap();
    let contact = rig.contact(3600);
    let reply = rig.reply_to(&request, 200, &[("Contact", &contact)]);
    assert!(rig.answer(&reply));
    assert_eq!(rig.st.refresh_rejections, 0);
}

#[test]
fn interval_too_brief_is_retried_once_in_place_and_does_not_count_as_a_rejection() {
    let mut rig = Rig::new();
    rig.drive().unwrap();
    let request = rig.received().unwrap();
    let reply = rig.reply_to(&request, 423, &[("Min-Expires", "7200")]);
    assert!(rig.answer(&reply));
    let retry = rig
        .received()
        .expect("a second REGISTER with the larger interval");
    assert!(retry.contains("Expires: 7200\r\n"));
    assert!(rig.st.registration_refresh.is_some());
    assert_eq!(rig.st.refresh_rejections, 0);
}

#[test]
fn a_lapsed_binding_is_refreshed_in_place_only_while_a_call_holds_the_line() {
    let mut rig = Rig::new();
    let lapsed = Some(SystemTime::now() - Duration::from_secs(5));
    assert!(
        !rig.st.in_place_refresh_possible(&rig.session, lapsed),
        "idle and lapsed: re-register in full"
    );
    rig.st.active_call = Some(test_active_call("c1", "our-to-tag", CALLER_FROM));
    assert!(
        rig.st.in_place_refresh_possible(&rig.session, lapsed),
        "a call is up: keep trying the refresh, recover when it ends"
    );
}

#[test]
fn an_expiry_during_a_call_is_reported_without_ending_the_call() {
    let mut rig = Rig::new();
    rig.st.active_call = Some(test_active_call("c1", "our-to-tag", CALLER_FROM));
    rig.status.lock().unwrap().expires_at = Some(SystemTime::now() - Duration::from_secs(1));
    refused(&mut rig, 500, &[]);
    assert!(rig.st.active_call.is_some());
    assert_eq!(rig.status.lock().unwrap().state, RegistrationState::Failed);
    assert!(
        rig.st.force_renewal,
        "re-registration is owed once the call ends"
    );
}

#[test]
fn a_spent_budget_mid_call_is_owed_not_taken() {
    let mut rig = Rig::new();
    rig.st.active_call = Some(test_active_call("c1", "our-to-tag", CALLER_FROM));
    rig.st.consecutive_renewal_failures = MAX_CONSECUTIVE_RENEWAL_FAILURES - 1;
    rig.pool("127.0.0.2");
    refused(&mut rig, 503, &[("Retry-After", "30")]);
    assert!(rig.st.budget_exit_owed.is_some());
    assert!(rig.st.active_call.is_some());
}

#[test]
fn a_registration_without_cached_credentials_is_never_refreshed_in_place() {
    let mut rig = Rig::new();
    rig.session.registration_auth = None;
    assert!(!rig
        .st
        .in_place_refresh_possible(&rig.session, Some(expiry(&rig))));
}

#[test]
fn a_full_registration_request_pre_empts_the_refresh() {
    let mut rig = Rig::new();
    rig.st.force_renewal = true;
    rig.drive().unwrap();
    assert!(rig.received().is_none());
    assert!(rig.ctx().status.lock().is_ok());
}

/// A pass of the loop's front half for one queued message: what
/// `dispatch_loop` does between handlers.
fn one_pass(rig: &mut Rig) -> Option<bool> {
    let ctx = RefreshCtx {
        status: &rig.status,
        obs: &rig.obs,
        reg_cfg: &rig.cfg,
    };
    rig.st
        .begin_pass(&mut rig.session, &ctx, &mut rig.link)
        .unwrap();
    let (message, _) = rig
        .st
        .held
        .pop_front()
        .or_else(|| rig.link.rx.try_recv().ok())?;
    match message {
        SipMessage::Response(response) => Some(
            rig.st
                .handle_registration_answer(&mut rig.session, &ctx, &response)
                .unwrap(),
        ),
        _ => Some(false),
    }
}

fn queue_response(rig: &Rig, response: SipResponse) {
    let sink = rig.session.transport().unwrap().sink().unwrap();
    rig.link
        .tx
        .send((SipMessage::Response(response), sink))
        .unwrap();
}

#[test]
fn an_answer_that_arrived_while_a_call_rang_is_honoured_even_after_timer_f() {
    // 100 s into the 128 s transaction an INVITE arrives and rings for 40 s.
    // The carrier answered the refresh during the ring; the loop is back at
    // 140 s. The answer was in time and must not be thrown away.
    let mut rig = Rig::new();
    rig.drive().unwrap();
    let request = rig.received().unwrap();
    let contact = rig.contact(3000);
    let reply = rig.reply_to(&request, 200, &[("Contact", &contact)]);
    queue_response(&rig, reply);
    rig.st
        .registration_refresh
        .as_mut()
        .unwrap()
        .set_deadline(Instant::now() - Duration::from_secs(12));
    rig.st.after_blocking_handler = true;

    assert_eq!(one_pass(&mut rig), Some(true));

    assert!(rig.st.registration_refresh.is_none());
    assert_eq!(
        rig.status.lock().unwrap().state,
        RegistrationState::Registered
    );
    assert_eq!(rig.st.refresh_rejections, 0);
    assert!(!rig.st.force_renewal && rig.st.next_renewal_attempt.is_none());
}

#[test]
fn without_an_answer_the_refresh_still_times_out_after_the_block() {
    let mut rig = Rig::new();
    // The connection was already rebuilt for this refresh once.
    rig.st.refresh_repaired = true;
    rig.drive().unwrap();
    rig.received().unwrap();
    rig.st
        .registration_refresh
        .as_mut()
        .unwrap()
        .set_deadline(Instant::now() - Duration::from_secs(12));
    rig.st.after_blocking_handler = true;

    assert_eq!(one_pass(&mut rig), None);

    assert!(rig.st.registration_refresh.is_none(), "Timer F ends it");
    assert!(
        rig.st.next_renewal_attempt.is_some(),
        "and a retry is scheduled"
    );
}

#[test]
fn timers_are_not_judged_while_held_messages_wait() {
    let mut rig = Rig::new();
    rig.drive().unwrap();
    let request = rig.received().unwrap();
    let reply = rig.reply_to(&request, 100, &[]);
    queue_response(&rig, reply);
    rig.st
        .registration_refresh
        .as_mut()
        .unwrap()
        .set_deadline(Instant::now() - Duration::from_secs(1));
    rig.st.after_blocking_handler = true;
    let ctx = RefreshCtx {
        status: &rig.status,
        obs: &rig.obs,
        reg_cfg: &rig.cfg,
    };
    rig.st
        .begin_pass(&mut rig.session, &ctx, &mut rig.link)
        .unwrap();
    assert_eq!(rig.st.held.len(), 1);
    assert!(
        rig.st.registration_refresh.is_some(),
        "the pending refresh is not failed before the waiting message is read"
    );
}

#[test]
fn the_latest_refresh_outcome_is_kept_for_the_status() {
    let mut rig = Rig::new();
    assert!(rig.st.last_refresh.is_none());
    refused(&mut rig, 403, &[]);
    assert_eq!(rig.st.last_refresh.as_ref().unwrap().1, "rejected 403");
    rig.st.force_renewal = false;

    rig.st.registration_refresh = None;
    rig.st.next_renewal_attempt = None;
    rig.drive().unwrap();
    let request = rig.received().unwrap();
    let contact = rig.contact(3600);
    let reply = rig.reply_to(&request, 200, &[("Contact", &contact)]);
    assert!(rig.answer(&reply));
    assert_eq!(rig.st.last_refresh.as_ref().unwrap().1, "ok");

    rig.st.registration_refresh = None;
    rig.st.next_renewal_attempt = None;
    {
        let mut guard = rig.status.lock().unwrap();
        guard.registered_at = Some(SystemTime::now() - Duration::from_secs(3100));
        guard.expires_at = Some(SystemTime::now() + Duration::from_secs(500));
    }
    rig.drive().unwrap();
    rig.received().unwrap();
    rig.st.refresh_repaired = true;
    rig.st
        .registration_refresh
        .as_mut()
        .unwrap()
        .set_deadline(Instant::now() - Duration::from_secs(1));
    rig.drive().unwrap();
    assert_eq!(rig.st.last_refresh.as_ref().unwrap().1, "timeout");
}

#[test]
fn a_dead_connection_is_rebuilt_and_the_refresh_sent_again_without_blaming_the_proxy() {
    // The network closed the idle Gm connection: the send fails. The refresh
    // must not be reported as a P-CSCF fault; it rebuilds the connection and
    // goes out on the new one.
    let mut rig = Rig::new();
    rig.pool("127.0.0.2");
    rig.session.transport = None;

    rig.drive().unwrap();

    assert_eq!(rig.link.repairs, 1);
    assert!(
        rig.received().is_some(),
        "the refresh went out on the rebuilt connection"
    );
    assert!(rig.st.registration_refresh.is_some());
    assert!(!rig
        .st
        .pool
        .is_marked(rig.session.pcscf_addr.ip(), SystemTime::now()));
    assert!(!rig.st.force_renewal);
    assert_eq!(rig.st.refresh_rejections, 0);
}

#[test]
fn an_unanswered_refresh_rebuilds_the_connection_once_and_tries_again() {
    // The first write into a connection the peer has closed succeeds and is
    // never answered. Timer F ends it; the connection is rebuilt and the
    // refresh sent again.
    let mut rig = Rig::new();
    rig.drive().unwrap();
    rig.received().unwrap();
    rig.st
        .registration_refresh
        .as_mut()
        .unwrap()
        .set_deadline(Instant::now() - Duration::from_secs(1));

    rig.drive().unwrap();

    assert_eq!(rig.link.repairs, 1);
    assert!(
        rig.received().is_some(),
        "sent again on the rebuilt connection"
    );
    assert!(rig.st.registration_refresh.is_some());
    assert!(
        rig.st.next_renewal_attempt.is_none(),
        "no backoff for the first loss"
    );
}

#[test]
fn a_second_loss_is_not_repaired_again_and_goes_to_the_normal_failure_path() {
    let mut rig = Rig::new();
    rig.pool("127.0.0.2");
    rig.drive().unwrap();
    rig.received().unwrap();
    for _ in 0..2 {
        rig.st
            .registration_refresh
            .as_mut()
            .unwrap()
            .set_deadline(Instant::now() - Duration::from_secs(1));
        rig.drive().unwrap();
        let _ = rig.received();
    }
    assert_eq!(rig.link.repairs, 1, "one rebuild per refresh, not a loop");
    assert!(rig.st.registration_refresh.is_none());
    assert!(
        rig.st.force_renewal,
        "now it is the proxy's fault: marked, and the next registration goes to the other one"
    );
    assert_eq!(rig.st.last_refresh.as_ref().unwrap().1, "timeout");
}

#[test]
fn a_rebuild_that_fails_falls_back_to_the_failure_path() {
    let mut rig = Rig::new();
    rig.link.repair_fails = true;
    rig.session.transport = None;

    rig.drive().unwrap();

    assert_eq!(rig.link.repairs, 1);
    assert!(rig.st.registration_refresh.is_none());
    assert!(rig.st.next_renewal_attempt.is_some() || rig.st.force_renewal);
    assert_eq!(rig.st.last_refresh.as_ref().unwrap().1, "error");
}

#[test]
fn an_accepted_refresh_allows_the_next_cycle_its_own_rebuild() {
    let mut rig = Rig::new();
    rig.st.refresh_repaired = true;
    rig.drive().unwrap();
    let request = rig.received().unwrap();
    let contact = rig.contact(3600);
    let reply = rig.reply_to(&request, 200, &[("Contact", &contact)]);
    assert!(rig.answer(&reply));
    assert!(!rig.st.refresh_repaired);
}

#[test]
fn a_rejected_refresh_is_not_a_connection_problem_and_is_not_repaired() {
    let mut rig = Rig::new();
    refused(&mut rig, 403, &[]);
    assert_eq!(rig.link.repairs, 0);
}

#[test]
fn every_saved_route_survives_an_answer_that_omits_them() {
    let mut rig = Rig::new();
    rig.session.headers = vec![
        (
            "Service-Route".into(),
            "<sip:first@ims.example.test;lr>".into(),
        ),
        (
            "Service-Route".into(),
            "<sip:second@ims.example.test;lr>".into(),
        ),
    ];
    rig.drive().unwrap();
    let request = rig.received().unwrap();
    let contact = rig.contact(3600);
    let reply = rig.reply_to(&request, 200, &[("Contact", &contact)]);
    assert!(rig.answer(&reply));
    let routes: Vec<_> = rig
        .session
        .headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case("Service-Route"))
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(
        routes,
        [
            "<sip:first@ims.example.test;lr>",
            "<sip:second@ims.example.test;lr>"
        ],
        "both entries, in order"
    );
}

#[test]
fn a_silent_proxy_is_marked_once_the_connection_rebuild_did_not_help() {
    let mut rig = Rig::new();
    rig.pool("127.0.0.2");
    rig.drive().unwrap();
    rig.received().unwrap();
    for _ in 0..2 {
        rig.st
            .registration_refresh
            .as_mut()
            .unwrap()
            .set_deadline(Instant::now() - Duration::from_secs(1));
        rig.drive().unwrap();
        let _ = rig.received();
    }
    assert!(
        rig.st
            .pool
            .is_marked(rig.session.pcscf_addr.ip(), SystemTime::now()),
        "two unanswered refreshes in a row are the proxy's fault"
    );
}

#[test]
fn the_carriers_pause_is_a_floor_under_the_retry() {
    let mut rig = Rig::new();
    rig.pool("127.0.0.2");
    refused(&mut rig, 503, &[("Retry-After", "30")]);
    let at = rig.st.next_renewal_attempt.expect("a retry is scheduled");
    assert!(
        at >= Instant::now() + Duration::from_secs(28),
        "not the 5 s initial backoff"
    );
}

#[test]
fn a_zero_lifetime_removes_the_binding_from_the_status() {
    let mut rig = Rig::new();
    rig.drive().unwrap();
    let request = rig.received().unwrap();
    let contact = rig.contact(0);
    let reply = rig.reply_to(&request, 200, &[("Contact", &contact)]);
    assert!(rig.answer(&reply));
    let guard = rig.status.lock().unwrap();
    assert_eq!(guard.state, RegistrationState::Failed);
    assert!(guard.expires_at.unwrap() <= SystemTime::now());
    assert!(rig.st.force_renewal);
}

#[test]
fn a_connection_the_network_closed_is_rebuilt_before_the_refresh_is_sent() {
    // The reader ended: the peer closed the idle connection. No send is needed
    // to find that out, and the refresh must not be written into it.
    let mut rig = Rig::new();
    rig.pool("127.0.0.2");
    rig.link.alive = false;

    rig.drive().unwrap();

    assert_eq!(rig.link.repairs, 1);
    assert!(rig.received().is_some(), "sent on the rebuilt connection");
    assert!(!rig
        .st
        .pool
        .is_marked(rig.session.pcscf_addr.ip(), SystemTime::now()));
}
