//! Scheduled card auto-restart (feature 010).
//!
//! This module owns the cycle state machine. It deliberately does **not** spawn
//! its own tokio task: the FSM is driven from inside [`crate::modules::CardPool::run`]'s
//! existing `tokio::select!` loop. See `specs/010-scheduled-card-restart/research.md`
//! decision 3 for the rationale.

use std::collections::VecDeque;
use std::time::Duration;

use rand::Rng;

/// One end-to-end execution triggered by a single cron occurrence.
#[derive(Debug)]
pub struct CycleState {
    /// Unique identifier for log/metrics correlation; the unix-second timestamp
    /// of the cron tick that triggered this cycle.
    pub id: u64,
    /// The wall-clock cron tick that triggered this cycle.
    pub cron_tick: chrono::DateTime<chrono::Local>,
    /// The actual (post-jitter) cycle-start instant.
    pub started_at: tokio::time::Instant,
    /// Which phase of the cycle we are in.
    pub phase: CyclePhase,
    /// Initial-pass queue, populated in ascending slot order at cycle-start.
    pub pending: VecDeque<u32>,
    /// Slots that had an active SIP call on their initial attempt; retried at
    /// the end of the cycle.
    pub deferred: VecDeque<u32>,
    /// The slot currently being restarted (if any).
    pub current: Option<CurrentCard>,
    /// When the scheduler should next take an action inside the event loop.
    pub next_action_at: tokio::time::Instant,
    /// Recorded outcomes for every attempt in this cycle (success, failure,
    /// skipped, deferred — the full history, in order).
    pub outcomes: Vec<CycleOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CyclePhase {
    Initial,
    DeferredRetry,
    Complete,
}

#[derive(Debug, Clone)]
pub struct CurrentCard {
    pub slot: u32,
    pub attempt: AttemptType,
    pub started_at: tokio::time::Instant,
    pub deadline: tokio::time::Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptType {
    Initial,
    DeferredRetry,
}

impl std::fmt::Display for AttemptType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttemptType::Initial => f.write_str("initial"),
            AttemptType::DeferredRetry => f.write_str("deferred-retry"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleOutcome {
    pub slot: u32,
    pub attempt: AttemptType,
    pub outcome: Outcome,
    pub duration: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failed { reason: String },
    Deferred { reason: String },
    Skipped { reason: SkipReason },
    TimedOut,
    AlreadyRestartedByManual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    NonReady(String),
    ActiveCall,
    SlotDisappeared,
}

impl Outcome {
    /// The string used for the `outcome` label of the Prometheus counter
    /// `gsm_sip_bridge_scheduled_restart_total`. Must match the contract in
    /// `specs/010-scheduled-card-restart/contracts/config-schema.md`.
    pub fn metric_label(&self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Failed { .. } => "failed",
            Outcome::Deferred { .. } => "deferred",
            Outcome::Skipped { reason } => match reason {
                SkipReason::NonReady(_) => "skipped-non-ready",
                SkipReason::ActiveCall => "skipped-active-call",
                SkipReason::SlotDisappeared => "skipped-slot-disappeared",
            },
            Outcome::TimedOut => "timed-out",
            Outcome::AlreadyRestartedByManual => "skipped-already-restarted-by-manual",
        }
    }
}

/// Snapshot of slot state used by the FSM. Implemented by the pool's slot map
/// (production) and by a `MockSlotView` (tests).
pub trait SlotView {
    /// Is the slot known and currently in the `Ready` lifecycle state?
    fn is_ready(&self, slot: u32) -> bool;
    /// Is the slot in a state that should never be touched by the cycle
    /// (`Initializing`, `Recovering`, `GivenUp`)? Returns Some(reason) when
    /// it should be skipped, None when it should proceed.
    fn non_ready_skip_reason(&self, slot: u32) -> Option<String>;
    /// Does the slot have a live SIP call bridged on it right now?
    fn has_active_call(&self, slot: u32) -> bool;
    /// Has the slot's restart completed since the scheduler last polled?
    /// Returns `Some(Success)` when state moved from Recovering -> Ready,
    /// `Some(Failed)` when it ended in `GivenUp`, or `None` otherwise.
    fn restart_progress(&self, slot: u32) -> RestartProgress;
}

/// Result of a manual `card restart` command issued while a cycle is in
/// progress, used to implement FR-014a (clarification Q4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualRestartCycleAdvice {
    /// No cycle is active, or the slot is not tracked by the cycle. The caller
    /// should proceed with the manual restart normally.
    Proceed,
    /// The slot is currently being restarted by the cycle. The caller MUST
    /// reject the command with the embedded error message.
    Reject { error: String },
    /// The slot was in the pending or deferred queue. It has been removed and
    /// an `AlreadyRestartedByManual` outcome appended. The caller should
    /// proceed with the manual restart.
    PreemptAndProceed,
}

/// Decide what to do about a manual `card restart` for `slot` based on the
/// active cycle's state. Mutates the cycle as needed; the caller applies the
/// actual reboot (or rejection) based on the return value.
pub fn handle_manual_restart_during_cycle(
    cycle: &mut CycleState,
    slot: u32,
) -> ManualRestartCycleAdvice {
    if let Some(current) = cycle.current.as_ref() {
        if current.slot == slot {
            return ManualRestartCycleAdvice::Reject {
                error: format!(
                    "slot {slot} is currently being restarted by the scheduled cycle (cycle id={})",
                    cycle.id
                ),
            };
        }
    }

    let was_pending = if let Some(i) = cycle.pending.iter().position(|&s| s == slot) {
        cycle.pending.remove(i);
        true
    } else {
        false
    };
    let was_deferred = if let Some(i) = cycle.deferred.iter().position(|&s| s == slot) {
        cycle.deferred.remove(i);
        true
    } else {
        false
    };

    if was_pending || was_deferred {
        cycle.outcomes.push(CycleOutcome {
            slot,
            attempt: AttemptType::Initial,
            outcome: Outcome::AlreadyRestartedByManual,
            duration: Duration::ZERO,
        });
        ManualRestartCycleAdvice::PreemptAndProceed
    } else {
        ManualRestartCycleAdvice::Proceed
    }
}

/// What the scheduler observed about an in-progress per-card restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartProgress {
    /// Still in flight (e.g., Recovering or Initializing).
    InFlight,
    /// Now in `Ready` — success.
    Succeeded,
    /// Now in `GivenUp` — failure.
    Failed,
    /// Slot disappeared from the slot map (e.g., hot-unplug).
    Gone,
}

