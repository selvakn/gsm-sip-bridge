//! End-to-end integration tests for feature 010 — scheduled card auto-restart.
//!
//! These tests drive the cycle FSM directly (the same code path that
//! `CardPool::advance_scheduler` calls in production) and assert ordering,
//! deferred-retry behavior, and Prometheus metric emission. Full CardPool::run
//! drive-through requires mocking the SIP bridge / store / SMS handler stack,
//! which is out of scope for v1; the FSM is identical either way.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use gsm_sip_bridge::metrics;
use gsm_sip_bridge::modules::scheduler::{
    self, AttemptType, CycleOutcome, CyclePhase, CycleState, Outcome, RestartProgress,
    SchedulerAction, SkipReason, SlotView, REBOOT_SETTLE_DELAY,
};
use rand::SeedableRng;

/// Programmable slot view backed by a `Mutex<HashMap>` so tests can mutate
/// state between scheduler ticks.
struct TestView {
    inner: Mutex<TestViewState>,
}

#[derive(Default)]
struct TestViewState {
    ready: HashMap<u32, bool>,
    active_call: HashMap<u32, bool>,
    progress: HashMap<u32, RestartProgress>,
    /// Slots we removed mid-cycle (simulating hot-unplug).
    gone: HashMap<u32, bool>,
}

impl TestView {
    fn new() -> Self {
        Self {
            inner: Mutex::new(TestViewState::default()),
        }
    }
    fn set_ready(&self, slot: u32, v: bool) {
        self.inner.lock().unwrap().ready.insert(slot, v);
    }
    fn set_active_call(&self, slot: u32, v: bool) {
        self.inner.lock().unwrap().active_call.insert(slot, v);
    }
    fn set_progress(&self, slot: u32, p: RestartProgress) {
        self.inner.lock().unwrap().progress.insert(slot, p);
    }
}

impl SlotView for TestView {
    fn is_ready(&self, slot: u32) -> bool {
        let g = self.inner.lock().unwrap();
        if *g.gone.get(&slot).unwrap_or(&false) {
            return false;
        }
        *g.ready.get(&slot).unwrap_or(&true)
    }
    fn non_ready_skip_reason(&self, slot: u32) -> Option<String> {
        let g = self.inner.lock().unwrap();
        if *g.gone.get(&slot).unwrap_or(&false) {
            return Some("slot disappeared".into());
        }
        if !*g.ready.get(&slot).unwrap_or(&true) {
            return Some("Recovering".into());
        }
        None
    }
    fn has_active_call(&self, slot: u32) -> bool {
        *self
            .inner
            .lock()
            .unwrap()
            .active_call
            .get(&slot)
            .unwrap_or(&false)
    }
    fn restart_progress(&self, slot: u32) -> RestartProgress {
        let g = self.inner.lock().unwrap();
        if *g.gone.get(&slot).unwrap_or(&false) {
            return RestartProgress::Gone;
        }
        g.progress
            .get(&slot)
            .cloned()
            .unwrap_or(RestartProgress::InFlight)
    }
}

