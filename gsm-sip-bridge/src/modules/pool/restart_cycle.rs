//! `CardPool`'s half of the `[scheduled_restart]` feature: arming the cron
//! schedule, driving one restart cycle to completion, and logging outcomes.
//!
//! The cycle's *decisions* are pure and live in `modules::scheduler`
//! (`tick_scheduler` and friends). Everything here is the side-effecting
//! shell around them — sending the reboot, mutating slot lifecycle, emitting
//! metrics and tracing. Split from `pool::mod` because it is the only concern
//! that touches `cycle`/`cron_schedule`/`next_scheduled_at`, and none of the
//! rest of the pool touches those.

use super::CardPool;
use crate::metrics;
use crate::modules::at_commander::AtCommander;
use crate::modules::protocol::ModuleCmd;
use crate::modules::restart_policy::{retry_delay_for, scheduled_restart_mode, RestartMode};
use crate::modules::scheduler::{
    self, AttemptType, CycleOutcome, CyclePhase, CycleState, Outcome, SchedulerAction, SkipReason,
};
use crate::modules::slot::{LifecycleState, PoolSlotView, SlotState};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use tokio::task::JoinSet;

impl CardPool {
    /// Compute the next jittered cycle start instant from the cron schedule.
    /// Returns `None` if the schedule is disabled or has no future occurrence.
    pub(super) fn recompute_next_scheduled_at(&mut self) {
        let Some(schedule) = self.cron_schedule.as_ref() else {
            self.next_scheduled_at = None;
            return;
        };
        let now_local = chrono::Local::now();
        // Use the last natural cron tick as the lower bound so we never re-fire
        // the same occurrence regardless of jitter direction.  On the very first
        // call `last_fired_tick` is None, so we fall back to `now_local`.
        let after = self.last_fired_tick.unwrap_or(now_local);
        let Some(next_tick) = schedule.after(&after).next() else {
            tracing::warn!("scheduled_restart has no future cron occurrence; disabling scheduler");
            self.cron_schedule = None;
            self.next_scheduled_at = None;
            return;
        };
        // Persist the natural tick immediately so the next recompute call always
        // advances past this occurrence, even if the jittered start lands earlier.
        self.last_fired_tick = Some(next_tick);
        let mut rng = rand::rng();
        let jitter =
            scheduler::jitter_offset(&mut rng, self.config.scheduled_restart.start_jitter_seconds);
        let delta_sec = (next_tick - now_local).num_seconds() + jitter;
        let now_instant = tokio::time::Instant::now();
        let target = if delta_sec <= 0 {
            now_instant
        } else {
            now_instant + Duration::from_secs(delta_sec as u64)
        };
        self.next_scheduled_at = Some(target);
        tracing::info!(
            next_cron_tick = %next_tick,
            jittered_delta_seconds = delta_sec,
            "scheduled_restart next cycle armed"
        );
    }

    pub(super) fn advance_scheduler(
        &mut self,
        slots: &mut HashMap<u32, SlotState>,
        tasks: &mut JoinSet<(u32, String)>,
        now: tokio::time::Instant,
    ) {
        // 1) If no cycle is active and the scheduled instant has arrived, start one.
        if self.cycle.is_none() {
            let Some(scheduled) = self.next_scheduled_at else {
                return;
            };
            if now < scheduled {
                return;
            }
            self.start_cycle(slots, now);
            return;
        }

        // 2) If a cycle is active and its next-action deadline has arrived, tick it.
        let Some(cycle) = self.cycle.as_mut() else {
            return;
        };
        if now < cycle.next_action_at {
            return;
        }

        let view = PoolSlotView { slots };
        let mut rng = rand::rng();
        let actions = scheduler::tick_scheduler(
            cycle,
            &view,
            now,
            &mut rng,
            self.config.scheduled_restart.inter_card_gap_seconds,
            self.config.scheduled_restart.inter_card_gap_jitter_seconds,
        );

        let mut complete = false;
        for action in actions {
            match action {
                SchedulerAction::SendReboot { slot } => {
                    self.apply_send_reboot(slots, tasks, slot, now);
                }
                SchedulerAction::RecordOutcome { slot, outcome } => {
                    self.record_outcome(slot, &outcome);
                }
                SchedulerAction::Complete => {
                    complete = true;
                }
            }
        }

        if complete {
            self.complete_cycle();
        }
    }

