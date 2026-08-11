//! One modem's runtime record in the card pool, and the small pure helpers
//! that query a whole slot map.
//!
//! Split out of `modules::mod` because `SlotState` is the central data type
//! every other piece of the pool takes a `&HashMap<u32, SlotState>` of — the
//! scheduler adapter, the retry loop, the control-command handlers and the
//! outbound-dial claim all read it, and none of them owns it. Fields are
//! `pub(crate)` rather than private because `modules::pool`'s three `impl`
//! blocks live in sibling modules and mutate them directly, exactly as they
//! did when everything shared one file.

use crate::control::protocol::SlotInfo;
use crate::modules::at_commander::{NetworkMode, NetworkType};
use crate::modules::discovery::DiscoveredModule;
use crate::modules::protocol::ModuleCmd;
use crate::modules::scheduler::{RestartProgress, SlotView};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LifecycleState {
    Initializing,
    Ready,
    Recovering,
    GivenUp,
}

impl std::fmt::Display for LifecycleState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LifecycleState::Initializing => write!(f, "Initializing"),
            LifecycleState::Ready => write!(f, "Ready"),
            LifecycleState::Recovering => write!(f, "Recovering"),
            LifecycleState::GivenUp => write!(f, "GivenUp"),
        }
    }
}

pub(crate) struct SlotState {
    pub(crate) slot: u32,
    pub(crate) module: DiscoveredModule,
    pub(crate) imei: String,
    pub(crate) phone_number: String,
    pub(crate) network_type: NetworkType,
    pub(crate) network_mode: Option<NetworkMode>,
    pub(crate) lifecycle: LifecycleState,
    pub(crate) retry_count: u32,
    pub(crate) next_retry_at: Option<tokio::time::Instant>,
    pub(crate) cmd_tx: Option<crossbeam_channel::Sender<ModuleCmd>>,
    pub(crate) has_active_call: bool,
}

impl SlotState {
    /// This card's phone number for alert notifications (specs/034-alert-
    /// identity), or `None` when the SIM has no usable number — empty, or the
    /// `AT+CNUM` `"Unknown"` sentinel — in which case the alert renders
    /// `unknown` in its place.
    pub(crate) fn alert_phone(&self) -> Option<String> {
        usable_phone(&self.phone_number)
    }

    pub(crate) fn info(&self) -> SlotInfo {
        SlotInfo {
            slot: self.slot,
            state: self.lifecycle.to_string(),
            phone: if self.phone_number.is_empty() {
                "Unknown".to_string()
            } else {
                self.phone_number.clone()
            },
            network: self.network_type.to_string(),
        }
    }
}

/// Normalizes a raw `AT+CNUM` result into an alertable number (specs/034-alert-
/// identity): `None` for the empty string or the `"Unknown"` sentinel
/// `query_phone_number` returns when EF_MSISDN is blank, else the trimmed
/// number. Shared by `SlotState::alert_phone` and the pool's worker spawn, so
/// the card's number is read once (at init) and reused, not re-queried.
pub(crate) fn usable_phone(phone: &str) -> Option<String> {
    let p = phone.trim();
    (!p.is_empty() && p != "Unknown").then(|| p.to_string())
}

pub(crate) fn backoff_delay(attempt: u32, initial_sec: u64, max_sec: u64) -> Duration {
    let shift = attempt.min(30);
    let secs = initial_sec.saturating_mul(1u64 << shift);
    Duration::from_secs(secs.min(max_sec))
}

/// specs/022-discord-critical-alerts (Greptile P1 fix): finds a stale
/// `GivenUp` slot for `module_id`, if one exists under a *different* key —
/// rediscovery (a USB rescan or a fresh startup scan) creates a brand new
/// slot rather than reusing the old one, so without this the old slot would
/// otherwise sit there forever with no recovery notification ever sent and
/// its `CRITICAL_EVENT_ACTIVE` gauge stuck at 1.
pub(crate) fn find_given_up_slot(slots: &HashMap<u32, SlotState>, module_id: &str) -> Option<u32> {
    slots
        .iter()
        .find(|(_, s)| s.lifecycle == LifecycleState::GivenUp && s.module.id == module_id)
        .map(|(k, _)| *k)
}

