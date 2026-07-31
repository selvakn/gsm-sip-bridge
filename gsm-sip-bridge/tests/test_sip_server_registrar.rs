//! The embedded SIP registrar's wire contract, exercised end to end.
//!
//! Both ends are real: the registrar binds a real UDP socket and the "phone" is
//! a second real `UdpSocket` speaking real SIP bytes at it. **No mocks** — the
//! registrar is pure Rust with no PJSIP dependency, so it runs here exactly as
//! it does in production, and the constitution's mock-justification requirement
//! has nothing to discharge.
//!
//! Every assertion below corresponds to a row in
//! `specs/024-sip-server-mode/contracts/sip-registrar.md`.

use std::net::UdpSocket;
use std::time::Duration;

use gsm_sip_bridge::config::secret::Secret;
use gsm_sip_bridge::config::{SipServerAccount, SipServerConfig};
use gsm_sip_bridge::sip::server::Registrar;

const REALM: &str = "test-realm";
const USER: &str = "1001";
const PASSWORD: &str = "s3cret";
const CONTACT: &str = "<sip:1001@192.168.1.50:5060>";

fn config() -> SipServerConfig {
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

/// A registrar on an ephemeral loopback port, plus a socket to talk to it.
struct Harness {
    registrar: Registrar,
    phone: UdpSocket,
}

impl Harness {
    fn new() -> Self {
        Self::with_config(config())
    }

    fn with_config(config: SipServerConfig) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind registrar");
        let registrar = Registrar::start_on(socket, &config).expect("start registrar");

        let phone = UdpSocket::bind("127.0.0.1:0").expect("bind phone");
        phone
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("phone timeout");
        phone.connect(registrar.local_addr()).expect("connect");

        Self { registrar, phone }
    }

    /// Sends `request` and returns the response, failing the test on silence —
    /// a dropped datagram is itself a contract violation (FR-015).
    fn round_trip(&self, request: &str) -> String {
        self.phone.send(request.as_bytes()).expect("send");
        let mut buf = [0u8; 8192];
        let len = self
            .phone
            .recv(&mut buf)
            .unwrap_or_else(|e| panic!("no response to:\n{request}\nerror: {e}"));
        String::from_utf8_lossy(&buf[..len]).into_owned()
    }

    /// Registers successfully and returns the `200 OK`.
    fn register_ok(&self, cseq: u32, call_id: &str) -> String {
        let challenge = self.round_trip(&register(cseq, call_id, None, None));
        let nonce = nonce_from(&challenge);
        let auth = authorization(USER, PASSWORD, &nonce, Some("00000001"));
        let response = self.round_trip(&register(cseq + 1, call_id, Some(&auth), None));
        assert_status(&response, 200);
        response
    }
}

fn status_of(response: &str) -> u16 {
    response
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in:\n{response}"))
}

fn assert_status(response: &str, want: u16) {
    assert_eq!(
        status_of(response),
        want,
        "expected {want}, got:\n{response}"
    );
}

fn header_of<'a>(response: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{}:", name.to_ascii_lowercase());
    response.lines().find_map(|line| {
        line.to_ascii_lowercase()
            .starts_with(&prefix)
            .then(|| line[prefix.len()..].trim())
    })
}

fn nonce_from(challenge: &str) -> String {
    let header = header_of(challenge, "WWW-Authenticate")
        .unwrap_or_else(|| panic!("no WWW-Authenticate in:\n{challenge}"));
    let start = header.find("nonce=\"").expect("nonce param") + "nonce=\"".len();
    let end = header[start..].find('"').expect("nonce end") + start;
    header[start..end].to_string()
}

fn register(cseq: u32, call_id: &str, authorization: Option<&str>, extra: Option<&str>) -> String {
    format!(
        "REGISTER sip:bridge SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.168.1.50:5060;branch=z9hG4bK{cseq}\r\n\
         From: <sip:{USER}@bridge>;tag=phone-tag\r\n\
         To: <sip:{USER}@bridge>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} REGISTER\r\n\
         Contact: {CONTACT}\r\n\
         User-Agent: TestPhone/1.0\r\n\
         {}{}\
         Content-Length: 0\r\n\r\n",
        authorization
            .map(|a| format!("Authorization: {a}\r\n"))
            .unwrap_or_default(),
        extra.unwrap_or(""),
    )
}