    fn start_cycle(&mut self, slots: &HashMap<u32, SlotState>, now: tokio::time::Instant) {
        // FR-014 guard belongs here too: if a previous cycle is somehow still
        // active (shouldn't be — we cleared next_scheduled_at on cycle start)
        // bail out.
        if self.cycle.is_some() {
            tracing::warn!(
                "scheduled_restart cycle-trigger-dropped: a previous cycle is still active"
            );
            return;
        }

        let cron_tick = chrono::Local::now();
        let id = cron_tick.timestamp().max(0) as u64;

        let mut as_vec: Vec<u32> = slots.keys().copied().collect();
        as_vec.sort_unstable();
        let pending: VecDeque<u32> = as_vec.iter().copied().collect();

        tracing::info!(
            cycle_id = id,
            cron_tick = %cron_tick,
            actual_start = %chrono::Local::now(),
            n_slots = pending.len(),
            pending_slots = ?as_vec,
            "scheduled_restart cycle-start"
        );

        // `last_fired_tick` is already set by `recompute_next_scheduled_at` to
        // the natural cron tick, so the next recompute advances past it.
        self.next_scheduled_at = None;

        self.cycle = Some(CycleState {
            id,
            cron_tick,
            started_at: now,
            phase: CyclePhase::Initial,
            pending,
            deferred: VecDeque::new(),
            current: None,
            next_action_at: now,
            outcomes: Vec::new(),
        });
    }

    fn apply_send_reboot(
        &self,
        slots: &mut HashMap<u32, SlotState>,
        tasks: &mut JoinSet<(u32, String)>,
        slot: u32,
        now: tokio::time::Instant,
    ) {
        let cycle_id = self.cycle.as_ref().map(|c| c.id).unwrap_or(0);
        let attempt = self
            .cycle
            .as_ref()
            .and_then(|c| c.current.as_ref().map(|cc| cc.attempt))
            .unwrap_or(AttemptType::Initial);

        let Some(state) = slots.get_mut(&slot) else {
            tracing::warn!(
                slot = slot,
                "scheduled_restart attempted to reboot a slot that vanished mid-cycle"
            );
            return;
        };

        tracing::info!(
            cycle_id = cycle_id,
            slot = slot,
            module = %state.module.id,
            attempt = %attempt,
            "scheduled_restart per-card-start"
        );

        // `[scheduled_restart].restart_mode` picks the restart's severity for
        // every card in this cycle; manual `card restart` always does a full
        // reset regardless of this setting.
        let mode = scheduled_restart_mode(&self.config.scheduled_restart);
        let retry_delay = retry_delay_for(mode, state.cmd_tx.is_some());

        // Mirror the manual `card restart` code path: send Reboot via the worker
        // if present, else open the serial port directly.
        if let Some(cmd_tx) = state.cmd_tx.take() {
            let _ = cmd_tx.send(ModuleCmd::Reboot(mode));
        } else {
            // No worker owns this slot right now, so there's no dedicated OS
            // thread to hand the command to — unlike the `cmd_tx` path above.
            // Open the port and run the restart on tokio's blocking pool
            // rather than inline: `RestartMode::Radio` is `AT+CFUN=0` ->
            // `sleep(4s)` -> `AT+CFUN=1`, up to ~14s of synchronous AT I/O
            // that would otherwise stall this CardPool's shared event loop —
            // every other card's control commands and bridge events — for
            // the duration (Greptile review, PR #30).
            //
            // Spawned on `tasks` (the same JoinSet every module worker
            // lives in), not a bare detached `tokio::task::spawn_blocking`:
            // that JoinSet is drained by CardPool::run's `join_next` branch,
            // which recomputes `next_retry_at` from this task's *actual*
            // completion. A bare detached task has nothing to correct the
            // `retry_delay` estimate below if tokio's blocking pool is busy
            // enough to delay when this closure even starts running — Greptile
            // review, PR #30, follow-up — so `retry_delay` here is only a
            // defensive starting point, exactly like the `cmd_tx` path's own
            // estimate above it (RECOVERY_RETRY_DELAY's doc comment).
            spawn_fallback_restart(
                tasks,
                state.module.serial_port.clone(),
                state.module.id.clone(),
                slot,
                mode,
                "scheduled_restart",
            );
        }
        state.lifecycle = LifecycleState::Recovering;
        state.retry_count = 0;
        state.next_retry_at = Some(now + retry_delay);
    }

