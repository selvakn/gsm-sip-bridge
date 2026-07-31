//! RFC 2617 digest authentication, registrar side (spec 024, FR-008 to FR-010).
//!
//! The math itself is [`crate::ims::digest`], written for the carrier-facing
//! IMS-AKA client. Its `ha1` takes the password as raw bytes because RFC 3310
//! feeds it the AKA `RES` octets; passing `password.as_bytes()` makes it plain
//! RFC 2617, byte for byte. A second MD5 implementation for this direction
//! would be pure duplication.
//!
//! Policy: **challenge every REGISTER** that arrives without an
//! `Authorization` header. It is what every IP phone expects, and it makes the
//! nonce lifecycle trivial — one nonce is issued, used once, and dropped.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::ims::digest;
use crate::ims::sip_client::{parse_digest_challenge, random_hex};

/// Cap on outstanding challenges, so an unauthenticated peer cannot grow the
/// table without bound. Far above any plausible number of handsets; the point
/// is only that the ceiling exists.
const MAX_OUTSTANDING_NONCES: usize = 256;

/// Why a REGISTER was refused. The wire response is identical for the first
/// two — only the metric label differs, so the registrar cannot be used to
/// discover which account names are valid (FR-009).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFailure {
    /// Credentials did not verify.
    BadPassword,
    /// No such account. Answered exactly like `BadPassword`.
    UnknownUser,
    /// Well-formed, but against a nonce we no longer hold. The phone should
    /// retry silently rather than prompt a human, so this gets `stale=true`.
    StaleNonce,
    /// A replayed nonce-count, or an algorithm we do not implement.
    Rejected,
}

impl AuthFailure {
    /// Whether the challenge we send back carries `stale=true`.
    pub fn is_stale(self) -> bool {
        matches!(self, AuthFailure::StaleNonce)
    }

    /// The `outcome` metric label for this refusal.
    pub fn metric_label(self) -> &'static str {
        match self {
            AuthFailure::BadPassword => "rejected_auth",
            AuthFailure::UnknownUser => "rejected_unknown_user",
            AuthFailure::StaleNonce => "rejected_stale",
            AuthFailure::Rejected => "rejected_auth",
        }
    }
}

#[derive(Debug)]
struct NonceEntry {
    issued_at: Instant,
    /// Highest nonce-count seen under `qop=auth`. Replay guard.
    last_nc: u32,
}

/// Outstanding challenges.
pub struct NonceStore {
    inner: Mutex<HashMap<String, NonceEntry>>,
    lifetime: Duration,
}

impl NonceStore {
    pub fn new(lifetime: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            lifetime,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, NonceEntry>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mints a nonce and records it as outstanding.
    pub fn issue(&self, now: Instant) -> String {
        let nonce = random_hex(16);
        let mut map = self.lock();
        map.retain(|_, e| now.duration_since(e.issued_at) < self.lifetime);
        if map.len() >= MAX_OUTSTANDING_NONCES {
            // Evict the oldest rather than refusing to issue: a full table
            // must not become a way to lock legitimate phones out.
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, e)| e.issued_at)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(
            nonce.clone(),
            NonceEntry {
                issued_at: now,
                last_nc: 0,
            },
        );
        nonce
    }

    /// Drops expired entries; returns how many remain outstanding.
    pub fn sweep(&self, now: Instant) -> usize {
        let mut map = self.lock();
        map.retain(|_, e| now.duration_since(e.issued_at) < self.lifetime);
        map.len()
    }

    /// Consumes `nonce` for a successful single-use (no-`qop`) exchange.
    fn consume(&self, nonce: &str) {
        self.lock().remove(nonce);
    }

    /// Whether `nonce` is outstanding and unexpired.
    fn is_live(&self, nonce: &str, now: Instant) -> bool {
        self.lock()
            .get(nonce)
            .is_some_and(|e| now.duration_since(e.issued_at) < self.lifetime)
    }

    /// Records `nc` against `nonce`, rejecting a value that does not advance.
    fn accept_nc(&self, nonce: &str, nc: u32) -> bool {
        let mut map = self.lock();
        match map.get_mut(nonce) {
            Some(entry) if nc > entry.last_nc => {
                entry.last_nc = nc;
                true
            }
            _ => false,
        }
    }
}