/// Builds the `Authorization` a conforming handset would send. Computed with
/// md5 here rather than with the bridge's own helper, so this test would catch
/// the bridge silently changing what it verifies against.
fn authorization(user: &str, password: &str, nonce: &str, nc: Option<&str>) -> String {
    let md5 = |s: &str| {
        use md5::{Digest, Md5};
        let mut hasher = Md5::new();
        hasher.update(s.as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let ha1 = md5(&format!("{user}:{REALM}:{password}"));
    let ha2 = md5("REGISTER:sip:bridge");
    match nc {
        Some(nc) => {
            let response = md5(&format!("{ha1}:{nonce}:{nc}:cnonce1:auth:{ha2}"));
            format!(
                "Digest username=\"{user}\", realm=\"{REALM}\", nonce=\"{nonce}\", \
                 uri=\"sip:bridge\", response=\"{response}\", qop=auth, nc={nc}, \
                 cnonce=\"cnonce1\", algorithm=MD5"
            )
        }
        None => {
            let response = md5(&format!("{ha1}:{nonce}:{ha2}"));
            format!(
                "Digest username=\"{user}\", realm=\"{REALM}\", nonce=\"{nonce}\", \
                 uri=\"sip:bridge\", response=\"{response}\""
            )
        }
    }
}

fn non_register(method: &str) -> String {
    format!(
        "{method} sip:bridge SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.168.1.50:5060;branch=z9hG4bKx\r\n\
         From: <sip:{USER}@bridge>;tag=phone-tag\r\n\
         To: <sip:someone@bridge>\r\n\
         Call-ID: other-call\r\n\
         CSeq: 1 {method}\r\n\
         Contact: {CONTACT}\r\n\
         Content-Length: 0\r\n\r\n"
    )
}

// -------------------------------------------------------------- REGISTER ---

/// §1.1 — every REGISTER is challenged, and nothing is registered by asking.
#[test]
fn an_unauthenticated_register_is_challenged() {
    let h = Harness::new();
    let response = h.round_trip(&register(1, "call-1", None, None));

    assert_status(&response, 401);
    let auth = header_of(&response, "WWW-Authenticate").expect("challenge");
    assert!(auth.starts_with("Digest "), "got: {auth}");
    assert!(auth.contains(&format!("realm=\"{REALM}\"")), "got: {auth}");
    assert!(auth.contains("nonce=\""), "got: {auth}");
    assert!(auth.contains("qop=\"auth\""), "got: {auth}");
    assert!(
        !auth.contains("stale"),
        "a first challenge is not stale: {auth}"
    );
    assert!(
        h.registrar
            .bindings()
            .get_live(USER, std::time::Instant::now())
            .is_none(),
        "an unanswered challenge must not register anything"
    );
}

/// §1.2 — the modern `qop=auth` form.
#[test]
fn a_correct_qop_auth_registration_is_accepted_and_stored() {
    let h = Harness::new();
    let response = h.register_ok(1, "call-1");

    let contact = header_of(&response, "Contact").expect("Contact echoed");
    assert!(
        contact.contains("sip:1001@192.168.1.50:5060"),
        "got: {contact}"
    );
    assert!(
        contact.contains("expires="),
        "must state the granted lifetime: {contact}"
    );

    let binding = h
        .registrar
        .bindings()
        .get_live(USER, std::time::Instant::now())
        .expect("registered");
    assert_eq!(binding.contact_uri, "sip:1001@192.168.1.50:5060");
    assert_eq!(binding.user_agent.as_deref(), Some("TestPhone/1.0"));
}

/// §1.2 — the legacy RFC 2069 form, still sent by handsets in the field.
#[test]
fn a_correct_registration_without_qop_is_accepted() {
    let h = Harness::new();
    let nonce = nonce_from(&h.round_trip(&register(1, "call-1", None, None)));
    let auth = authorization(USER, PASSWORD, &nonce, None);

    assert_status(
        &h.round_trip(&register(2, "call-1", Some(&auth), None)),
        200,
    );
    assert!(h
        .registrar
        .bindings()
        .get_live(USER, std::time::Instant::now())
        .is_some());
}

/// §1.3 and §1.4 — the two must be **byte-identical** apart from the nonce,
/// so the registrar cannot be used to discover which accounts exist (FR-009).
#[test]
fn a_wrong_password_and_an_unknown_user_are_refused_identically() {
    let h = Harness::new();

    let nonce = nonce_from(&h.round_trip(&register(1, "call-a", None, None)));
    let wrong = h.round_trip(&register(
        2,
        "call-a",
        Some(&authorization(
            USER,
            "wrong-password",
            &nonce,
            Some("00000001"),
        )),
        None,
    ));

    // Same Call-ID and CSeq as above, so the only thing that could differ
    // between the two responses is what the registrar itself chose to say.
    let nonce = nonce_from(&h.round_trip(&register(1, "call-a", None, None)));
    let unknown = h.round_trip(&register(
        2,
        "call-a",
        Some(&authorization(
            "9999",
            "any-password",
            &nonce,
            Some("00000001"),
        )),
        None,
    ));

    assert_status(&wrong, 401);
    assert_status(&unknown, 401);

    // Strip the nonce, which is random per challenge, and compare the rest.
    let scrub = |r: &str| {
        r.lines()
            .map(|l| {
                if l.to_ascii_lowercase().starts_with("www-authenticate:") {
                    let start = l.find("nonce=\"").unwrap() + "nonce=\"".len();
                    let end = l[start..].find('"').unwrap() + start;
                    format!("{}<nonce>{}", &l[..start], &l[end..])
                } else if l.to_ascii_lowercase().starts_with("to:") {
                    // Our To-tag is random too.
                    l.split(";tag=").next().unwrap_or(l).to_string()
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        scrub(&wrong),
        scrub(&unknown),
        "the two refusals must be indistinguishable on the wire"
    );
    assert!(h
        .registrar
        .bindings()
        .get_live(USER, std::time::Instant::now())
        .is_none());
}

/// §1.5 — a nonce we never minted is stale, so the phone retries silently
/// instead of prompting a human for a password.
#[test]
fn an_unknown_nonce_is_answered_as_stale() {
    let h = Harness::new();
    let auth = authorization(USER, PASSWORD, "nonce-we-never-minted", Some("00000001"));
    let response = h.round_trip(&register(1, "call-1", Some(&auth), None));

    assert_status(&response, 401);
    let header = header_of(&response, "WWW-Authenticate").expect("challenge");
    assert!(header.contains("stale=true"), "got: {header}");
}

// §1.5 — a nonce that ages out is stale for the same reason. Not asserted
// here: the shortest lifetime config permits is 10s, and sleeping that long
// would blow the per-test timeout. The expiry arithmetic is covered without
// sleeping by `sip::server::auth::tests::an_expired_nonce_is_stale_...`, and
// the wire shape of a stale challenge is covered by the test above.

/// §1.6 — replaying a captured header must not register anything.
#[test]
fn a_replayed_nonce_count_is_refused() {
    let h = Harness::new();
    let nonce = nonce_from(&h.round_trip(&register(1, "call-1", None, None)));
    let auth = authorization(USER, PASSWORD, &nonce, Some("00000001"));

    assert_status(
        &h.round_trip(&register(2, "call-1", Some(&auth), None)),
        200,
    );
    // Same nonce, same nc, a different dialog: a replay.
    assert_status(
        &h.round_trip(&register(2, "call-2", Some(&auth), None)),
        401,
    );
}

/// §1.7 — algorithms we do not implement are refused rather than ignored.
#[test]
fn an_unsupported_algorithm_is_refused() {
    let h = Harness::new();
    let nonce = nonce_from(&h.round_trip(&register(1, "call-1", None, None)));
    let auth = format!(
        "Digest username=\"{USER}\", realm=\"{REALM}\", nonce=\"{nonce}\", uri=\"sip:bridge\", \
         response=\"whatever\", algorithm=SHA-256"
    );
    assert_status(
        &h.round_trip(&register(2, "call-1", Some(&auth), None)),
        401,
    );
}

/// §1.8 — a phone asking to refresh faster than we allow is told the floor,
/// not silently granted something else.
#[test]
fn an_expiry_below_the_floor_is_refused_with_min_expires() {
    let h = Harness::new();
    let nonce = nonce_from(&h.round_trip(&register(1, "call-1", None, None)));
    let auth = authorization(USER, PASSWORD, &nonce, Some("00000001"));
    let response = h.round_trip(&register(2, "call-1", Some(&auth), Some("Expires: 30\r\n")));

    assert_status(&response, 423);
    assert_eq!(header_of(&response, "Min-Expires"), Some("60"));
    assert!(h
        .registrar
        .bindings()
        .get_live(USER, std::time::Instant::now())
        .is_none());
}

/// §1.9 — an over-long request is clamped, and the response reports what was
/// actually granted rather than echoing what was asked for.
#[test]
fn an_expiry_above_the_ceiling_is_clamped_and_reported() {
    let h = Harness::new();
    let nonce = nonce_from(&h.round_trip(&register(1, "call-1", None, None)));
    let auth = authorization(USER, PASSWORD, &nonce, Some("00000001"));
    let response = h.round_trip(&register(
        2,
        "call-1",
        Some(&auth),
        Some("Expires: 99999\r\n"),
    ));

    assert_status(&response, 200);
    let contact = header_of(&response, "Contact").expect("Contact");
    assert!(
        contact.contains("expires=3600"),
        "must report the grant: {contact}"
    );
    assert!(
        !contact.contains("99999"),
        "must not echo the request: {contact}"
    );
}

/// §1.10 — an explicit un-registration is honoured immediately.
#[test]
fn expires_zero_deregisters_and_returns_no_contact() {
    let h = Harness::new();
    h.register_ok(1, "call-1");
    assert!(h
        .registrar
        .bindings()
        .get_live(USER, std::time::Instant::now())
        .is_some());

    let nonce = nonce_from(&h.round_trip(&register(3, "call-2", None, None)));
    let auth = authorization(USER, PASSWORD, &nonce, Some("00000001"));
    let response = h.round_trip(&register(4, "call-2", Some(&auth), Some("Expires: 0\r\n")));

    assert_status(&response, 200);
    assert_eq!(header_of(&response, "Contact"), None, "got:\n{response}");
    assert!(
        h.registrar
            .bindings()
            .get_live(USER, std::time::Instant::now())
            .is_none(),
        "the binding must be gone"
    );
}

/// §1.11 — packet loss makes handsets retransmit; that must not be read as a
/// new registration or as a conflict.
#[test]
fn a_retransmitted_register_is_answered_without_changing_the_binding() {
    let h = Harness::new();
    let nonce = nonce_from(&h.round_trip(&register(1, "call-1", None, None)));
    let auth = authorization(USER, PASSWORD, &nonce, Some("00000001"));

    let first = h.round_trip(&register(2, "call-1", Some(&auth), None));
    assert_status(&first, 200);
    let before = h
        .registrar
        .bindings()
        .get_live(USER, std::time::Instant::now())
        .expect("registered");

    // Same Call-ID, same CSeq — the definition of a retransmission.
    let again = h.round_trip(&register(2, "call-1", Some(&auth), None));
    assert_status(&again, 200);

    let after = h
        .registrar
        .bindings()
        .get_live(USER, std::time::Instant::now())
        .expect("still registered");
    assert_eq!(after.contact_uri, before.contact_uri);
    assert_eq!(after.cseq, before.cseq, "CSeq must not advance");
    assert!(
        after.expires_at <= before.expires_at,
        "a retransmission must not extend the registration"
    );
}

/// §1.12 — enough of a request to answer, but not enough to act on.
#[test]
fn a_register_without_a_contact_is_a_bad_request() {
    let h = Harness::new();
    let nonce = nonce_from(&h.round_trip(&register(1, "call-1", None, None)));
    let auth = authorization(USER, PASSWORD, &nonce, Some("00000001"));
    let request = format!(
        "REGISTER sip:bridge SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.168.1.50:5060;branch=z9hG4bK2\r\n\
         From: <sip:{USER}@bridge>;tag=phone-tag\r\n\
         To: <sip:{USER}@bridge>\r\n\
         Call-ID: call-1\r\n\
         CSeq: 2 REGISTER\r\n\
         Authorization: {auth}\r\n\
         Content-Length: 0\r\n\r\n"
    );
    assert_status(&h.round_trip(&request), 400);
}

/// SC-003 — a handset that moves to a new address keeps receiving calls, with
/// no operator action.
#[test]
fn re_registering_from_a_new_address_moves_where_calls_go() {
    let h = Harness::new();
    h.register_ok(1, "call-1");

    let nonce = nonce_from(&h.round_trip(&register(3, "call-2", None, None)));
    let auth = authorization(USER, PASSWORD, &nonce, Some("00000001"));
    let moved = format!(
        "REGISTER sip:bridge SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.168.1.99:5062;branch=z9hG4bK9\r\n\
         From: <sip:{USER}@bridge>;tag=phone-tag\r\n\
         To: <sip:{USER}@bridge>\r\n\
         Call-ID: call-2\r\n\
         CSeq: 4 REGISTER\r\n\
         Contact: <sip:{USER}@192.168.1.99:5062>\r\n\
         Authorization: {auth}\r\n\
         Content-Length: 0\r\n\r\n"
    );
    assert_status(&h.round_trip(&moved), 200);

    let bindings = h.registrar.bindings();
    let binding = bindings
        .get_live(USER, std::time::Instant::now())
        .expect("still registered");
    assert_eq!(binding.contact_uri, "sip:1001@192.168.1.99:5062");
    assert_eq!(
        bindings.live_count(std::time::Instant::now()),
        1,
        "moving must replace, not accumulate"
    );
}

// ---------------------------------------------------------- other methods ---

/// §2 — handsets use OPTIONS as a keepalive. Unanswered, they mark the server
/// dead and drop their binding, so the mode would work and then quietly stop.
#[test]
fn options_is_answered_so_keepalives_do_not_drop_the_registration() {
    let h = Harness::new();
    let response = h.round_trip(&non_register("OPTIONS"));

    assert_status(&response, 200);
    let allow = header_of(&response, "Allow").expect("Allow");
    assert!(allow.contains("REGISTER"), "got: {allow}");
    assert!(allow.contains("INVITE"), "got: {allow}");
}

/// §2 — phone-originated dialling is out of scope, and an explicit refusal
/// beats a 32-second retransmit and a timeout on the handset's screen.
#[test]
fn a_call_from_a_phone_is_explicitly_refused() {
    let h = Harness::new();
    assert_status(&h.round_trip(&non_register("INVITE")), 403);
}

#[test]
fn a_subscribe_is_refused_as_a_bad_event() {
    let h = Harness::new();
    assert_status(&h.round_trip(&non_register("SUBSCRIBE")), 489);
}

#[test]
fn an_unsupported_method_is_refused_with_allow() {
    let h = Harness::new();
    let response = h.round_trip(&non_register("PUBLISH"));

    assert_status(&response, 405);
    assert!(header_of(&response, "Allow").is_some(), "got:\n{response}");
}

// ------------------------------------------------ response construction ----

/// §3 — a request that traversed more than one hop must have its full `Via`
/// stack returned, in order. Guards the reuse of the shared response builder.
#[test]
fn every_via_is_echoed_in_order() {
    let h = Harness::new();
    let request = format!(
        "REGISTER sip:bridge SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bKouter\r\n\
         Via: SIP/2.0/UDP 192.168.1.50:5060;branch=z9hG4bKinner\r\n\
         From: <sip:{USER}@bridge>;tag=phone-tag\r\n\
         To: <sip:{USER}@bridge>\r\n\
         Call-ID: call-1\r\n\
         CSeq: 1 REGISTER\r\n\
         Contact: {CONTACT}\r\n\
         Content-Length: 0\r\n\r\n"
    );
    let response = h.round_trip(&request);

    let vias: Vec<&str> = response
        .lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("via:"))
        .collect();
    assert_eq!(vias.len(), 2, "got:\n{response}");
    assert!(
        vias[0].contains("z9hG4bKouter"),
        "order must hold: {vias:?}"
    );
    assert!(
        vias[1].contains("z9hG4bKinner"),
        "order must hold: {vias:?}"
    );
}

/// The dialog identifiers a phone matches the response against must come back
/// untouched, or it will not recognise the answer as its own.
#[test]
fn call_id_and_cseq_are_echoed_verbatim() {
    let h = Harness::new();
    let response = h.round_trip(&register(7, "distinctive-call-id", None, None));

    assert_eq!(header_of(&response, "Call-ID"), Some("distinctive-call-id"));
    assert_eq!(header_of(&response, "CSeq"), Some("7 REGISTER"));
}

/// Garbage must not take the registrar down, and a phone that follows it with
/// a real request must still be served.
#[test]
fn an_unparseable_datagram_is_ignored_without_killing_the_registrar() {
    let h = Harness::new();
    h.phone.send(b"this is not SIP at all").expect("send");

    // No response is expected for something we cannot even name a dialog for.
    h.phone
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut buf = [0u8; 1024];
    assert!(h.phone.recv(&mut buf).is_err(), "must not answer garbage");

    h.phone
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    assert_status(&h.round_trip(&register(1, "call-1", None, None)), 401);
}