    pub(super) fn record_outcome(&self, slot: u32, outcome: &CycleOutcome) {
        let cycle_id = self.cycle.as_ref().map(|c| c.id).unwrap_or(0);
        let label = outcome.outcome.metric_label();
        metrics::SCHEDULED_RESTART_TOTAL
            .with_label_values(&[&slot.to_string(), label])
            .inc();

        match &outcome.outcome {
            Outcome::Success => {
                tracing::info!(
                    cycle_id = cycle_id,
                    slot = slot,
                    attempt = %outcome.attempt,
                    outcome = "success",
                    duration_ms = outcome.duration.as_millis() as u64,
                    "scheduled_restart per-card-outcome"
                );
            }
            Outcome::Failed { reason } => {
                tracing::warn!(
                    cycle_id = cycle_id,
                    slot = slot,
                    attempt = %outcome.attempt,
                    outcome = "failed",
                    reason = %reason,
                    duration_ms = outcome.duration.as_millis() as u64,
                    "scheduled_restart per-card-outcome"
                );
            }
            Outcome::TimedOut => {
                tracing::warn!(
                    cycle_id = cycle_id,
                    slot = slot,
                    attempt = %outcome.attempt,
                    outcome = "timed-out",
                    duration_ms = outcome.duration.as_millis() as u64,
                    "scheduled_restart per-card-outcome"
                );
            }
            Outcome::Deferred { reason } => {
                tracing::debug!(
                    cycle_id = cycle_id,
                    slot = slot,
                    attempt = %outcome.attempt,
                    outcome = "deferred",
                    reason = %reason,
                    "scheduled_restart per-card-outcome"
                );
            }
            Outcome::Skipped { reason } => {
                let reason_str = match reason {
                    SkipReason::NonReady(s) => format!("non-ready: {s}"),
                    SkipReason::ActiveCall => "active-call (after deferred retry)".to_string(),
                    SkipReason::SlotDisappeared => "slot disappeared".to_string(),
                };
                tracing::debug!(
                    cycle_id = cycle_id,
                    slot = slot,
                    attempt = %outcome.attempt,
                    outcome = "skipped",
                    reason = %reason_str,
                    "scheduled_restart per-card-outcome"
                );
            }
            Outcome::AlreadyRestartedByManual => {
                tracing::debug!(
                    cycle_id = cycle_id,
                    slot = slot,
                    attempt = %outcome.attempt,
                    outcome = "skipped-already-restarted-by-manual",
                    "scheduled_restart per-card-outcome"
                );
            }
        }
    }

    fn complete_cycle(&mut self) {
        let Some(cycle) = self.cycle.take() else {
            return;
        };

        let total = cycle.outcomes.len();
        let succeeded = cycle
            .outcomes
            .iter()
            .filter(|o| matches!(o.outcome, Outcome::Success))
            .count();
        let failed = cycle
            .outcomes
            .iter()
            .filter(|o| matches!(o.outcome, Outcome::Failed { .. } | Outcome::TimedOut))
            .count();
        let deferred_recovered = cycle
            .outcomes
            .iter()
            .filter(|o| {
                matches!(o.outcome, Outcome::Success) && o.attempt == AttemptType::DeferredRetry
            })
            .count();
        let skipped = cycle
            .outcomes
            .iter()
            .filter(|o| {
                matches!(
                    o.outcome,
                    Outcome::Skipped { .. } | Outcome::AlreadyRestartedByManual
                )
            })
            .count();
        let duration_ms = tokio::time::Instant::now()
            .duration_since(cycle.started_at)
            .as_millis() as u64;

        tracing::info!(
            cycle_id = cycle.id,
            total = total,
            succeeded = succeeded,
            failed = failed,
            deferred_recovered = deferred_recovered,
            skipped = skipped,
            duration_ms = duration_ms,
            "scheduled_restart cycle-complete"
        );

        self.recompute_next_scheduled_at();
    }
}

/// Opens the modem port and restarts it on tokio's blocking pool, for the
/// case where no worker thread owns the slot. Shared by the scheduled cycle
/// and manual `card restart`, which had identical copies of this — see
/// `apply_send_reboot`'s comment for why it is tracked in `tasks` rather than
/// detached. `context` only labels the warning if the port will not open.
pub(super) fn spawn_fallback_restart(
    tasks: &mut JoinSet<(u32, String)>,
    serial_port: std::path::PathBuf,
    module_id: String,
    slot: u32,
    mode: RestartMode,
    context: &'static str,
) {
    tasks.spawn_blocking(move || {
        match AtCommander::open(&serial_port) {
            Ok(mut at) => match mode {
                RestartMode::Radio => at.radio_restart(),
                RestartMode::Full => at.reboot(),
            },
            Err(e) => {
                tracing::warn!(
                    module = %module_id,
                    error = %e,
                    "{context}: could not open modem port for fallback restart"
                );
            }
        }
        (slot, module_id)
    });
}
