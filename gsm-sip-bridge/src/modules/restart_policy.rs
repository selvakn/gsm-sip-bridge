//! How hard to restart a card, and how long to wait afterward before
//! recovery is allowed to retry it.
//!
//! Split out of `modules::mod` because all four items here are pure functions
//! of config plus two booleans — no `CardPool`, no slot map, no AT commander.
//! They were already written to be testable in isolation (see
//! `scheduled_restart_mode`'s and `retry_delay_for`'s doc comments, both of
//! which say so explicitly); this file is just where that isolation finally
//! shows up in the module tree.

use crate::config::ScheduledRestartConfig;
use std::str::FromStr;
use std::time::Duration;

/// How hard to restart a card — shared by the scheduled-restart cycle
/// (`[scheduled_restart].restart_mode`) and manual `card restart` (always
/// `Full`, matching this crate's long-standing behavior).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartMode {
    /// `AT+CFUN=0` -> `AT+CFUN=1`: drops and re-acquires network
    /// registration without power-cycling the module or re-enumerating USB.
    Radio,
    /// `AT+CFUN=1,1`: a full module reset. Can move the card's ttyUSB path.
    Full,
}

/// Parses the same two values `[scheduled_restart].restart_mode` accepts
/// (`build_scheduled_restart`, config/build.rs), for the manual `card
/// restart --mode` control command — mirrors `NetworkMode::from_str`'s
/// shape (`modules::at_commander`).
impl FromStr for RestartMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "full" => Ok(RestartMode::Full),
            "radio" => Ok(RestartMode::Radio),
            _ => Err(format!(
                "unknown restart mode: {s} (expected \"full\" or \"radio\")"
            )),
        }
    }
}

/// `[scheduled_restart].restart_mode` -> `RestartMode`, pulled out of
/// `CardPool::apply_send_reboot` so the decision is testable without
/// constructing a whole `CardPool`. `build_scheduled_restart` (config/build.rs)
/// already rejects anything other than `"full"`/`"radio"` at load time (the
/// section is disabled instead), so this only ever sees one of the two.
pub(crate) fn scheduled_restart_mode(config: &ScheduledRestartConfig) -> RestartMode {
    if config.restart_mode == "radio" {
        RestartMode::Radio
    } else {
        RestartMode::Full
    }
}

/// How long a restarted slot stays `Recovering` before recovery is allowed
/// to retry it, when the restart command went to a live worker (`cmd_tx`)
/// running `RestartMode::Full` (`AT+CFUN=1,1`, one command, ~5s worst-case
/// AT timeout — comfortably under this). Just a starting estimate — the
/// worker's eventual exit (picked up via `CardPool::run`'s `JoinSet`)
/// recomputes `next_retry_at` for real once the restart actually finishes,
/// so this only matters for the brief window before that.
pub(crate) const RECOVERY_RETRY_DELAY: Duration = Duration::from_secs(10);

/// Same idea, but for anything driving `RestartMode::Radio`. Sized from two
/// components, both worst-case:
///
/// - *Pickup latency* before the restart even starts: on the `cmd_tx`/worker
///   path, `run_module_loop`'s own idle loop only checks `cmd_rx` once per
///   iteration, and one iteration can already take ~10s (an `AT_WORKER_PROBE_INTERVAL`
///   liveness probe, itself an AT round trip up to `AtCommander`'s ~5s read
///   timeout, followed by `read_line_from_at` blocking up to that same ~5s
///   timeout waiting for a URC). The no-worker fallback has no such delay
///   itself, but shares the same uncertainty from a different source: tokio's
///   blocking pool may not run its `spawn_blocking` closure the instant it's
///   queued.
/// - The restart sequence itself: `AT+CFUN=0` (~5s worst-case AT timeout) ->
///   `sleep(4s)` -> `AT+CFUN=1` (~5s worst-case) — ~14s.
///
/// ~24s total, rounded up for margin. `next_retry_at` set to `now +` this is
/// still only a *backstop*, not a guarantee: both paths' actual completion
/// is tracked for real via `CardPool::run`'s `JoinSet` (`tasks`), whose
/// `join_next` branch recomputes `next_retry_at` from the real exit event —
/// that's what actually closes the race in the common case, this constant
/// only bounds how bad the window is if it doesn't land in time. (A `JoinSet`
/// task that panics loses its `(slot, module_id)` payload — `join_next`'s
/// `Err` arm can't identify which slot to correct — so an unbounded wait
/// instead of a numeric backstop isn't a safe alternative: a panicking
/// restart would leave that slot stuck in `Recovering` forever instead of
/// eventually retried.)
///
/// Either way, recovery reopening the same serial port before the old
/// restart releases it would interleave AT commands, fail initialization,
/// or leave the modem in an inconsistent radio state (Greptile review,
/// PR #30) — so both paths use this generous margin whenever `Radio` is the
/// selected mode.
pub(crate) const RADIO_RESTART_RETRY_DELAY: Duration = Duration::from_secs(30);