/// Selects the first `Ready`, non-busy CS slot and claims it
/// (`has_active_call = true`) in the same pass, before the caller ever
/// dispatches a `ModuleCmd::Dial` — pulled out of `handle_outbound_request`
/// so the claim discipline itself (not the SIP-side accept/refuse plumbing
/// around it, which needs a real `SipBridge`) is directly unit-testable
/// (specs/025-outbound-calling, T025). No path preference beyond "first
/// idle wins" (FR-007) — iterates every slot in `slots`, not just one line,
/// so this covers every CS modem attached to the host.
///
/// Calling this twice in a row with the same `slots` map and only one
/// idle slot returns `Some` then `None` — the whole point: a second
/// outbound request (or a concurrent inbound GSM call reusing the same
/// slot-claim convention) sees the slot as busy the instant this returns,
/// not only once the `ModuleCmd::Dial` round trip finishes.
pub(crate) fn claim_idle_cs_slot(
    slots: &mut HashMap<u32, SlotState>,
) -> Option<(u32, String, crossbeam_channel::Sender<ModuleCmd>)> {
    let (slot, audio_device, cmd_tx) = slots
        .iter()
        .find(|(_, s)| s.lifecycle == LifecycleState::Ready && !s.has_active_call)
        .map(|(slot, s)| (*slot, s.module.audio_device.clone(), s.cmd_tx.clone()))?;
    let cmd_tx = cmd_tx?;

    if let Some(state) = slots.get_mut(&slot) {
        state.has_active_call = true;
    }
    Some((slot, audio_device, cmd_tx))
}

/// `SlotView` implementation backed by the pool's slot map. Built fresh on
/// each scheduler tick because the borrow it holds is short-lived.
pub(crate) struct PoolSlotView<'a> {
    pub(crate) slots: &'a HashMap<u32, SlotState>,
}

impl<'a> SlotView for PoolSlotView<'a> {
    fn is_ready(&self, slot: u32) -> bool {
        self.slots
            .get(&slot)
            .is_some_and(|s| s.lifecycle == LifecycleState::Ready)
    }

    fn non_ready_skip_reason(&self, slot: u32) -> Option<String> {
        match self.slots.get(&slot) {
            None => Some("slot not present".to_string()),
            Some(s) if s.lifecycle == LifecycleState::Ready => None,
            Some(s) => Some(s.lifecycle.to_string()),
        }
    }

    fn has_active_call(&self, slot: u32) -> bool {
        self.slots.get(&slot).is_some_and(|s| s.has_active_call)
    }

