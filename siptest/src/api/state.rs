//! Shared daemon state: registration status, the capped call registry
//! (data-model.md RetentionPolicy), and the safety gate's attempt history.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use crate::api::events::EventBus;
use crate::call::{Call, CallId};
use crate::safety::CallAttemptHistory;
use crate::sip::registration::RegistrationStatus;

pub enum Lookup {
    Found(Box<Call>),
    Evicted,
    NotFound,
}

/// Insertion-ordered, capped at `max_retained`. Evicting the oldest call
/// deletes its recording files (if any) and drops its record; the id moves
/// into a small remembered set so a later lookup can report `Evicted`
/// distinctly from `NotFound` (FR-025a).
pub struct CallRegistry {
    max_retained: usize,
    order: VecDeque<CallId>,
    calls: std::collections::HashMap<CallId, Call>,
    evicted: HashSet<CallId>,
}

impl CallRegistry {
    pub fn new(max_retained: usize) -> Self {
        Self {
            max_retained: max_retained.max(1),
            order: VecDeque::new(),
            calls: std::collections::HashMap::new(),
            evicted: HashSet::new(),
        }
    }

    /// Inserts or updates a call's record. Eviction only happens for a call
    /// that is not already tracked and pushes the registry over its cap — an
    /// in-progress call being updated in place never triggers eviction.
    pub fn upsert(&mut self, call: Call) -> Option<(CallId, Vec<std::path::PathBuf>)> {
        let id = call.id.clone();
        let is_new = !self.calls.contains_key(&id);
        self.calls.insert(id.clone(), call);
        if is_new {
            self.order.push_back(id);
        }

        if self.order.len() > self.max_retained {
            if let Some(oldest) = self.order.pop_front() {
                if let Some(removed) = self.calls.remove(&oldest) {
                    self.evicted.insert(oldest.clone());
                    let mut paths = Vec::new();
                    if let Some(report) = &removed.report {
                        if let Some(p) = &report.recordings.received {
                            paths.push(std::path::PathBuf::from(p));
                        }
                        if let Some(p) = &report.recordings.sent {
                            paths.push(std::path::PathBuf::from(p));
                        }
                    }
                    return Some((oldest, paths));
                }
            }
        }
        None
    }

    pub fn lookup(&self, id: &CallId) -> Lookup {
        if let Some(call) = self.calls.get(id) {
            Lookup::Found(Box::new(call.clone()))
        } else if self.evicted.contains(id) {
            Lookup::Evicted
        } else {
            Lookup::NotFound
        }
    }

    pub fn recent(&self, limit: usize) -> Vec<Call> {
        self.order
            .iter()
            .rev()
            .take(limit)
            .filter_map(|id| self.calls.get(id).cloned())
            .collect()
    }

    pub fn active(&self) -> Option<Call> {
        self.order
            .iter()
            .rev()
            .find_map(|id| self.calls.get(id))
            .filter(|c| !c.is_terminal())
            .cloned()
    }
}

#[derive(Default)]
pub struct Counters {
    pub calls_placed: u64,
    pub calls_received: u64,
    pub registrations: u64,
    pub errors: u64,
}

pub struct SharedState {
    pub registration: Mutex<RegistrationStatus>,
    pub calls: Mutex<CallRegistry>,
    pub attempt_history: Mutex<CallAttemptHistory>,
    pub counters: Mutex<Counters>,
    pub local_sip_addr: std::net::SocketAddr,
    pub bridge_registrar: std::net::SocketAddr,
    pub last_outbound_observed: Mutex<Option<std::net::SocketAddr>>,
    pub events: EventBus,
    pub safety: crate::safety::SafetyPolicy,
    pub config: crate::config::Config,
    pub next_call_seq: Mutex<u64>,
    pub sip_socket: Arc<crate::sip::socket::SipSocket>,
    pub registration_creds: Mutex<crate::sip::registration::RegistrationCredentials>,
}

impl SharedState {
    pub fn next_call_id(&self) -> CallId {
        let mut n = self.next_call_seq.lock().unwrap_or_else(|e| e.into_inner());
        *n += 1;
        CallId(format!("c-{n}"))
    }
}

pub type AppState = Arc<SharedState>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::{CallState, CallerId, Direction};
    use std::time::Instant;

    fn dummy_call(id: &str) -> Call {
        Call {
            id: CallId(id.to_string()),
            direction: Direction::Outbound,
            state: CallState::Ended,
            peer: "+919000000000".to_string(),
            peer_uri: "sip:+919000000000@192.168.15.10:5072".to_string(),
            caller_id: CallerId::default(),
            started_at: Instant::now(),
            end_reason: None,
            report: None,
        }
    }

    #[test]
    fn eviction_drops_the_oldest_and_marks_it_evicted() {
        let mut reg = CallRegistry::new(2);
        assert!(reg.upsert(dummy_call("c-1")).is_none());
        assert!(reg.upsert(dummy_call("c-2")).is_none());
        let evicted = reg.upsert(dummy_call("c-3"));
        assert!(evicted.is_some());
        assert_eq!(evicted.unwrap().0, CallId("c-1".to_string()));

        assert!(matches!(
            reg.lookup(&CallId("c-1".to_string())),
            Lookup::Evicted
        ));
        assert!(matches!(
            reg.lookup(&CallId("nonexistent".to_string())),
            Lookup::NotFound
        ));
        assert!(matches!(
            reg.lookup(&CallId("c-3".to_string())),
            Lookup::Found(_)
        ));
    }

    #[test]
    fn updating_an_existing_call_does_not_evict() {
        let mut reg = CallRegistry::new(1);
        assert!(reg.upsert(dummy_call("c-1")).is_none());
        let mut updated = dummy_call("c-1");
        updated.state = CallState::Answered;
        assert!(reg.upsert(updated).is_none());
        assert!(matches!(
            reg.lookup(&CallId("c-1".to_string())),
            Lookup::Found(_)
        ));
    }
}
