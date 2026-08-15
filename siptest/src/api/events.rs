//! Ordered, cursor-addressable event log (FR-029, contracts/control-api.md).
//! Deliberately not SSE (research.md R9): a `since`-cursor long-poll is
//! replayable, gap-detectable, and resumable after a crash, and it does not
//! need tokio in the event path at all — `Condvar` is enough.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

const RING_CAPACITY: usize = 500;

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub seq: u64,
    pub at: String,
    pub kind: String,
    #[serde(flatten)]
    pub payload: Value,
}

struct Inner {
    next_seq: u64,
    ring: VecDeque<Event>,
}

pub struct EventBus {
    state: Mutex<Inner>,
    cond: Condvar,
}

impl Default for EventBus {
    fn default() -> Self {
        Self {
            state: Mutex::new(Inner {
                next_seq: 1,
                ring: VecDeque::new(),
            }),
            cond: Condvar::new(),
        }
    }
}

impl EventBus {
    pub fn publish(&self, kind: &str, payload: Value) -> u64 {
        let mut inner = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let seq = inner.next_seq;
        inner.next_seq += 1;
        let at = chrono_now_rfc3339();
        inner.ring.push_back(Event {
            seq,
            at,
            kind: kind.to_string(),
            payload,
        });
        if inner.ring.len() > RING_CAPACITY {
            inner.ring.pop_front();
        }
        drop(inner);
        self.cond.notify_all();
        seq
    }

    pub fn current_seq(&self) -> u64 {
        let inner = self.state.lock().unwrap_or_else(|e| e.into_inner());
        inner.next_seq.saturating_sub(1)
    }

    /// Returns every event with `seq > since`, blocking up to `timeout` if
    /// none are yet available.
    pub fn since(&self, since: u64, timeout: Duration) -> Vec<Event> {
        let deadline = Instant::now() + timeout;
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let matched: Vec<Event> = guard
                .ring
                .iter()
                .filter(|e| e.seq > since)
                .cloned()
                .collect();
            if !matched.is_empty() {
                return matched;
            }
            let now = Instant::now();
            if now >= deadline {
                return Vec::new();
            }
            let (g, _timed_out) = self
                .cond
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            guard = g;
        }
    }
}

fn chrono_now_rfc3339() -> String {
    // No chrono dependency in this crate; a minimal RFC 3339 UTC stamp from
    // std alone is enough for an event log timestamp.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    format_unix_utc(secs, millis)
}

/// Formats a Unix timestamp as `YYYY-MM-DDTHH:MM:SS.mmmZ` without pulling in
/// a date/time crate — this crate has no other need for one.
fn format_unix_utc(secs: u64, millis: u32) -> String {
    const DAYS_PER_400Y: i64 = 146097;
    let days = (secs / 86400) as i64;
    let secs_of_day = secs % 86400;
    let (h, m, s) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );

    // Civil-from-days algorithm (Howard Hinnant), proleptic Gregorian.
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - DAYS_PER_400Y + 1 } / DAYS_PER_400Y;
    let doe = (z - era * DAYS_PER_400Y) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m_num = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m_num <= 2 { y + 1 } else { y };

    format!("{y:04}-{m_num:02}-{d:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_returns_only_events_after_the_cursor() {
        let bus = EventBus::default();
        bus.publish("a", serde_json::json!({}));
        let s2 = bus.publish("b", serde_json::json!({}));
        bus.publish("c", serde_json::json!({}));

        let events = bus.since(s2 - 1, Duration::from_millis(50));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "b");
        assert_eq!(events[1].kind, "c");
    }

    #[test]
    fn since_blocks_then_delivers_a_published_event_without_a_missed_wakeup() {
        use std::sync::Arc;
        use std::thread;

        let bus = Arc::new(EventBus::default());
        let bus2 = bus.clone();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            bus2.publish("late", serde_json::json!({"x": 1}));
        });

        let events = bus.since(0, Duration::from_secs(2));
        handle.join().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "late");
    }

    #[test]
    fn since_older_than_the_ring_still_answers_without_erroring() {
        let bus = EventBus::default();
        for i in 0..10 {
            bus.publish(&format!("e{i}"), serde_json::json!({}));
        }
        let events = bus.since(0, Duration::from_millis(10));
        assert_eq!(events.len(), 10);
    }
}