    fn restart_progress(&self, slot: u32) -> RestartProgress {
        match self.slots.get(&slot) {
            None => RestartProgress::Gone,
            Some(s) => match s.lifecycle {
                LifecycleState::Ready => RestartProgress::Succeeded,
                LifecycleState::GivenUp => RestartProgress::Failed,
                LifecycleState::Initializing | LifecycleState::Recovering => {
                    RestartProgress::InFlight
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_backoff_delay_initial() {
        let d = backoff_delay(0, 5, 120);
        assert_eq!(d, Duration::from_secs(5));
    }

    #[test]
    fn test_backoff_delay_doubles() {
        assert_eq!(backoff_delay(1, 5, 120), Duration::from_secs(10));
        assert_eq!(backoff_delay(2, 5, 120), Duration::from_secs(20));
        assert_eq!(backoff_delay(3, 5, 120), Duration::from_secs(40));
        assert_eq!(backoff_delay(4, 5, 120), Duration::from_secs(80));
    }

    #[test]
    fn test_backoff_delay_caps_at_max() {
        assert_eq!(backoff_delay(5, 5, 120), Duration::from_secs(120));
        assert_eq!(backoff_delay(10, 5, 120), Duration::from_secs(120));
        assert_eq!(backoff_delay(30, 5, 120), Duration::from_secs(120));
    }

    // specs/022-discord-critical-alerts (Greptile P1: "GivenUp state never
    // recovers").

    fn slot_state(module_id: &str, lifecycle: LifecycleState) -> SlotState {
        SlotState {
            slot: 0,
            module: DiscoveredModule {
                id: module_id.to_string(),
                serial_port: PathBuf::from(format!("/dev/tty{module_id}")),
                audio_device: String::new(),
                usb_serial: String::new(),
            },
            imei: String::new(),
            phone_number: String::new(),
            network_type: NetworkType::Unknown,
            network_mode: None,
            lifecycle,
            retry_count: 0,
            next_retry_at: None,
            cmd_tx: None,
            has_active_call: false,
        }
    }

    #[test]
    fn find_given_up_slot_locates_stale_slot_by_module_id() {
        let mut slots = HashMap::new();
        slots.insert(3, slot_state("card0", LifecycleState::GivenUp));

        assert_eq!(find_given_up_slot(&slots, "card0"), Some(3));
    }

    #[test]
    fn find_given_up_slot_ignores_non_given_up_slots_for_the_same_module() {
        let mut slots = HashMap::new();
        slots.insert(3, slot_state("card0", LifecycleState::Ready));

        assert_eq!(
            find_given_up_slot(&slots, "card0"),
            None,
            "a Ready slot is not a stale incident to clear"
        );
    }

    fn slot_state_with_cmd_tx(module_id: &str) -> SlotState {
        let (cmd_tx, _cmd_rx) = crossbeam_channel::unbounded();
        SlotState {
            cmd_tx: Some(cmd_tx),
            ..slot_state(module_id, LifecycleState::Ready)
        }
    }

    /// specs/025-outbound-calling T025 (fifth code review's rewrite of
    /// T024-T026 — the original `ControlCmd::Dial` contention test's premise
    /// no longer exists, that variant was deleted): the whole point of
    /// claiming a slot in the same pass as selecting it is that a *second*
    /// selection against the same map sees the claim immediately, not only
    /// once the dial round trip completes. With a single idle slot, the
    /// first call must succeed and the second must find nothing idle.
    #[test]
    fn claim_idle_cs_slot_does_not_double_book_the_last_idle_slot() {
        let mut slots = HashMap::new();
        slots.insert(0, slot_state_with_cmd_tx("card0"));

        let first = claim_idle_cs_slot(&mut slots);
        assert!(first.is_some(), "the only idle slot must be claimable");
        assert_eq!(first.unwrap().0, 0);
        assert!(
            slots[&0].has_active_call,
            "claiming must mark the slot busy before any dial round trip runs"
        );

        let second = claim_idle_cs_slot(&mut slots);
        assert!(
            second.is_none(),
            "a second request must not double-book the slot the first one just claimed"
        );
    }

    #[test]
    fn claim_idle_cs_slot_picks_among_every_attached_modem() {
        let mut slots = HashMap::new();
        slots.insert(0, slot_state_with_cmd_tx("card0"));
        // Busy: must be skipped even though it comes first in iteration for
        // some hash orderings.
        let mut busy = slot_state_with_cmd_tx("card1");
        busy.has_active_call = true;
        slots.insert(1, busy);
        slots.insert(2, slot_state_with_cmd_tx("card2"));

        let mut claimed = Vec::new();
        while let Some((slot, _, _)) = claim_idle_cs_slot(&mut slots) {
            claimed.push(slot);
        }

        claimed.sort_unstable();
        assert_eq!(
            claimed,
            vec![0, 2],
            "every Ready, non-busy modem must eventually be claimable, and the busy one never"
        );
    }

    #[test]
    fn find_given_up_slot_ignores_a_different_modules_given_up_slot() {
        let mut slots = HashMap::new();
        slots.insert(3, slot_state("card1", LifecycleState::GivenUp));

        assert_eq!(find_given_up_slot(&slots, "card0"), None);
    }
}
