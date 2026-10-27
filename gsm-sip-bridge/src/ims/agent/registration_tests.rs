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
    inbound: crate::ims::session::Inbound,
    peer: std::net::SocketAddr,
    repairs: u32,
    repair_fails: bool,
    alive: bool,
}

impl FakeLink {
    fn new(peer: std::net::SocketAddr) -> Self {
        Self {
            inbound: crate::ims::session::Inbound::detached(),
            peer,
            repairs: 0,
            repair_fails: false,
            alive: true,
        }
    }
}

impl GmLink for FakeLink {
    fn queue(&self) -> &mpsc::Receiver<(SipMessage, SipSink)> {
        &self.inbound.rx
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

    fn read_connection(
        &self,
        transport: &SipTransport,
    ) -> BridgeResult<crate::ims::session::ClientReader> {
        crate::ims::session::read_client_connection(transport, &self.inbound)
    }

    fn adopt_reader(
        &self,
        reader: crate::ims::session::ClientReader,
    ) -> crate::ims::session::ClientReader {
        crate::ims::session::adopt_client_reader(&self.inbound, reader)
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
            .handle_registration_answer(&mut self.session, &ctx, &mut self.link, response)
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
    for status in [403, 408, 500, 504] {
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
        .or_else(|| rig.link.inbound.rx.try_recv().ok())?;
    match message {
        SipMessage::Response(response) => Some(
            rig.st
                .handle_registration_answer(&mut rig.session, &ctx, &mut rig.link, &response)
                .unwrap(),
        ),
        _ => Some(false),
    }
}

fn queue_response(rig: &Rig, response: SipResponse) {
    let sink = rig.session.transport().unwrap().sink().unwrap();
    rig.link
        .inbound
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

#[test]
fn a_spent_budget_while_a_call_rings_is_owed_not_raised() {
    let mut rig = Rig::new();
    rig.st.consecutive_renewal_failures = MAX_CONSECUTIVE_RENEWAL_FAILURES - 1;
    rig.st.ringing = true;
    rig.drive().unwrap();
    rig.received().unwrap();
    rig.st
        .registration_refresh
        .as_mut()
        .unwrap()
        .set_deadline(Instant::now() - Duration::from_secs(1));
    rig.st.refresh_repaired = true;

    assert!(rig.drive().is_ok(), "the ring is not cut short");
    assert!(
        rig.st.budget_exit_owed.is_some(),
        "the exit waits for the line"
    );
    rig.st.ringing = false;
    assert!(rig.st.take_owed_budget_exit().is_err());
}

#[test]
fn a_silent_open_connection_is_not_rebuilt_while_a_call_may_be_using_it() {
    let mut rig = Rig::new();
    rig.st.ringing = true;
    rig.drive().unwrap();
    rig.received().unwrap();
    rig.st
        .registration_refresh
        .as_mut()
        .unwrap()
        .set_deadline(Instant::now() - Duration::from_secs(1));

    rig.drive().unwrap();

    assert_eq!(rig.link.repairs, 0, "the connection is left alone");
    assert!(rig.st.registration_refresh.is_none());
    assert_eq!(rig.st.last_refresh.as_ref().unwrap().1, "timeout");
}

#[test]
fn a_dead_connection_is_rebuilt_even_while_a_call_is_up() {
    let mut rig = Rig::new();
    rig.st.ringing = true;
    rig.link.alive = false;

    rig.drive().unwrap();

    assert_eq!(rig.link.repairs, 1);
    assert!(
        rig.st.registration_refresh.is_some(),
        "and the refresh is sent again"
    );
}

// --- network-requested re-authentication (Slice C) ---------------------------

use crate::ims::gm_ipsec::fake_kernel::Kernel;
use crate::ims::gm_ipsec::{SaSet, XfrmBackend};
use crate::ims::registration_auth::scripted::{
    card, challenge_header, success_reply, sync_failure_reply,
};

impl Rig {
    /// A registration with a Gm association, on a kernel that has the working
    /// generation installed. The SIM answers from `replies`.
    fn with_sa(replies: Vec<String>, sim_delay: Duration) -> (Self, Kernel) {
        let mut rig = Rig::new();
        rig.session.gm_state = crate::ims::test_session_with_sa(
            None,
            rig.peer.local_addr().unwrap(),
            Some("old-verify"),
        )
        .gm_state;
        rig.session.use_tcp = false;
        let gm = rig.session.gm_state.as_ref().unwrap();
        let working = SaSet {
            endpoints: gm.endpoints.clone(),
            ours: gm.proposal.clone(),
            theirs: gm.theirs.clone(),
        };
        let kernel = Kernel::default();
        let keys =
            crate::ims::gm_ipsec::SaKeys::derive(&working.theirs, &[1; 16], &[2; 16]).unwrap();
        for state in working.states() {
            kernel.state_add(&state, &keys).unwrap();
        }
        for policy in working.policies() {
            kernel.policy_add(&policy).unwrap();
        }
        let table = kernel.clone();
        rig.st.reauth_env = registration_refresh::ReauthEnv {
            open_card: Arc::new(card(replies, sim_delay)),
            xfrm: Arc::new(move || Box::new(table.clone())),
        };
        (rig, kernel)
    }

    /// The challenge a registrar sends to a refresh: a nonce for the SIM, and
    /// replacement associations whose protected server port is the peer's.
    fn challenge(&self, request: &str) -> SipResponse {
        let offer = format!(
            "ipsec-3gpp; q=0.5; alg=hmac-sha-1-96; ealg=null; spi-c=9001; spi-s=9002; port-c=7001; port-s={}",
            self.peer.local_addr().unwrap().port()
        );
        self.reply_to(
            request,
            401,
            &[
                ("WWW-Authenticate", &challenge_header()),
                ("Security-Server", &offer),
            ],
        )
    }

    /// Drives the loop until the next REGISTER goes out, then reads it.
    fn next_register(&mut self) -> (String, std::net::SocketAddr) {
        let until = Instant::now() + Duration::from_secs(5);
        let mut bytes = [0; 8192];
        loop {
            self.drive().unwrap();
            if let Ok((size, from)) = self.peer.recv_from(&mut bytes) {
                return (String::from_utf8(bytes[..size].to_vec()).unwrap(), from);
            }
            assert!(Instant::now() < until, "no REGISTER was sent");
        }
    }

    fn header(request: &str, name: &str) -> String {
        request
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{name}: ")))
            .unwrap_or_default()
            .to_string()
    }
}

fn client_port(request: &str) -> u16 {
    Rig::header(request, "Security-Client")
        .split(';')
        .find_map(|p| p.trim().strip_prefix("port-c="))
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn a_challenge_during_a_call_is_answered_in_the_background_and_activated_on_success() {
    let (mut rig, kernel) = Rig::with_sa(vec![success_reply()], Duration::from_millis(250));
    rig.st.active_call = Some(test_active_call("c1", "our-to-tag", CALLER_FROM));
    let working_port = rig.session.gm_state.as_ref().unwrap().proposal.port_c;
    let server_port = rig.session.gm_state.as_ref().unwrap().proposal.port_s;
    let old_states = kernel.states();

    rig.drive().unwrap();
    let refresh = rig.received().expect("the refresh was sent");
    assert!(rig.answer(&rig.challenge(&refresh)));

    // The SIM is still working: the loop is not held up, the registration is
    // untouched, and no full re-registration was asked for.
    let started = Instant::now();
    rig.drive().unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "the loop must not wait for the SIM"
    );
    assert!(rig.st.challenged.is_some());
    assert!(!rig.st.force_renewal);
    assert_eq!(
        kernel.states(),
        old_states,
        "nothing is installed before the SIM answers"
    );
    assert!(rig.st.active_call.is_some());

    // The answer goes out from the proposed new client port, on the new
    // connection, over the associations the challenge offered.
    let (answer, from) = rig.next_register();
    assert!(answer.contains("Authorization: Digest username=\"001010000000001@"));
    assert!(
        !answer.contains("nonce=\"cached\""),
        "the cached authorization is not replayed"
    );
    assert_eq!(
        Rig::header(&answer, "Call-ID"),
        Rig::header(&refresh, "Call-ID")
    );
    assert_eq!(
        from.port(),
        client_port(&answer),
        "sent from the proposed client port"
    );
    assert_ne!(from.port(), working_port);
    assert!(Rig::header(&answer, "Security-Verify").contains("spi-c=9001"));
    assert!(
        kernel.states().len() > old_states.len(),
        "the new associations are installed beside the old"
    );
    let cseq = |r: &str| {
        Rig::header(r, "CSeq")
            .split(' ')
            .next()
            .unwrap()
            .parse::<u32>()
            .unwrap()
    };
    assert_eq!(cseq(&answer), cseq(&refresh) + 1);

    // Until the network confirms, the working association is what carries
    // traffic.
    assert_eq!(
        rig.session.gm_state.as_ref().unwrap().proposal.port_c,
        working_port
    );

    let ok = rig.reply_to(&answer, 200, &[("Contact", &rig.contact(3600))]);
    assert!(rig.answer(&ok));

    let gm = rig.session.gm_state.as_ref().unwrap();
    assert_eq!(
        gm.proposal.port_s, server_port,
        "the protected server port does not change"
    );
    assert_eq!(gm.proposal.port_c, from.port(), "the client port does");
    assert_eq!(gm.theirs.spi_c, 9001);
    assert_eq!(rig.session.local_addr.port(), from.port());
    assert!(rig
        .session
        .registration_auth
        .as_ref()
        .unwrap()
        .authorization
        .starts_with("Digest username="));
    assert!(rig.st.active_call.is_some(), "the call was never touched");
    assert_eq!(rig.st.retired_gm.len(), 1);
    assert!(!rig.st.force_renewal);
    assert_eq!(rig.st.last_refresh.as_ref().unwrap().1, "ok");
    assert!(expiry(&rig) > SystemTime::now() + Duration::from_secs(3500));
}

#[test]
fn the_retired_association_stays_for_the_drain_and_longer_while_a_call_is_being_placed() {
    let (mut rig, kernel) = Rig::with_sa(vec![success_reply()], Duration::ZERO);
    rig.drive().unwrap();
    let refresh = rig.received().unwrap();
    rig.answer(&rig.challenge(&refresh));
    let (answer, _) = rig.next_register();
    let ok = rig.reply_to(&answer, 200, &[("Contact", &rig.contact(3600))]);
    assert!(rig.answer(&ok));
    assert_eq!(kernel.states().len(), 8);

    let now = Instant::now();
    rig.st.drive_retired(now + Duration::from_secs(100), false);
    assert_eq!(rig.st.retired_gm.len(), 1, "128 s have not passed");

    // A call being placed may still be waiting on the old connection.
    rig.st.drive_retired(now + Duration::from_secs(200), true);
    assert_eq!(rig.st.retired_gm.len(), 1);
    assert_eq!(kernel.states().len(), 8);

    rig.st.drive_retired(now + Duration::from_secs(200), false);
    assert!(rig.st.retired_gm.is_empty());
    assert_eq!(kernel.states().len(), 4, "only the new generation remains");

    let (mut again, _) = Rig::with_sa(vec![success_reply()], Duration::ZERO);
    again.drive().unwrap();
    let refresh = again.received().unwrap();
    again.answer(&again.challenge(&refresh));
    let (answer, _) = again.next_register();
    again.answer(&again.reply_to(&answer, 200, &[("Contact", &again.contact(3600))]));
    again
        .st
        .drive_retired(Instant::now() + Duration::from_secs(700), true);
    assert!(
        again.st.retired_gm.is_empty(),
        "no call holds it past the limit"
    );
}

#[test]
fn an_answer_the_network_does_not_confirm_leaves_the_working_association_exactly_as_it_was() {
    for how in ["timer-f", "rejected"] {
        let (mut rig, kernel) = Rig::with_sa(vec![success_reply()], Duration::ZERO);
        rig.st.active_call = Some(test_active_call("c1", "our-to-tag", CALLER_FROM));
        let (states, policies) = (kernel.states(), kernel.policies());
        let (local, proposal_port) = (
            rig.session.local_addr,
            rig.session.gm_state.as_ref().unwrap().proposal.port_c,
        );
        rig.drive().unwrap();
        let refresh = rig.received().unwrap();
        rig.answer(&rig.challenge(&refresh));
        let (answer, _) = rig.next_register();
        assert!(kernel.states().len() > states.len());

        if how == "timer-f" {
            rig.st
                .registration_refresh
                .as_mut()
                .unwrap()
                .set_deadline(Instant::now());
            rig.drive().unwrap();
        } else {
            assert!(rig.answer(&rig.reply_to(&answer, 403, &[])));
        }

        assert!(rig.st.force_renewal, "{how}: a full registration follows");
        assert!(rig.st.registration_refresh.is_none(), "{how}");
        assert_eq!(
            kernel.states(),
            states,
            "{how}: the staged associations are gone"
        );
        assert_eq!(kernel.policies(), policies, "{how}");
        assert_eq!(rig.session.local_addr, local, "{how}");
        assert_eq!(
            rig.session.gm_state.as_ref().unwrap().proposal.port_c,
            proposal_port,
            "{how}"
        );
        assert!(rig.st.active_call.is_some(), "{how}: the call survives");
        assert!(rig.st.retired_gm.is_empty(), "{how}");

        // A late answer from the abandoned attempt changes nothing.
        let late = rig.reply_to(&answer, 200, &[("Contact", &rig.contact(3600))]);
        assert!(!rig.answer(&late), "{how}: nobody is waiting for it");
        assert_eq!(rig.session.local_addr, local, "{how}");
        assert_eq!(kernel.states(), states, "{how}");
    }
}

#[test]
fn a_sim_that_cannot_answer_falls_back_to_a_full_registration_and_installs_nothing() {
    let (mut rig, kernel) = Rig::with_sa(vec![success_reply()], Duration::ZERO);
    rig.st.reauth_env.open_card = Arc::new(|_| Err(BridgeError::Ims("no modem".into())));
    let states = kernel.states();
    rig.drive().unwrap();
    let refresh = rig.received().unwrap();
    rig.answer(&rig.challenge(&refresh));
    let until = Instant::now() + Duration::from_secs(5);
    while !rig.st.force_renewal {
        rig.drive().unwrap();
        assert!(Instant::now() < until, "never gave up");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(rig.st.challenged.is_none());
    assert_eq!(kernel.states(), states);
    assert!(rig.received().is_none(), "no answer was sent");
}

#[test]
fn a_challenge_without_new_associations_is_answered_on_the_working_connection() {
    let (mut rig, kernel) = Rig::with_sa(vec![success_reply()], Duration::ZERO);
    let states = kernel.states();
    let working = rig.session.local_addr;
    rig.drive().unwrap();
    let refresh = rig.received().unwrap();
    let bare = rig.reply_to(&refresh, 401, &[("WWW-Authenticate", &challenge_header())]);
    rig.answer(&bare);
    let (answer, from) = rig.next_register();
    assert_eq!(from, working, "sent from where the working connection is");
    assert_eq!(kernel.states(), states);
    let ok = rig.reply_to(&answer, 200, &[("Contact", &rig.contact(3600))]);
    assert!(rig.answer(&ok));
    assert_eq!(rig.session.local_addr, working);
    assert!(rig.st.retired_gm.is_empty());
    assert!(rig
        .session
        .registration_auth
        .as_ref()
        .unwrap()
        .authorization
        .contains("username="));
}

#[test]
fn a_sequence_mismatch_is_resynchronised_and_the_fresh_challenge_answered() {
    let (mut rig, kernel) =
        Rig::with_sa(vec![sync_failure_reply(), success_reply()], Duration::ZERO);
    rig.drive().unwrap();
    let refresh = rig.received().unwrap();
    rig.answer(&rig.challenge(&refresh));

    // The first AUTHENTICATE fails its sequence check: AUTS goes to the network,
    // on the working connection and with nothing installed.
    let states = kernel.states();
    let (resync, from) = rig.next_register();
    assert!(resync.contains("auts=\""));
    assert_eq!(from, rig.session.local_addr);
    assert_eq!(kernel.states(), states);

    // The network answers with a fresh challenge; this one succeeds.
    rig.answer(&rig.challenge(&resync));
    let (answer, _) = rig.next_register();
    assert!(!answer.contains("auts="));
    assert!(answer.contains("response=\""));
    assert!(kernel.states().len() > states.len());
    let ok = rig.reply_to(&answer, 200, &[("Contact", &rig.contact(3600))]);
    assert!(rig.answer(&ok));
    assert_eq!(rig.session.gm_state.as_ref().unwrap().theirs.spi_c, 9001);
}

#[test]
fn a_network_that_keeps_asking_for_a_resynchronisation_is_given_up_on() {
    let (mut rig, _kernel) = Rig::with_sa(vec![sync_failure_reply()], Duration::ZERO);
    rig.drive().unwrap();
    let mut request = rig.received().unwrap();
    for _ in 0..5 {
        rig.answer(&rig.challenge(&request));
        if rig.st.force_renewal {
            break;
        }
        let (next, _) = rig.next_register();
        request = next;
    }
    assert!(rig.st.force_renewal, "it must not resynchronise forever");
}

#[test]
fn a_challenge_to_the_answer_itself_is_a_rejection_and_falls_back() {
    let (mut rig, kernel) = Rig::with_sa(vec![success_reply()], Duration::ZERO);
    let states = kernel.states();
    rig.drive().unwrap();
    let refresh = rig.received().unwrap();
    rig.answer(&rig.challenge(&refresh));
    let (answer, _) = rig.next_register();
    assert!(rig.answer(&rig.challenge(&answer)));
    assert!(rig.st.force_renewal);
    assert!(
        rig.st.challenged.is_none(),
        "it does not start a second round"
    );
    assert_eq!(kernel.states(), states);
}

#[test]
fn udp_replies_addressed_to_the_advertised_server_port_are_read() {
    // The registration's protected server port is a real socket here, as it is
    // in the agent (`spawn_gm_server`), feeding the same queue as the client
    // connection's reader. The network is free to answer to either.
    let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
    let server_port = probe.local_addr().unwrap().port();
    drop(probe);
    let (mut rig, _kernel) = Rig::with_sa(vec![success_reply()], Duration::ZERO);
    {
        let gm = rig.session.gm_state.as_mut().unwrap();
        gm.endpoints.local_s.set_port(server_port);
        gm.proposal.port_s = server_port;
    }
    rig.session.contact_addr.set_port(server_port);
    let _server = crate::ims::sip_client::spawn_gm_server(
        rig.session.gm_state.as_ref().unwrap().endpoints.local_s,
        false,
        rig.link.inbound.tx.clone(),
    )
    .unwrap();

    rig.drive().unwrap();
    let refresh = rig.received().unwrap();
    rig.answer(&rig.challenge(&refresh));
    let (answer, _) = rig.next_register();

    // The 200 comes to the server port, not to the new client port.
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    sender
        .send_to(
            format!(
                "SIP/2.0 200 OK\r\nCall-ID: {}\r\nCSeq: {}\r\nVia: {}\r\nContact: {}\r\nContent-Length: 0\r\n\r\n",
                Rig::header(&answer, "Call-ID"),
                Rig::header(&answer, "CSeq"),
                Rig::header(&answer, "Via"),
                Rig::header(&ok_contact(&rig), "Contact"),
            )
            .as_bytes(),
            ("127.0.0.1", server_port),
        )
        .unwrap();

    let (message, _) = rig
        .link
        .inbound
        .rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the reply to the server port reached the dispatch queue");
    let SipMessage::Response(response) = message else {
        panic!("expected a response");
    };
    assert!(rig.answer(&response), "and it confirmed the answer");
    assert_eq!(rig.session.gm_state.as_ref().unwrap().theirs.spi_c, 9001);
}

/// The Contact header of a 200 for this registration, as request-shaped text.
fn ok_contact(rig: &Rig) -> String {
    format!("Contact: {}\r\n", rig.contact(3600))
}
