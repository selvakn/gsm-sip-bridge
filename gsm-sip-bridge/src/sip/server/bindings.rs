//! Where the registrar remembers which IP phones are currently reachable.
//!
//! One entry per account, created and refreshed by REGISTER, expiring on its
//! own. Nothing is persisted: a phone re-establishes its binding on its own
//! refresh timer after a restart, which is what SIP registration already
//! guarantees (spec 024).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Instant;

/// One registered phone.
#[derive(Clone, Debug)]
pub struct Binding {
    /// The account name the phone authenticated as.
    pub aor: String,
    /// Where to send INVITEs, taken verbatim from the REGISTER's `Contact`.
    ///
    /// Stored and dialled verbatim on purpose. Rewriting it to the source
    /// address would break handsets that listen on a port other than the one
    /// they send from; a mismatch is worth a WARN, not a silent correction.
    pub contact_uri: String,
    /// Where the REGISTER actually came from — diagnostics only.
    pub source: SocketAddr,
    pub call_id: String,
    pub cseq: u32,
    pub expires_at: Instant,
    pub user_agent: Option<String>,
}

impl Binding {
    fn is_live(&self, now: Instant) -> bool {
        self.expires_at > now
    }
}

/// The registrar's binding table.
///
/// **One binding per account, not RFC 3261 §10.3's contact set.** A contact set
/// exists so a registrar can fork an INVITE to every device registered under an
/// account. This bridge deliberately does not fork — it places exactly one call
/// toward exactly one account, preserving the single-active-call model the rest
/// of the bridge is built around. Storing extra contacts that could never be
/// dialled would be state with no consumer (constitution: simplicity; spec 024
/// research.md R-005).
///
/// The visible consequence, documented for operators: registering a second
/// device on the same account replaces the first, so calls go to whichever
/// registered most recently.
#[derive(Debug, Default)]
pub struct BindingStore {
    inner: Mutex<HashMap<String, Binding>>,
}