/// The `WWW-Authenticate` value for a challenge.
pub fn challenge_header(realm: &str, nonce: &str, stale: bool) -> String {
    let mut header =
        format!("Digest realm=\"{realm}\", nonce=\"{nonce}\", qop=\"auth\", algorithm=MD5");
    if stale {
        header.push_str(", stale=true");
    }
    header
}

/// Verifies an `Authorization` header against the configured accounts.
///
/// `lookup` returns the password for a username, or `None` if no such account
/// exists. `method` and `request_uri` come from the request being
/// authenticated.
///
/// On success, returns the authenticated username.
pub fn verify(
    authorization: &str,
    method: &str,
    request_uri: &str,
    realm: &str,
    nonces: &NonceStore,
    now: Instant,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<String, AuthFailure> {
    let params = parse_digest_challenge(authorization).map_err(|_| AuthFailure::Rejected)?;
    let get = |key: &str| -> Option<&str> {
        params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    };

    let (Some(username), Some(nonce), Some(response)) =
        (get("username"), get("nonce"), get("response"))
    else {
        return Err(AuthFailure::Rejected);
    };

    // MD5-sess and SHA-256 are not implemented. No handset in the target set
    // needs them, and adding SHA-256 later is self-contained.
    match get("algorithm") {
        None => {}
        Some(a) if a.eq_ignore_ascii_case("MD5") => {}
        Some(_) => return Err(AuthFailure::Rejected),
    }
    // auth-int hashes the body; REGISTER has none, so it is never offered.
    if get("qop").is_some_and(|q| q.eq_ignore_ascii_case("auth-int")) {
        return Err(AuthFailure::Rejected);
    }

    // A nonce we no longer hold means the phone took too long, or already used
    // it. Either way it should retry silently — checked before credentials so
    // a slow-but-correct handset is never told its password is wrong.
    if !nonces.is_live(nonce, now) {
        return Err(AuthFailure::StaleNonce);
    }

    // RFC 2617: HA2 uses the client's own `uri` parameter, not our
    // Request-URI. Handsets disagree on which they send, and rejecting the
    // disagreement would lock out conforming phones.
    let digest_uri = get("uri").unwrap_or(request_uri);
    if digest_uri != request_uri {
        tracing::debug!(
            client_uri = digest_uri,
            request_uri,
            "sip_server: digest uri differs from the request-uri (allowed by RFC 2617)"
        );
    }

    // An unknown user must be indistinguishable on the wire from a wrong
    // password, so the credential check runs to completion either way and only
    // the returned label differs.
    let password = lookup(username);
    let ha1 = digest::ha1(
        username,
        realm,
        password.as_deref().unwrap_or("").as_bytes(),
    );
    let ha2 = digest::ha2(method, digest_uri);

    // Which form applies is decided by `qop` **alone**, and the matching replay
    // guard is selected in the same breath.
    //
    // Splitting those two decisions is a replay hole: a form chosen by
    // `(qop, nc, cnonce)` while the nonce was consumed only on `qop.is_none()`
    // let `qop=auth` *without* `nc`/`cnonce` fall through to the legacy digest,
    // skipping the nonce-count check (never reached) and the single-use
    // consumption (`qop` was present). A captured legacy `Authorization` with
    // `qop=auth` bolted on could then be replayed until the nonce expired,
    // each time overwriting or removing the victim's binding.
    let (expected, nonce_count) = match get("qop") {
        Some(qop) => {
            // Advertising `qop` means the client must supply both. Missing
            // either is malformed, not an invitation to fall back — RFC 2617
            // §3.2.2 requires `nc` and `cnonce` whenever `qop` is sent.
            let (Some(nc), Some(cnonce)) = (get("nc"), get("cnonce")) else {
                return Err(AuthFailure::Rejected);
            };
            let parsed_nc = u32::from_str_radix(nc, 16).map_err(|_| AuthFailure::Rejected)?;
            (
                digest::response_qop(&ha1, nonce, nc, cnonce, qop, &ha2),
                Some(parsed_nc),
            )
        }
        // The legacy RFC 2069 form. Still sent by handsets in the field, so
        // still accepted — the nonce is single-use, which is what stops replay
        // without a nonce-count to compare.
        None => (digest::response_simple(&ha1, nonce, &ha2), None),
    };

    // The credential is proven **before** any nonce state moves.
    //
    // Recording the nonce-count first let an unauthenticated attacker poison a
    // handset's nonce: sniff it off the wire (this is UDP on a LAN, and the
    // challenge is cleartext), send it back with `nc=ffffffff` and a junk
    // digest, and the count was already stored by the time the digest was
    // rejected. The handset's next genuine REGISTER then failed the
    // strictly-increasing check and was answered `401` *without* `stale`, which
    // handsets read as "wrong password" rather than "retry" — a registration
    // outage from an attacker who never knew the password.
    if !constant_time_eq(expected.as_bytes(), response.as_bytes()) {
        return Err(if password.is_none() {
            AuthFailure::UnknownUser
        } else {
            AuthFailure::BadPassword
        });
    }
    if password.is_none() {
        // Only reachable if an empty-password account matched, which config
        // validation forbids. Belt and braces.
        return Err(AuthFailure::UnknownUser);
    }

    // Genuine credential from here on, so the replay guards may now mutate.
    match nonce_count {
        // The strictly-increasing count is what lets one nonce serve a
        // handset's whole refresh cycle instead of one request.
        Some(nc) => {
            if !nonces.accept_nc(nonce, nc) {
                return Err(AuthFailure::Rejected);
            }
        }
        // Retire the nonce for the legacy form, whose only replay defence this
        // is — there is no count to compare.
        None => nonces.consume(nonce),
    }
    Ok(username.to_string())
}

/// Compares without an early exit on the first differing byte, so response
/// verification does not leak how much of a guess was correct.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const REALM: &str = "gsm-sip-bridge";
    const URI: &str = "sip:192.168.1.1:5060";

    fn accounts(u: &str) -> Option<String> {
        match u {
            "1001" => Some("s3cret".to_string()),
            _ => None,
        }
    }

    fn store() -> NonceStore {
        NonceStore::new(Duration::from_secs(120))
    }

    /// Builds the header a conforming phone would send. Computed with
    /// `ims::digest` directly, so a change on either side of the shared math
    /// breaks this test rather than silently agreeing with itself.
    fn authorization(user: &str, password: &str, nonce: &str, qop: Option<(&str, &str)>) -> String {
        let ha1 = digest::ha1(user, REALM, password.as_bytes());
        let ha2 = digest::ha2("REGISTER", URI);
        match qop {
            Some((nc, cnonce)) => {
                let response = digest::response_qop(&ha1, nonce, nc, cnonce, "auth", &ha2);
                format!(
                    "Digest username=\"{user}\", realm=\"{REALM}\", nonce=\"{nonce}\", \
                     uri=\"{URI}\", response=\"{response}\", qop=auth, nc={nc}, \
                     cnonce=\"{cnonce}\", algorithm=MD5"
                )
            }
            None => {
                let response = digest::response_simple(&ha1, nonce, &ha2);
                format!(
                    "Digest username=\"{user}\", realm=\"{REALM}\", nonce=\"{nonce}\", \
                     uri=\"{URI}\", response=\"{response}\""
                )
            }
        }
    }

    fn check(header: &str, nonces: &NonceStore, now: Instant) -> Result<String, AuthFailure> {
        verify(header, "REGISTER", URI, REALM, nonces, now, accounts)
    }

    #[test]
    fn the_challenge_carries_realm_nonce_and_qop() {
        let header = challenge_header(REALM, "deadbeef", false);
        assert!(header.starts_with("Digest "));
        assert!(header.contains("realm=\"gsm-sip-bridge\""));
        assert!(header.contains("nonce=\"deadbeef\""));
        assert!(header.contains("qop=\"auth\""));
        assert!(header.contains("algorithm=MD5"));
        assert!(!header.contains("stale"));

        assert!(challenge_header(REALM, "deadbeef", true).contains("stale=true"));
    }

    #[test]
    fn a_correct_qop_auth_response_authenticates() {
        let nonces = store();
        let now = Instant::now();
        let nonce = nonces.issue(now);
        let header = authorization("1001", "s3cret", &nonce, Some(("00000001", "abc")));
        assert_eq!(check(&header, &nonces, now).unwrap(), "1001");
    }

    /// The legacy RFC 2069 form, still sent by handsets in the field.
    #[test]
    fn a_correct_response_without_qop_authenticates() {
        let nonces = store();
        let now = Instant::now();
        let nonce = nonces.issue(now);
        let header = authorization("1001", "s3cret", &nonce, None);
        assert_eq!(check(&header, &nonces, now).unwrap(), "1001");
    }

    #[test]
    fn a_wrong_password_is_refused() {
        let nonces = store();
        let now = Instant::now();
        let nonce = nonces.issue(now);
        let header = authorization("1001", "wrong", &nonce, Some(("00000001", "abc")));
        assert_eq!(check(&header, &nonces, now), Err(AuthFailure::BadPassword));
    }

    /// The two must be distinguishable to us and indistinguishable to the
    /// caller — the wire response is built from `is_stale()`, which agrees.
    #[test]
    fn an_unknown_user_is_refused_the_same_way_a_wrong_password_is() {
        let nonces = store();
        let now = Instant::now();
        let n1 = nonces.issue(now);
        let n2 = nonces.issue(now);

        let unknown = check(&authorization("9999", "any", &n1, None), &nonces, now);
        let wrong = check(&authorization("1001", "wrong", &n2, None), &nonces, now);

        assert_eq!(unknown, Err(AuthFailure::UnknownUser));
        assert_eq!(wrong, Err(AuthFailure::BadPassword));
        assert_eq!(
            unknown.unwrap_err().is_stale(),
            wrong.unwrap_err().is_stale(),
            "both must produce the same challenge on the wire"
        );
    }

    #[test]
    fn an_expired_nonce_is_stale_so_the_phone_retries_silently() {
        let nonces = NonceStore::new(Duration::from_secs(120));
        let now = Instant::now();
        let nonce = nonces.issue(now);
        let header = authorization("1001", "s3cret", &nonce, Some(("00000001", "abc")));

        let later = now + Duration::from_secs(121);
        assert_eq!(check(&header, &nonces, later), Err(AuthFailure::StaleNonce));
        assert!(AuthFailure::StaleNonce.is_stale());
    }

    #[test]
    fn a_nonce_we_never_issued_is_stale() {
        let nonces = store();
        let now = Instant::now();
        let header = authorization("1001", "s3cret", "nonce-we-never-minted", None);
        assert_eq!(check(&header, &nonces, now), Err(AuthFailure::StaleNonce));
    }

    #[test]
    fn a_replayed_nonce_count_is_rejected() {
        let nonces = store();
        let now = Instant::now();
        let nonce = nonces.issue(now);
        let header = authorization("1001", "s3cret", &nonce, Some(("00000001", "abc")));

        assert!(check(&header, &nonces, now).is_ok());
        assert_eq!(
            check(&header, &nonces, now),
            Err(AuthFailure::Rejected),
            "the same nc must not be accepted twice"
        );
    }

    #[test]
    fn a_nonce_count_must_strictly_advance() {
        let nonces = store();
        let now = Instant::now();
        let nonce = nonces.issue(now);

        let second = authorization("1001", "s3cret", &nonce, Some(("00000002", "abc")));
        assert!(check(&second, &nonces, now).is_ok());

        let first = authorization("1001", "s3cret", &nonce, Some(("00000001", "abc")));
        assert_eq!(check(&first, &nonces, now), Err(AuthFailure::Rejected));
    }

    /// Without `qop` there is no nonce-count, so retiring the nonce on success
    /// is the only thing standing between a captured header and a replay.
    #[test]
    fn a_nonce_used_without_qop_cannot_be_used_again() {
        let nonces = store();
        let now = Instant::now();
        let nonce = nonces.issue(now);
        let header = authorization("1001", "s3cret", &nonce, None);

        assert!(check(&header, &nonces, now).is_ok());
        assert_eq!(check(&header, &nonces, now), Err(AuthFailure::StaleNonce));
    }

    /// Regression, PR #21 review: a captured legacy `Authorization` with
    /// `qop=auth` bolted on but no `nc`/`cnonce` used to fall through to the
    /// legacy digest — skipping the nonce-count check (never reached) and the
    /// single-use consumption (`qop` was present). It could then be replayed
    /// until the nonce expired, overwriting or removing the victim's binding
    /// each time.
    #[test]
    fn qop_without_nc_or_cnonce_is_refused_rather_than_falling_back_to_legacy() {
        let nonces = store();
        let now = Instant::now();
        let nonce = nonces.issue(now);

        // A legitimate legacy header, as a handset in the field would send it.
        let legacy = authorization("1001", "s3cret", &nonce, None);
        // The attacker's edit: claim qop, supply neither companion field.
        let forged = format!("{legacy}, qop=auth");

        assert_eq!(
            check(&forged, &nonces, now),
            Err(AuthFailure::Rejected),
            "claiming qop without nc/cnonce must be refused, not treated as legacy"
        );
        // And the nonce it targeted must still be usable by the real handset,
        // rather than burned by the forgery.
        assert!(check(&legacy, &nonces, now).is_ok());
    }

    /// The other half of the same hole: the forged form must not be replayable
    /// even once, let alone repeatedly.
    #[test]
    fn a_forged_qop_header_cannot_be_replayed() {
        let nonces = store();
        let now = Instant::now();
        let nonce = nonces.issue(now);
        let forged = format!(
            "{}, qop=auth",
            authorization("1001", "s3cret", &nonce, None)
        );

        for attempt in 0..3 {
            assert_eq!(
                check(&forged, &nonces, now),
                Err(AuthFailure::Rejected),
                "attempt {attempt} must be refused"
            );
        }
    }

    /// Regression, PR #21 review round 2: an attacker who sniffs a handset's
    /// nonce off the wire — plausible, since this is UDP on a LAN and the
    /// challenge is cleartext — used to be able to knock that handset off by
    /// replaying the nonce with a huge `nc` and a junk digest. The count was
    /// recorded before the digest was checked, so the handset's next genuine
    /// REGISTER then failed the strictly-increasing test.
    #[test]
    fn an_invalid_digest_cannot_poison_the_nonce_it_targets() {
        let nonces = store();
        let now = Instant::now();
        let nonce = nonces.issue(now);

        // The attacker knows only the nonce, not the password.
        let forged = format!(
            "Digest username=\"1001\", realm=\"{REALM}\", nonce=\"{nonce}\", uri=\"{URI}\", \
             response=\"00000000000000000000000000000000\", qop=auth, nc=ffffffff, \
             cnonce=\"attacker\", algorithm=MD5"
        );
        assert_eq!(
            check(&forged, &nonces, now),
            Err(AuthFailure::BadPassword),
            "a junk digest is a credential failure, not a replay"
        );

        // The genuine handset must still be able to use its own nonce.
        let genuine = authorization("1001", "s3cret", &nonce, Some(("00000001", "abc")));
        assert_eq!(
            check(&genuine, &nonces, now).unwrap(),
            "1001",
            "the attacker must not have consumed the handset's nc headroom"
        );
    }

    /// The same, for a wrong *password* rather than a junk digest — the more
    /// likely accident, and it must not cost the real handset its nonce either.
    #[test]
    fn a_wrong_password_does_not_consume_nonce_headroom() {
        let nonces = store();
        let now = Instant::now();
        let nonce = nonces.issue(now);

        let wrong = authorization("1001", "wrong", &nonce, Some(("0000000f", "abc")));
        assert_eq!(check(&wrong, &nonces, now), Err(AuthFailure::BadPassword));

        let genuine = authorization("1001", "s3cret", &nonce, Some(("00000001", "abc")));
        assert_eq!(check(&genuine, &nonces, now).unwrap(), "1001");
    }

    /// And an unknown account must not be able to poison a nonce either.
    #[test]
    fn an_unknown_user_does_not_consume_nonce_headroom() {
        let nonces = store();
        let now = Instant::now();
        let nonce = nonces.issue(now);

        let unknown = authorization("9999", "any", &nonce, Some(("ffffff00", "abc")));
        assert_eq!(check(&unknown, &nonces, now), Err(AuthFailure::UnknownUser));

        let genuine = authorization("1001", "s3cret", &nonce, Some(("00000001", "abc")));
        assert_eq!(check(&genuine, &nonces, now).unwrap(), "1001");
    }

    /// A nonce must survive a handset's whole refresh cycle under `qop=auth`,
    /// which is what makes the nc guard the right defence there rather than
    /// single-use consumption.
    #[test]
    fn one_nonce_serves_a_whole_refresh_cycle_under_qop() {
        let nonces = store();
        let now = Instant::now();
        let nonce = nonces.issue(now);

        for nc in ["00000001", "00000002", "00000003"] {
            let auth = authorization("1001", "s3cret", &nonce, Some((nc, "abc")));
            assert_eq!(check(&auth, &nonces, now).unwrap(), "1001", "nc={nc}");
        }
    }

    #[test]
    fn unsupported_algorithms_are_rejected() {
        let nonces = store();
        let now = Instant::now();
        for algorithm in ["MD5-sess", "SHA-256"] {
            let nonce = nonces.issue(now);
            let header = format!(
                "Digest username=\"1001\", realm=\"{REALM}\", nonce=\"{nonce}\", \
                 uri=\"{URI}\", response=\"whatever\", algorithm={algorithm}"
            );
            assert_eq!(
                check(&header, &nonces, now),
                Err(AuthFailure::Rejected),
                "{algorithm} must be refused"
            );
        }
    }

    #[test]
    fn auth_int_is_rejected_since_register_has_no_body() {
        let nonces = store();
        let now = Instant::now();
        let nonce = nonces.issue(now);
        let header = format!(
            "Digest username=\"1001\", realm=\"{REALM}\", nonce=\"{nonce}\", uri=\"{URI}\", \
             response=\"whatever\", qop=auth-int, nc=00000001, cnonce=\"abc\""
        );
        assert_eq!(check(&header, &nonces, now), Err(AuthFailure::Rejected));
    }

    #[test]
    fn a_malformed_authorization_header_is_rejected() {
        let nonces = store();
        let now = Instant::now();
        for header in [
            "",
            "Basic dXNlcjpwYXNz",
            "Digest ",
            "Digest username=\"1001\"",
        ] {
            assert_eq!(
                check(header, &nonces, now),
                Err(AuthFailure::Rejected),
                "{header:?} must be refused"
            );
        }
    }

    /// A phone that sends `uri=sip:realm` where we saw a different Request-URI
    /// is conforming, and must authenticate.
    #[test]
    fn the_clients_own_digest_uri_is_used_not_ours() {
        let nonces = store();
        let now = Instant::now();
        let nonce = nonces.issue(now);
        let header = authorization("1001", "s3cret", &nonce, None);
        // The registrar saw a different Request-URI than the phone hashed.
        let out = verify(
            &header,
            "REGISTER",
            "sip:somewhere-else:5060",
            REALM,
            &nonces,
            now,
            accounts,
        );
        assert_eq!(out.unwrap(), "1001");
    }

    #[test]
    fn sweep_drops_expired_nonces() {
        let nonces = NonceStore::new(Duration::from_secs(60));
        let now = Instant::now();
        nonces.issue(now);
        nonces.issue(now);
        assert_eq!(nonces.sweep(now), 2);
        assert_eq!(nonces.sweep(now + Duration::from_secs(61)), 0);
    }

    /// An unauthenticated peer must not be able to grow the table without
    /// bound by asking for challenges it never answers.
    #[test]
    fn outstanding_nonces_are_capped() {
        let nonces = store();
        let now = Instant::now();
        for _ in 0..(MAX_OUTSTANDING_NONCES + 50) {
            nonces.issue(now);
        }
        assert!(nonces.sweep(now) <= MAX_OUTSTANDING_NONCES);
    }

    #[test]
    fn failure_metric_labels_separate_the_cases_the_wire_conflates() {
        assert_eq!(AuthFailure::BadPassword.metric_label(), "rejected_auth");
        assert_eq!(
            AuthFailure::UnknownUser.metric_label(),
            "rejected_unknown_user"
        );
        assert_eq!(AuthFailure::StaleNonce.metric_label(), "rejected_stale");
    }
}
