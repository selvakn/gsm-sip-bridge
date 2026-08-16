//! Guards outbound dialling (FR-006a/FR-006b). Outbound calls cost money and
//! ring real people, and an agent can retry in a loop, so both checks run
//! before any signalling leaves the host and fail closed.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct SafetyPolicy {
    /// Exact numbers or trailing-`*` prefixes. Empty denies every
    /// destination — the guard fails closed, never open.
    #[serde(default)]
    pub allowed_destinations: Vec<String>,
    #[serde(default = "default_min_call_interval_secs")]
    pub min_call_interval_secs: u64,
    #[serde(default = "default_max_calls_per_hour")]
    pub max_calls_per_hour: u32,
}

fn default_min_call_interval_secs() -> u64 {
    10
}
fn default_max_calls_per_hour() -> u32 {
    20
}

impl Default for SafetyPolicy {
    fn default() -> Self {
        Self {
            allowed_destinations: Vec::new(),
            min_call_interval_secs: 10,
            max_calls_per_hour: 20,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyRefusal {
    NotAllowed,
    RateLimited { retry_after_s: u64 },
}

impl SafetyPolicy {
    fn destination_allowed(&self, destination: &str) -> bool {
        self.allowed_destinations
            .iter()
            .any(|pattern| match pattern.strip_suffix('*') {
                Some(prefix) => destination.starts_with(prefix),
                None => pattern == destination,
            })
    }

    /// Checks a proposed outbound call against the allow-list and rate
    /// limit. Does **not** mutate `history` — the caller records the attempt
    /// with [`CallAttemptHistory::record`] only once the call is actually
    /// placed, so a rejected attempt never consumes rate budget.
    pub fn check(
        &self,
        destination: &str,
        history: &CallAttemptHistory,
        now: Instant,
    ) -> Result<(), SafetyRefusal> {
        if !self.destination_allowed(destination) {
            return Err(SafetyRefusal::NotAllowed);
        }
        if let Some(retry_after_s) = history.blocked_until(
            now,
            Duration::from_secs(self.min_call_interval_secs),
            self.max_calls_per_hour,
        ) {
            return Err(SafetyRefusal::RateLimited { retry_after_s });
        }
        Ok(())
    }
}

/// A bounded sliding window of outbound call attempt timestamps.
#[derive(Debug, Default)]
pub struct CallAttemptHistory {
    attempts: VecDeque<Instant>,
}

const WINDOW: Duration = Duration::from_secs(3600);

impl CallAttemptHistory {
    pub fn new() -> Self {
        Self::default()
    }

    fn prune(&mut self, now: Instant) {
        while let Some(&front) = self.attempts.front() {
            if now.duration_since(front) > WINDOW {
                self.attempts.pop_front();
            } else {
                break;
            }
        }
    }

    /// Returns `Some(retry_after_s)` if an attempt right now would violate
    /// either the minimum interval or the hourly cap; `None` if it is clear.
    fn blocked_until(
        &self,
        now: Instant,
        min_interval: Duration,
        max_per_hour: u32,
    ) -> Option<u64> {
        if let Some(&last) = self.attempts.back() {
            let since = now.duration_since(last);
            if since < min_interval {
                return Some((min_interval - since).as_secs().max(1));
            }
        }
        if self.attempts.len() as u32 >= max_per_hour {
            if let Some(&oldest) = self.attempts.iter().rev().nth(max_per_hour as usize - 1) {
                let elapsed = now.duration_since(oldest);
                if elapsed < WINDOW {
                    return Some((WINDOW - elapsed).as_secs().max(1));
                }
            }
        }
        None
    }

    /// Records an attempt that was actually placed (i.e. passed `check`).
    pub fn record(&mut self, now: Instant) {
        self.prune(now);
        self.attempts.push_back(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_allow_list_denies_everything() {
        let policy = SafetyPolicy::default();
        let history = CallAttemptHistory::new();
        assert_eq!(
            policy.check("+919000000000", &history, Instant::now()),
            Err(SafetyRefusal::NotAllowed)
        );
    }

    #[test]
    fn exact_and_prefix_matches_pass() {
        let policy = SafetyPolicy {
            allowed_destinations: vec!["+919000000000".into(), "+9190000*".into()],
            ..SafetyPolicy::default()
        };
        let history = CallAttemptHistory::new();
        let now = Instant::now();
        assert!(policy.check("+919000000000", &history, now).is_ok());
        assert!(policy.check("+919000099999", &history, now).is_ok());
        assert_eq!(
            policy.check("+919999999999", &history, now),
            Err(SafetyRefusal::NotAllowed)
        );
    }

    #[test]
    fn attempt_inside_min_interval_is_refused_with_retry_after() {
        let policy = SafetyPolicy {
            allowed_destinations: vec!["+919000000000".into()],
            min_call_interval_secs: 10,
            max_calls_per_hour: 20,
        };
        let mut history = CallAttemptHistory::new();
        let t0 = Instant::now();
        history.record(t0);

        let refusal = policy.check("+919000000000", &history, t0 + Duration::from_secs(3));
        assert_eq!(
            refusal,
            Err(SafetyRefusal::RateLimited { retry_after_s: 7 })
        );
    }

    #[test]
    fn twenty_first_attempt_within_an_hour_is_refused() {
        let policy = SafetyPolicy {
            allowed_destinations: vec!["+919000000000".into()],
            min_call_interval_secs: 0,
            max_calls_per_hour: 20,
        };
        let mut history = CallAttemptHistory::new();
        let t0 = Instant::now();
        for i in 0..20 {
            let t = t0 + Duration::from_secs(i * 30);
            assert!(policy.check("+919000000000", &history, t).is_ok());
            history.record(t);
        }
        let t20 = t0 + Duration::from_secs(20 * 30);
        assert!(matches!(
            policy.check("+919000000000", &history, t20),
            Err(SafetyRefusal::RateLimited { .. })
        ));
    }

    #[test]
    fn window_slides_so_an_old_attempt_frees_capacity() {
        let policy = SafetyPolicy {
            allowed_destinations: vec!["+919000000000".into()],
            min_call_interval_secs: 0,
            max_calls_per_hour: 1,
        };
        let mut history = CallAttemptHistory::new();
        let t0 = Instant::now();
        history.record(t0);
        assert!(policy
            .check("+919000000000", &history, t0 + Duration::from_secs(3601))
            .is_ok());
    }
}