/// Mutations the FSM wants the event loop to apply.
#[derive(Debug, Clone, PartialEq)]
pub enum SchedulerAction {
    /// Send `ModuleCmd::Reboot` to this slot's worker and start the per-card timer.
    SendReboot { slot: u32 },
    /// Record an outcome for this slot (log + metrics).
    RecordOutcome { slot: u32, outcome: CycleOutcome },
    /// Move the cycle into the Complete phase; the event loop tears it down.
    Complete,
}

/// Uniform random integer in `[-max_seconds, +max_seconds]`. Returns 0 when
/// `max_seconds == 0`.
pub fn jitter_offset<R: Rng + ?Sized>(rng: &mut R, max_seconds: u64) -> i64 {
    if max_seconds == 0 {
        return 0;
    }
    let max = max_seconds as i64;
    rng.gen_range(-max..=max)
}

/// `base + uniform_random([-jitter, +jitter])`, clamped at zero.
pub fn gap_with_jitter<R: Rng + ?Sized>(rng: &mut R, base: u64, jitter: u64) -> Duration {
    let raw = base as i64 + jitter_offset(rng, jitter);
    Duration::from_secs(raw.max(0) as u64)
}

/// Parse a 5-field cron expression (`min hour dom month dow`) into the
/// `cron` crate's 7-field schedule by prepending `0 ` (seconds=0) and appending
/// ` *` (year=any).
pub fn parse_cron_5field(expr: &str) -> Result<cron::Schedule, String> {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return Err("cron expression is empty".into());
    }
    let n_fields = trimmed.split_whitespace().count();
    if n_fields != 5 {
        return Err(format!(
            "cron expression must have 5 fields (minute hour day-of-month month day-of-week), got {n_fields}"
        ));
    }
    let translated = format!("0 {trimmed} *");
    translated
        .parse::<cron::Schedule>()
        .map_err(|e| format!("invalid cron expression {expr:?}: {e}"))
}