impl BindingStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A poisoned lock here means some other thread panicked mid-update. The
    /// map is a plain `HashMap` with no cross-entry invariant, so the worst
    /// case is one half-written entry — not a reason to take the whole
    /// registrar down. Same reasoning as `pjsua_safe`'s bridge-pair map.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Binding>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stores `binding`, replacing any existing one for the same account.
    pub fn upsert(&self, binding: Binding) {
        self.lock().insert(binding.aor.clone(), binding);
    }

    /// Drops the account's binding, if any. Used for explicit de-registration.
    pub fn remove(&self, aor: &str) {
        self.lock().remove(aor);
    }

    /// The account's binding, but only if it has not expired.
    ///
    /// Expiry is evaluated here rather than by a background thread: a lapsed
    /// binding is indistinguishable from an absent one to every caller, so
    /// there is nothing for a sweeper to do that this does not already do.
    pub fn get_live(&self, aor: &str, now: Instant) -> Option<Binding> {
        self.lock().get(aor).filter(|b| b.is_live(now)).cloned()
    }

    /// The account's binding whether or not it has expired.
    ///
    /// Only the REGISTER path wants this: a retransmitted REGISTER must be
    /// recognised as one even if the binding it refreshes has just lapsed.
    pub fn peek(&self, aor: &str) -> Option<Binding> {
        self.lock().get(aor).cloned()
    }

    /// The binding registered under `call_id`, expired or not.
    ///
    /// Used to recognise a retransmitted REGISTER before it has been
    /// authenticated, when there is no account name to look under yet.
    pub fn find_by_call_id(&self, call_id: &str) -> Option<Binding> {
        self.lock().values().find(|b| b.call_id == call_id).cloned()
    }

    /// The live binding whose REGISTER came from `addr`, if any.
    ///
    /// Used to recognise an INVITE as coming from an already-authenticated
    /// phone (spec 025 FR-003): the phone proved its password at REGISTER
    /// time, and this checks the INVITE arrived from that same source
    /// address, without requiring a second digest exchange on every call.
    pub fn find_by_source(&self, addr: std::net::SocketAddr, now: Instant) -> Option<Binding> {
        self.lock()
            .values()
            .find(|b| b.source == addr && b.is_live(now))
            .cloned()
    }

    /// Drops expired entries and returns how many remain live.
    ///
    /// Only exists so the gauges and logs report the truth — correctness does
    /// not depend on it, because [`get_live`](Self::get_live) filters anyway.
    pub fn sweep(&self, now: Instant) -> usize {
        let mut map = self.lock();
        map.retain(|_, b| b.is_live(now));
        map.len()
    }

    pub fn live_count(&self, now: Instant) -> usize {
        self.lock().values().filter(|b| b.is_live(now)).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn binding(aor: &str, expires_at: Instant) -> Binding {
        Binding {
            aor: aor.to_string(),
            contact_uri: format!("sip:{aor}@192.168.1.50:5060"),
            source: "192.168.1.50:5060".parse().unwrap(),
            call_id: format!("callid-{aor}"),
            cseq: 1,
            expires_at,
            user_agent: None,
        }
    }

    /// Every expiry rule is driven by a caller-supplied `now`, so the whole
    /// table can be tested at arbitrary simulated times without sleeping —
    /// which matters under the suite's per-test timeout.
    #[test]
    fn a_binding_is_live_until_its_expiry_and_not_after() {
        let store = BindingStore::new();
        let t0 = Instant::now();
        store.upsert(binding("1001", t0 + Duration::from_secs(60)));

        assert!(store.get_live("1001", t0).is_some());
        assert!(store
            .get_live("1001", t0 + Duration::from_secs(59))
            .is_some());
        assert!(store
            .get_live("1001", t0 + Duration::from_secs(60))
            .is_none());
        assert!(store
            .get_live("1001", t0 + Duration::from_secs(61))
            .is_none());
    }

    #[test]
    fn an_unknown_account_has_no_binding() {
        let store = BindingStore::new();
        assert!(store.get_live("nobody", Instant::now()).is_none());
    }

    /// The one-binding-per-account rule: a second registration replaces the
    /// first rather than accumulating beside it.
    #[test]
    fn registering_again_replaces_rather_than_accumulates() {
        let store = BindingStore::new();
        let t0 = Instant::now();
        store.upsert(binding("1001", t0 + Duration::from_secs(60)));

        let mut moved = binding("1001", t0 + Duration::from_secs(600));
        moved.contact_uri = "sip:1001@192.168.1.99:5062".to_string();
        moved.source = "192.168.1.99:5062".parse().unwrap();
        store.upsert(moved);

        assert_eq!(store.live_count(t0), 1, "must not accumulate");
        let found = store.get_live("1001", t0).expect("still registered");
        assert_eq!(found.contact_uri, "sip:1001@192.168.1.99:5062");
        assert!(
            store
                .get_live("1001", t0 + Duration::from_secs(120))
                .is_some(),
            "the newer expiry must win"
        );
    }

    /// The REGISTER path looks a retransmission up by `Call-ID` because at
    /// that point the request has not been authenticated, so there is no
    /// account name to look under yet.
    #[test]
    fn a_binding_can_be_found_by_call_id_before_its_account_is_known() {
        let store = BindingStore::new();
        let t0 = Instant::now();
        store.upsert(binding("1001", t0 + Duration::from_secs(60)));
        store.upsert(binding("1002", t0 + Duration::from_secs(60)));

        assert_eq!(
            store.find_by_call_id("callid-1002").map(|b| b.aor),
            Some("1002".to_string())
        );
        assert!(store.find_by_call_id("callid-nobody").is_none());
    }

    #[test]
    fn remove_deregisters_immediately() {
        let store = BindingStore::new();
        let t0 = Instant::now();
        store.upsert(binding("1001", t0 + Duration::from_secs(3600)));
        store.remove("1001");
        assert!(store.get_live("1001", t0).is_none());
        assert_eq!(store.live_count(t0), 0);
    }

    #[test]
    fn removing_an_unregistered_account_is_a_no_op() {
        let store = BindingStore::new();
        store.remove("nobody");
        assert_eq!(store.live_count(Instant::now()), 0);
    }

    #[test]
    fn sweep_drops_the_expired_and_counts_what_is_left() {
        let store = BindingStore::new();
        let t0 = Instant::now();
        store.upsert(binding("1001", t0 + Duration::from_secs(30)));
        store.upsert(binding("1002", t0 + Duration::from_secs(3600)));
        store.upsert(binding("1003", t0 + Duration::from_secs(10)));

        assert_eq!(store.sweep(t0), 3, "nothing has expired yet");
        assert_eq!(store.sweep(t0 + Duration::from_secs(60)), 1);
        assert!(store.peek("1001").is_none(), "swept entries are gone");
        assert!(store.peek("1002").is_some());
    }

    /// `peek` is what lets the REGISTER path recognise a retransmission even
    /// when the binding it refreshes has just lapsed.
    #[test]
    fn peek_sees_an_expired_binding_that_get_live_hides() {
        let store = BindingStore::new();
        let t0 = Instant::now();
        store.upsert(binding("1001", t0 + Duration::from_secs(10)));

        let later = t0 + Duration::from_secs(60);
        assert!(store.get_live("1001", later).is_none());
        assert!(store.peek("1001").is_some());
    }

    #[test]
    fn live_count_ignores_expired_entries_without_removing_them() {
        let store = BindingStore::new();
        let t0 = Instant::now();
        store.upsert(binding("1001", t0 + Duration::from_secs(10)));

        let later = t0 + Duration::from_secs(60);
        assert_eq!(store.live_count(later), 0);
        assert!(store.peek("1001").is_some(), "counting must not mutate");
    }
}