/// How long `apply_send_reboot` should park a slot in `Recovering` before
/// recovery may retry it, given the selected restart mode and whether a
/// live worker (`cmd_tx`) took the command or the no-worker fallback ran it
/// detached. Pulled out of `apply_send_reboot` so this decision is testable
/// without constructing a whole `CardPool` — same reasoning as
/// `scheduled_restart_mode`. See `RECOVERY_RETRY_DELAY`/
/// `RADIO_RESTART_RETRY_DELAY`'s doc comments for why these differ.
pub(crate) fn retry_delay_for(mode: RestartMode, had_worker: bool) -> Duration {
    if had_worker && mode == RestartMode::Full {
        RECOVERY_RETRY_DELAY
    } else {
        RADIO_RESTART_RETRY_DELAY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_mode_from_str_parses_full_and_radio() {
        assert_eq!("full".parse::<RestartMode>(), Ok(RestartMode::Full));
        assert_eq!("radio".parse::<RestartMode>(), Ok(RestartMode::Radio));
    }

    #[test]
    fn restart_mode_from_str_rejects_unknown_values() {
        // No case-folding, matching NetworkMode::from_str's own strictness
        // (modules::at_commander) — the CLI/config both document the exact
        // accepted spelling, so a typo should surface as an error, not
        // silently match something.
        assert!("Radio".parse::<RestartMode>().is_err());
        assert!("nuke-from-orbit".parse::<RestartMode>().is_err());
    }

    #[test]
    fn scheduled_restart_mode_selects_radio_when_configured() {
        let cfg = ScheduledRestartConfig {
            restart_mode: "radio".to_string(),
            ..ScheduledRestartConfig::default()
        };
        assert_eq!(scheduled_restart_mode(&cfg), RestartMode::Radio);
    }

    #[test]
    fn scheduled_restart_mode_defaults_to_full() {
        assert_eq!(
            scheduled_restart_mode(&ScheduledRestartConfig::default()),
            RestartMode::Full,
            "the default config's restart_mode is \"full\", preserving old behavior"
        );
    }

    #[test]
    fn retry_delay_for_full_mode_with_a_live_worker_uses_the_short_delay() {
        // The only combination that's actually safe with the historical
        // flat 10s: AT+CFUN=1,1 is one command, ~5s worst case, and the
        // worker's own eventual exit event recomputes next_retry_at anyway.
        assert_eq!(
            retry_delay_for(RestartMode::Full, true),
            RECOVERY_RETRY_DELAY
        );
    }

    #[test]
    fn retry_delay_for_radio_mode_uses_the_long_delay_even_with_a_live_worker() {
        // Greptile review (PR #30): radio mode's ~14s worst case can outlast
        // RECOVERY_RETRY_DELAY even on the tracked cmd_tx/worker path, since
        // next_retry_at is only an initial estimate until the worker's real
        // exit event supersedes it.
        assert_eq!(
            retry_delay_for(RestartMode::Radio, true),
            RADIO_RESTART_RETRY_DELAY
        );
    }

    #[test]
    fn retry_delay_for_the_no_worker_fallback_uses_the_long_delay_regardless_of_mode() {
        // Greptile review (PR #30): the detached fallback has nothing that
        // recomputes next_retry_at on completion, for either mode.
        assert_eq!(
            retry_delay_for(RestartMode::Full, false),
            RADIO_RESTART_RETRY_DELAY
        );
        assert_eq!(
            retry_delay_for(RestartMode::Radio, false),
            RADIO_RESTART_RETRY_DELAY
        );
    }
}