fn fresh_state(slots: &[u32]) -> CycleState {
    let now = tokio::time::Instant::now();
    CycleState {
        id: 42,
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

fn seeded_rng() -> rand::rngs::StdRng {
    rand::rngs::StdRng::seed_from_u64(0xBADC0FFEE0DDF00D)
}

/// Drive the FSM until either it returns `Complete` or `max_steps` ticks elapse.
/// Mutates simulated time. Returns the recorded actions, in order.
fn run_to_completion(
    state: &mut CycleState,
    view: &TestView,
    on_send_reboot: &mut dyn FnMut(u32, &TestView),
    max_steps: usize,
) -> Vec<SchedulerAction> {
    let mut all_actions = Vec::new();
    let mut rng = seeded_rng();
    let mut now = state.started_at;

    for _ in 0..max_steps {
        let actions = scheduler::tick_scheduler(state, view, now, &mut rng, 0, 0);
        let mut got_complete = false;
        for a in &actions {
            if let SchedulerAction::SendReboot { slot } = a {
                on_send_reboot(*slot, view);
            }
            if matches!(a, SchedulerAction::Complete) {
                got_complete = true;
            }
        }
        all_actions.extend(actions);
        if got_complete {
            return all_actions;
        }
        // Advance time to the next scheduled action (or +1 s for polling).
        now = state.next_action_at;
    }
    panic!("cycle did not complete within {max_steps} steps");
}

#[test]
fn three_card_cycle_runs_in_ascending_order_and_completes() {
    let view = TestView::new();
    for s in [0u32, 1, 2] {
        view.set_ready(s, true);
    }

    let mut state = fresh_state(&[0, 1, 2]);
    let send_log: RefCell<Vec<u32>> = RefCell::new(Vec::new());

    let actions = run_to_completion(
        &mut state,
        &view,
        &mut |slot, v| {
            send_log.borrow_mut().push(slot);
            // Simulate the existing recovery machinery bringing the slot back to
            // Ready inside the per-card timeout window.
            v.set_progress(slot, RestartProgress::Succeeded);
        },
        32,
    );

    let send_log = send_log.into_inner();
    assert_eq!(
        send_log,
        vec![0u32, 1, 2],
        "cards must be restarted in ascending slot order"
    );

    // Verify every slot produced a Success outcome.
    let success_count = state
        .outcomes
        .iter()
        .filter(|o| matches!(o.outcome, Outcome::Success))
        .count();
    assert_eq!(success_count, 3);

    assert_eq!(state.phase, CyclePhase::Complete);

    // Sanity: at least one Complete action emitted.
    assert!(actions
        .iter()
        .any(|a| matches!(a, SchedulerAction::Complete)));
}

#[test]
fn deferred_slot_succeeds_on_retry_when_call_ends() {
    let view = TestView::new();
    view.set_ready(0, true);
    view.set_ready(1, true);
    view.set_ready(2, true);
    // Slot 1 has an active call when the cycle starts; pending order is 0,1,2.
    // The call ends after slot 2's restart, before slot 1's deferred retry.
    view.set_active_call(1, true);

    let mut state = fresh_state(&[0, 1, 2]);

    let _ = run_to_completion(
        &mut state,
        &view,
        &mut |slot, v| {
            // When slot 2 (the last initial-pass card) finishes restarting,
            // simulate that the call on slot 1 has ended naturally.
            if slot == 2 {
                v.set_active_call(1, false);
            }
            v.set_progress(slot, RestartProgress::Succeeded);
        },
        64,
    );

    // Slot 1 must have one Initial-attempt Deferred outcome.
    let initial_defer = state.outcomes.iter().any(|o| {
        o.slot == 1
            && o.attempt == AttemptType::Initial
            && matches!(o.outcome, Outcome::Deferred { .. })
    });
    assert!(
        initial_defer,
        "slot 1 must have been deferred on its initial attempt"
    );

    // Slot 1 must have one DeferredRetry-attempt Success.
    let retry_success = state.outcomes.iter().any(|o| {
        o.slot == 1
            && o.attempt == AttemptType::DeferredRetry
            && matches!(o.outcome, Outcome::Success)
    });
    assert!(
        retry_success,
        "slot 1's deferred retry must succeed when the call has ended"
    );

    // Slot 0 must have one Initial-attempt Success.
    let initial_success = state.outcomes.iter().any(|o| {
        o.slot == 0 && o.attempt == AttemptType::Initial && matches!(o.outcome, Outcome::Success)
    });
    assert!(
        initial_success,
        "slot 0 must succeed on its initial attempt"
    );

    assert_eq!(state.phase, CyclePhase::Complete);
}

#[test]
fn deferred_slot_skipped_when_call_still_active_on_retry() {
    let view = TestView::new();
    view.set_ready(2, true);
    view.set_active_call(2, true); // Active call never clears.

    let mut state = fresh_state(&[2]);
    let _ = run_to_completion(
        &mut state,
        &view,
        &mut |slot, v| {
            v.set_progress(slot, RestartProgress::Succeeded);
        },
        16,
    );

    // Initial-attempt deferred.
    let deferred = state.outcomes.iter().any(|o| {
        o.slot == 2
            && o.attempt == AttemptType::Initial
            && matches!(o.outcome, Outcome::Deferred { .. })
    });
    assert!(deferred);

    // DeferredRetry-attempt skipped with reason ActiveCall.
    let skipped = state.outcomes.iter().any(|o| {
        o.slot == 2
            && o.attempt == AttemptType::DeferredRetry
            && matches!(
                o.outcome,
                Outcome::Skipped {
                    reason: SkipReason::ActiveCall
                }
            )
    });
    assert!(
        skipped,
        "deferred-retry must record SkipReason::ActiveCall when call is still live"
    );
}

#[test]
fn non_ready_slot_skipped_immediately() {
    let view = TestView::new();
    view.set_ready(5, false); // slot 5 is not ready
    view.set_ready(6, true);

    let mut state = fresh_state(&[5, 6]);
    let _ = run_to_completion(
        &mut state,
        &view,
        &mut |slot, v| {
            v.set_progress(slot, RestartProgress::Succeeded);
        },
        32,
    );

    // Slot 5: skipped non-ready.
    let skipped_5 = state.outcomes.iter().any(|o| {
        o.slot == 5
            && matches!(
                o.outcome,
                Outcome::Skipped {
                    reason: SkipReason::NonReady(_)
                }
            )
    });
    assert!(skipped_5, "slot 5 must be skipped as non-ready");

    // Slot 6: success.
    let succ_6 = state
        .outcomes
        .iter()
        .any(|o| o.slot == 6 && matches!(o.outcome, Outcome::Success));
    assert!(succ_6, "slot 6 must succeed");
}

#[test]
fn metric_counter_increments_for_every_outcome_label() {
    // Reset is impossible (counters are monotonic), but we can capture
    // before/after values and assert the delta.
    let before = {
        // Touch every metric we expect to see, to ensure they're registered.
        let success = metrics::SCHEDULED_RESTART_TOTAL
            .with_label_values(&["0", "success"])
            .get();
        let skipped_nonready = metrics::SCHEDULED_RESTART_TOTAL
            .with_label_values(&["1", "skipped-non-ready"])
            .get();
        (success, skipped_nonready)
    };

    // Simulate two outcomes by calling the same `metric_label()` path the pool uses.
    metrics::SCHEDULED_RESTART_TOTAL
        .with_label_values(&["0", Outcome::Success.metric_label()])
        .inc();
    metrics::SCHEDULED_RESTART_TOTAL
        .with_label_values(&[
            "1",
            Outcome::Skipped {
                reason: SkipReason::NonReady("Recovering".into()),
            }
            .metric_label(),
        ])
        .inc();

    let after_success = metrics::SCHEDULED_RESTART_TOTAL
        .with_label_values(&["0", "success"])
        .get();
    let after_skipped = metrics::SCHEDULED_RESTART_TOTAL
        .with_label_values(&["1", "skipped-non-ready"])
        .get();

    assert!(
        after_success > before.0,
        "success counter must increment ({} -> {})",
        before.0,
        after_success
    );
    assert!(
        after_skipped > before.1,
        "skipped-non-ready counter must increment ({} -> {})",
        before.1,
        after_skipped
    );
}

#[test]
fn per_card_timeout_records_timed_out() {
    let view = TestView::new();
    view.set_ready(0, true);
    // Never call set_progress(0, Succeeded) — slot stays InFlight forever.

    let mut state = fresh_state(&[0]);
    let mut rng = seeded_rng();
    let now0 = state.started_at;

    // Tick 1: send reboot.
    let a1 = scheduler::tick_scheduler(&mut state, &view, now0, &mut rng, 0, 0);
    assert!(a1
        .iter()
        .any(|a| matches!(a, SchedulerAction::SendReboot { slot: 0 })));
    let current = state.current.clone().unwrap();

    // Jump time past the per-card deadline.
    let past_deadline = current.deadline + Duration::from_millis(1);
    let a2 = scheduler::tick_scheduler(&mut state, &view, past_deadline, &mut rng, 0, 0);
    let timed_out = a2.iter().any(|a| {
        matches!(
            a,
            SchedulerAction::RecordOutcome {
                outcome: CycleOutcome {
                    outcome: Outcome::TimedOut,
                    ..
                },
                slot: 0,
            }
        )
    });
    assert!(timed_out, "per-card timeout must record Outcome::TimedOut");
}

#[test]
fn manual_restart_pending_slot_marks_already_restarted() {
    use gsm_sip_bridge::modules::scheduler::{
        handle_manual_restart_during_cycle, ManualRestartCycleAdvice,
    };
    let mut state = fresh_state(&[0, 1, 2]);
    // Manual restart slot 2 while it's still pending.
    let advice = handle_manual_restart_during_cycle(&mut state, 2);
    assert_eq!(advice, ManualRestartCycleAdvice::PreemptAndProceed);
    assert!(
        !state.pending.contains(&2),
        "slot 2 must be removed from pending"
    );
    assert!(state
        .outcomes
        .iter()
        .any(|o| o.slot == 2 && matches!(o.outcome, Outcome::AlreadyRestartedByManual)));
}

#[test]
fn manual_restart_currently_restarting_slot_is_rejected() {
    use gsm_sip_bridge::modules::scheduler::{
        handle_manual_restart_during_cycle, CurrentCard, ManualRestartCycleAdvice, PER_CARD_TIMEOUT,
    };
    let mut state = fresh_state(&[0]);
    let now = state.started_at;
    state.current = Some(CurrentCard {
        slot: 0,
        attempt: AttemptType::Initial,
        started_at: now,
        deadline: now + PER_CARD_TIMEOUT,
    });
    let advice = handle_manual_restart_during_cycle(&mut state, 0);
    match advice {
        ManualRestartCycleAdvice::Reject { error } => {
            assert!(error.contains("currently being restarted"));
            assert!(error.contains("cycle id="));
        }
        other => panic!("expected Reject, got {other:?}"),
    }
}

#[test]
fn manual_restart_already_processed_slot_proceeds_normally() {
    use gsm_sip_bridge::modules::scheduler::{
        handle_manual_restart_during_cycle, ManualRestartCycleAdvice,
    };
    let mut state = fresh_state(&[]); // empty pending
                                      // Slot 3 was already processed earlier in this cycle.
    state.outcomes.push(CycleOutcome {
        slot: 3,
        attempt: AttemptType::Initial,
        outcome: Outcome::Success,
        duration: Duration::from_secs(15),
    });
    let advice = handle_manual_restart_during_cycle(&mut state, 3);
    assert_eq!(advice, ManualRestartCycleAdvice::Proceed);
}

#[test]
fn no_catch_up_on_startup_uses_future_occurrence() {
    // FR-015: if the bridge starts after the most recent cron tick, the
    // scheduler must arm the *next* future occurrence, not the missed one.
    let schedule = scheduler::parse_cron_5field("0 1 * * *").unwrap();
    // Simulate "now" as 03:00 local — well past today's 01:00 tick.
    use chrono::TimeZone;
    let now_local = chrono::Local
        .with_ymd_and_hms(2026, 5, 26, 3, 0, 0)
        .single()
        .unwrap();
    let next = scheduler::compute_next_scheduled_at(&schedule, now_local).unwrap();
    // The next tick must be strictly AFTER now (tomorrow 01:00, not today's missed 01:00).
    assert!(next > now_local, "next must be after now (no catch-up)");
    use chrono::Datelike;
    assert_eq!(next.day(), 27, "must be tomorrow, not today");
}

#[test]
fn cycle_starts_with_reboot_settle_delay() {
    // After SendReboot, the next_action_at must be exactly started_at + REBOOT_SETTLE_DELAY
    // so the FSM gives the modem time to reboot before polling progress.
    let view = TestView::new();
    view.set_ready(0, true);
    let mut state = fresh_state(&[0]);
    let mut rng = seeded_rng();
    let started_at = state.started_at;
    let _ = scheduler::tick_scheduler(&mut state, &view, started_at, &mut rng, 0, 0);
    let current = state.current.as_ref().unwrap();
    assert_eq!(
        state.next_action_at - current.started_at,
        REBOOT_SETTLE_DELAY
    );
}