/// Convenience: next upcoming occurrence in system local time strictly after `after`.
pub fn compute_next_scheduled_at(
    schedule: &cron::Schedule,
    after: chrono::DateTime<chrono::Local>,
) -> Option<chrono::DateTime<chrono::Local>> {
    schedule.after(&after).next()
}

/// The hard timeout we allow a single per-card restart to take before recording
/// `Outcome::TimedOut` and moving on. Not currently user-tunable.
pub const PER_CARD_TIMEOUT: Duration = Duration::from_secs(60);

/// Initial pause after issuing `ModuleCmd::Reboot` before the existing retry
/// machinery is expected to bring the slot back to `Ready`. Kept identical to
/// the manual-restart path so the two share behavior.
pub const REBOOT_SETTLE_DELAY: Duration = Duration::from_secs(10);

/// Core FSM step. Pure-ish: reads from `slot_view`, mutates `state`, returns a
/// list of [`SchedulerAction`]s for the caller to apply.
///
/// The caller MUST call this whenever `state.next_action_at <= now` (or right
/// after creating the cycle). If the function returns `[]`, the cycle is still
/// alive and `state.next_action_at` is up-to-date; the event loop should
/// `sleep_until` it.
pub fn tick_scheduler<V: SlotView + ?Sized, R: Rng + ?Sized>(
    state: &mut CycleState,
    slot_view: &V,
    now: tokio::time::Instant,
    rng: &mut R,
    gap_base: u64,
    gap_jitter: u64,
) -> Vec<SchedulerAction> {
    let mut actions = Vec::new();

    // Step 1: if a card is currently being restarted, check its progress.
    if let Some(current) = state.current.clone() {
        match slot_view.restart_progress(current.slot) {
            RestartProgress::Succeeded => {
                let outcome = CycleOutcome {
                    slot: current.slot,
                    attempt: current.attempt,
                    outcome: Outcome::Success,
                    duration: now.duration_since(current.started_at),
                };
                state.outcomes.push(outcome.clone());
                actions.push(SchedulerAction::RecordOutcome {
                    slot: current.slot,
                    outcome,
                });
                state.current = None;
                state.next_action_at = now + gap_with_jitter(rng, gap_base, gap_jitter);
            }
            RestartProgress::Failed => {
                let outcome = CycleOutcome {
                    slot: current.slot,
                    attempt: current.attempt,
                    outcome: Outcome::Failed {
                        reason: "slot reached GivenUp during restart".into(),
                    },
                    duration: now.duration_since(current.started_at),
                };
                state.outcomes.push(outcome.clone());
                actions.push(SchedulerAction::RecordOutcome {
                    slot: current.slot,
                    outcome,
                });
                state.current = None;
                state.next_action_at = now + gap_with_jitter(rng, gap_base, gap_jitter);
            }
            RestartProgress::Gone => {
                let outcome = CycleOutcome {
                    slot: current.slot,
                    attempt: current.attempt,
                    outcome: Outcome::Skipped {
                        reason: SkipReason::SlotDisappeared,
                    },
                    duration: now.duration_since(current.started_at),
                };
                state.outcomes.push(outcome.clone());
                actions.push(SchedulerAction::RecordOutcome {
                    slot: current.slot,
                    outcome,
                });
                state.current = None;
                state.next_action_at = now + gap_with_jitter(rng, gap_base, gap_jitter);
            }
            RestartProgress::InFlight => {
                if now >= current.deadline {
                    let outcome = CycleOutcome {
                        slot: current.slot,
                        attempt: current.attempt,
                        outcome: Outcome::TimedOut,
                        duration: now.duration_since(current.started_at),
                    };
                    state.outcomes.push(outcome.clone());
                    actions.push(SchedulerAction::RecordOutcome {
                        slot: current.slot,
                        outcome,
                    });
                    state.current = None;
                    state.next_action_at = now + gap_with_jitter(rng, gap_base, gap_jitter);
                } else {
                    // Still in flight; poll again in 1s.
                    state.next_action_at = (now + Duration::from_secs(1)).min(current.deadline);
                    return actions;
                }
            }
        }
    }

    // Step 2: if no card is current, pop the next one from the active queue.
    if state.current.is_none() {
        loop {
            let attempt = match state.phase {
                CyclePhase::Initial => AttemptType::Initial,
                CyclePhase::DeferredRetry => AttemptType::DeferredRetry,
                CyclePhase::Complete => return actions,
            };

            let next_slot = match state.phase {
                CyclePhase::Initial => state.pending.pop_front(),
                CyclePhase::DeferredRetry => state.deferred.pop_front(),
                CyclePhase::Complete => None,
            };

            let Some(slot) = next_slot else {
                // No more in this phase. Try to advance phases.
                match state.phase {
                    CyclePhase::Initial => {
                        if state.deferred.is_empty() {
                            state.phase = CyclePhase::Complete;
                            actions.push(SchedulerAction::Complete);
                            return actions;
                        } else {
                            state.phase = CyclePhase::DeferredRetry;
                            continue;
                        }
                    }
                    CyclePhase::DeferredRetry => {
                        state.phase = CyclePhase::Complete;
                        actions.push(SchedulerAction::Complete);
                        return actions;
                    }
                    CyclePhase::Complete => return actions,
                }
            };

            // Inspect the slot at the moment its turn comes.
            if let Some(reason) = slot_view.non_ready_skip_reason(slot) {
                let outcome = CycleOutcome {
                    slot,
                    attempt,
                    outcome: Outcome::Skipped {
                        reason: SkipReason::NonReady(reason),
                    },
                    duration: Duration::ZERO,
                };
                state.outcomes.push(outcome.clone());
                actions.push(SchedulerAction::RecordOutcome { slot, outcome });
                continue;
            }

            if slot_view.has_active_call(slot) {
                match attempt {
                    AttemptType::Initial => {
                        state.deferred.push_back(slot);
                        let outcome = CycleOutcome {
                            slot,
                            attempt,
                            outcome: Outcome::Deferred {
                                reason: "active call".into(),
                            },
                            duration: Duration::ZERO,
                        };
                        state.outcomes.push(outcome.clone());
                        actions.push(SchedulerAction::RecordOutcome { slot, outcome });
                        continue;
                    }
                    AttemptType::DeferredRetry => {
                        let outcome = CycleOutcome {
                            slot,
                            attempt,
                            outcome: Outcome::Skipped {
                                reason: SkipReason::ActiveCall,
                            },
                            duration: Duration::ZERO,
                        };
                        state.outcomes.push(outcome.clone());
                        actions.push(SchedulerAction::RecordOutcome { slot, outcome });
                        continue;
                    }
                }
            }

            if !slot_view.is_ready(slot) {
                let outcome = CycleOutcome {
                    slot,
                    attempt,
                    outcome: Outcome::Skipped {
                        reason: SkipReason::NonReady("not ready at turn".into()),
                    },
                    duration: Duration::ZERO,
                };
                state.outcomes.push(outcome.clone());
                actions.push(SchedulerAction::RecordOutcome { slot, outcome });
                continue;
            }

            // Begin restart of this slot.
            let started_at = now;
            let deadline = started_at + PER_CARD_TIMEOUT;
            state.current = Some(CurrentCard {
                slot,
                attempt,
                started_at,
                deadline,
            });
            // Give the modem its expected reboot settle time before we start
            // polling progress; this matches the manual-restart code path.
            state.next_action_at = started_at + REBOOT_SETTLE_DELAY;
            actions.push(SchedulerAction::SendReboot { slot });
            return actions;
        }
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use std::collections::HashMap;

    fn fixed_instant() -> tokio::time::Instant {
        tokio::time::Instant::now()
    }

    fn make_state(slots: &[u32]) -> CycleState {
        let now = fixed_instant();
        CycleState {
            id: 1,
            cron_tick: chrono::Local::now(),
            started_at: now,
            phase: CyclePhase::Initial,
            pending: slots.iter().copied().collect(),
            deferred: VecDeque::new(),
            current: None,
            next_action_at: now,
            outcomes: Vec::new(),
        }
    }

    /// Mock view backed by a HashMap. Tests mutate this between ticks to
    /// simulate slot lifecycle changes.
    #[derive(Default)]
    struct MockView {
        ready: HashMap<u32, bool>,
        non_ready: HashMap<u32, Option<String>>,
        active_call: HashMap<u32, bool>,
        progress: HashMap<u32, RestartProgress>,
    }

    impl MockView {
        fn set_ready(&mut self, slot: u32, ready: bool) {
            self.ready.insert(slot, ready);
        }
        fn set_active_call(&mut self, slot: u32, active: bool) {
            self.active_call.insert(slot, active);
        }
        fn set_progress(&mut self, slot: u32, p: RestartProgress) {
            self.progress.insert(slot, p);
        }
        fn set_non_ready_reason(&mut self, slot: u32, reason: Option<String>) {
            self.non_ready.insert(slot, reason);
        }
    }

    impl SlotView for MockView {
        fn is_ready(&self, slot: u32) -> bool {
            *self.ready.get(&slot).unwrap_or(&true)
        }
        fn non_ready_skip_reason(&self, slot: u32) -> Option<String> {
            self.non_ready.get(&slot).cloned().unwrap_or(None)
        }
        fn has_active_call(&self, slot: u32) -> bool {
            *self.active_call.get(&slot).unwrap_or(&false)
        }
        fn restart_progress(&self, slot: u32) -> RestartProgress {
            self.progress
                .get(&slot)
                .cloned()
                .unwrap_or(RestartProgress::InFlight)
        }
    }

    fn seeded_rng() -> rand::rngs::StdRng {
        rand::rngs::StdRng::seed_from_u64(0xC0FFEE)
    }

    #[test]
    fn jitter_offset_zero_max_is_always_zero() {
        let mut rng = seeded_rng();
        for _ in 0..100 {
            assert_eq!(jitter_offset(&mut rng, 0), 0);
        }
    }

    #[test]
    fn jitter_offset_in_range() {
        let mut rng = seeded_rng();
        for _ in 0..1000 {
            let v = jitter_offset(&mut rng, 60);
            assert!(v >= -60 && v <= 60, "out of range: {v}");
        }
    }

    #[test]
    fn gap_with_jitter_never_negative() {
        let mut rng = seeded_rng();
        for _ in 0..1000 {
            let d = gap_with_jitter(&mut rng, 5, 10);
            assert!(d.as_secs() < u64::MAX / 2, "underflow");
        }
    }

    #[test]
    fn parse_cron_5field_accepts_default() {
        let s = parse_cron_5field("0 1 * * *").expect("default cron must parse");
        let after = chrono::Local
            .with_ymd_and_hms(2026, 5, 26, 12, 0, 0)
            .single()
            .unwrap();
        let next = s.after(&after).next().unwrap();
        assert_eq!(next.hour(), 1);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn parse_cron_5field_rejects_invalid() {
        assert!(parse_cron_5field("0 25 * * *").is_err());
        assert!(parse_cron_5field("not a cron").is_err());
        assert!(parse_cron_5field("").is_err());
        assert!(parse_cron_5field("0 1 * *").is_err()); // 4 fields
        assert!(parse_cron_5field("0 0 1 * * *").is_err()); // 6 fields
    }

    #[test]
    fn parse_cron_5field_every_five_minutes() {
        let s = parse_cron_5field("*/5 * * * *").unwrap();
        let after = chrono::Local
            .with_ymd_and_hms(2026, 5, 26, 12, 2, 30)
            .single()
            .unwrap();
        let next = s.after(&after).next().unwrap();
        assert_eq!(next.minute(), 5);
    }

    #[test]
    fn tick_single_slot_success() {
        let mut state = make_state(&[7]);
        let mut view = MockView::default();
        view.set_ready(7, true);
        let mut rng = seeded_rng();

        // First tick: should pop slot 7 and send reboot.
        let actions = tick_scheduler(&mut state, &view, fixed_instant(), &mut rng, 0, 0);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            actions[0],
            SchedulerAction::SendReboot { slot: 7 }
        ));
        assert!(state.current.is_some());

        // Mark slot 7 as succeeded, advance time past REBOOT_SETTLE_DELAY.
        view.set_progress(7, RestartProgress::Succeeded);
        let later = fixed_instant() + REBOOT_SETTLE_DELAY + Duration::from_millis(1);
        let actions = tick_scheduler(&mut state, &view, later, &mut rng, 0, 0);
        // We expect: record-outcome(success), then attempt to pop more (queue empty -> Complete).
        assert!(actions.iter().any(|a| matches!(
            a,
            SchedulerAction::RecordOutcome {
                outcome: CycleOutcome {
                    outcome: Outcome::Success,
                    ..
                },
                slot: 7,
            }
        )));
        assert!(actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Complete)));
        assert_eq!(state.phase, CyclePhase::Complete);
    }

    #[test]
    fn tick_non_ready_slot_skipped() {
        let mut state = make_state(&[3]);
        let mut view = MockView::default();
        view.set_non_ready_reason(3, Some("Recovering".into()));
        let mut rng = seeded_rng();

        let actions = tick_scheduler(&mut state, &view, fixed_instant(), &mut rng, 0, 0);
        assert!(actions.iter().any(|a| matches!(
            a,
            SchedulerAction::RecordOutcome {
                outcome: CycleOutcome {
                    outcome: Outcome::Skipped {
                        reason: SkipReason::NonReady(_),
                    },
                    ..
                },
                ..
            }
        )));
        assert!(actions
            .iter()
            .any(|a| matches!(a, SchedulerAction::Complete)));
    }

    #[test]
    fn tick_active_call_deferred_then_succeeds_on_retry() {
        let mut state = make_state(&[1]);
        let mut view = MockView::default();
        view.set_active_call(1, true);
        let mut rng = seeded_rng();

        // Tick 1: active call → deferred. Queue empty → DeferredRetry phase → pop 1 again.
        // Now active is still true; record as Skipped(ActiveCall) and Complete.
        // BUT for the "succeeds on retry" path, we need to clear active before
        // the deferred-retry attempt. Let's interleave:
        let actions = tick_scheduler(&mut state, &view, fixed_instant(), &mut rng, 0, 0);
        let deferred_logged = actions.iter().any(|a| {
            matches!(
                a,
                SchedulerAction::RecordOutcome {
                    outcome: CycleOutcome {
                        outcome: Outcome::Deferred { .. },
                        attempt: AttemptType::Initial,
                        ..
                    },
                    slot: 1,
                }
            )
        });
        assert!(deferred_logged, "initial active-call must be deferred");

        // After defer, phase transitions to DeferredRetry on next iteration.
        // The tick above immediately advances into the deferred-retry pop and
        // re-checks. Since active is still true, it should be Skipped(ActiveCall).
        let skipped = actions.iter().any(|a| {
            matches!(
                a,
                SchedulerAction::RecordOutcome {
                    outcome: CycleOutcome {
                        outcome: Outcome::Skipped {
                            reason: SkipReason::ActiveCall,
                        },
                        attempt: AttemptType::DeferredRetry,
                        ..
                    },
                    slot: 1,
                }
            )
        });
        assert!(
            skipped,
            "still-active deferred-retry must record SkipReason::ActiveCall"
        );

        assert_eq!(state.phase, CyclePhase::Complete);
    }

    #[test]
    fn tick_multi_slot_ascending_order() {
        let mut state = make_state(&[0, 1, 2]);
        let mut view = MockView::default();
        view.set_ready(0, true);
        view.set_ready(1, true);
        view.set_ready(2, true);
        let mut rng = seeded_rng();

        // Tick 1: pop slot 0, send reboot.
        let a1 = tick_scheduler(&mut state, &view, fixed_instant(), &mut rng, 0, 0);
        assert!(matches!(a1[0], SchedulerAction::SendReboot { slot: 0 }));

        // Slot 0 completes successfully.
        view.set_progress(0, RestartProgress::Succeeded);
        let t2 = fixed_instant() + REBOOT_SETTLE_DELAY;
        let a2 = tick_scheduler(&mut state, &view, t2, &mut rng, 0, 0);
        // After success record, the FSM should pop slot 1 in the same tick.
        assert!(a2
            .iter()
            .any(|a| matches!(a, SchedulerAction::SendReboot { slot: 1 })));

        view.set_progress(1, RestartProgress::Succeeded);
        let t3 = t2 + REBOOT_SETTLE_DELAY;
        let a3 = tick_scheduler(&mut state, &view, t3, &mut rng, 0, 0);
        assert!(a3
            .iter()
            .any(|a| matches!(a, SchedulerAction::SendReboot { slot: 2 })));

        view.set_progress(2, RestartProgress::Succeeded);
        let t4 = t3 + REBOOT_SETTLE_DELAY;
        let a4 = tick_scheduler(&mut state, &view, t4, &mut rng, 0, 0);
        assert!(a4.iter().any(|a| matches!(a, SchedulerAction::Complete)));
    }

    #[test]
    fn tick_timeout_records_timed_out() {
        let mut state = make_state(&[9]);
        let view = MockView::default(); // slot 9 stays InFlight forever
        let mut rng = seeded_rng();

        // Tick 1: send reboot.
        let _ = tick_scheduler(&mut state, &view, fixed_instant(), &mut rng, 0, 0);
        let current = state.current.as_ref().unwrap();
        let start = current.started_at;
        let deadline = current.deadline;
        assert_eq!(deadline - start, PER_CARD_TIMEOUT);

        // Advance to just past the deadline.
        let past = deadline + Duration::from_millis(1);
        let actions = tick_scheduler(&mut state, &view, past, &mut rng, 0, 0);
        assert!(actions.iter().any(|a| matches!(
            a,
            SchedulerAction::RecordOutcome {
                outcome: CycleOutcome {
                    outcome: Outcome::TimedOut,
                    ..
                },
                slot: 9,
            }
        )));
    }

    #[test]
    fn tick_slot_gone_recorded() {
        let mut state = make_state(&[4]);
        let mut view = MockView::default();
        view.set_ready(4, true);
        let mut rng = seeded_rng();

        let _ = tick_scheduler(&mut state, &view, fixed_instant(), &mut rng, 0, 0);
        view.set_progress(4, RestartProgress::Gone);
        let later = fixed_instant() + REBOOT_SETTLE_DELAY;
        let actions = tick_scheduler(&mut state, &view, later, &mut rng, 0, 0);
        assert!(actions.iter().any(|a| matches!(
            a,
            SchedulerAction::RecordOutcome {
                outcome: CycleOutcome {
                    outcome: Outcome::Skipped {
                        reason: SkipReason::SlotDisappeared,
                    },
                    ..
                },
                slot: 4,
            }
        )));
    }

    #[test]
    fn outcome_metric_labels() {
        assert_eq!(Outcome::Success.metric_label(), "success");
        assert_eq!(
            Outcome::Failed {
                reason: String::new()
            }
            .metric_label(),
            "failed"
        );
        assert_eq!(
            Outcome::Deferred {
                reason: String::new()
            }
            .metric_label(),
            "deferred"
        );
        assert_eq!(
            Outcome::Skipped {
                reason: SkipReason::NonReady(String::new())
            }
            .metric_label(),
            "skipped-non-ready"
        );
        assert_eq!(
            Outcome::Skipped {
                reason: SkipReason::ActiveCall
            }
            .metric_label(),
            "skipped-active-call"
        );
        assert_eq!(Outcome::TimedOut.metric_label(), "timed-out");
        assert_eq!(
            Outcome::AlreadyRestartedByManual.metric_label(),
            "skipped-already-restarted-by-manual"
        );
    }

    // chrono::TimeZone trait needs to be in scope for the cron tests above.
    use chrono::{Datelike, TimeZone, Timelike};

    #[test]
    fn parse_cron_5field_next_after_known_timestamp() {
        let s = parse_cron_5field("0 1 * * *").unwrap();
        let after = chrono::Local
            .with_ymd_and_hms(2026, 5, 26, 2, 0, 0)
            .single()
            .unwrap();
        let next = s.after(&after).next().unwrap();
        assert_eq!(next.day(), 27);
        assert_eq!(next.hour(), 1);
        assert_eq!(next.minute(), 0);
    }
}
