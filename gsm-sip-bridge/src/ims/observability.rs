//! Agent A's (`ims::agent`) side of restoring call observability under
//! VoWiFi (specs/014-vowifi-metrics-restore). One `AgentObservability`
//! instance lives for the process's lifetime, wrapping the
//! `observability::reporter::Reporter` that carries events to the daemon
//! and (best-effort) the `StoreHandle` that writes call history rows
//! directly, independent of whether the daemon is reachable.
//!
//! All gauge state (`active_calls`, `registered`, `tunnel_up`) is cached
//! here and re-sent in full on every report, since `AgentState` is
//! "absolute, latest-wins" on the wire (contracts/observability-protocol.md)
//! — a caller only ever needs to say what changed, not reconstruct the rest.

use crate::control::protocol::{
    AgentState, BridgeFailureReason, CallStatus, ObservedEvent, RegistrationStatus,
};
use crate::observability::reporter::Reporter;
use crate::store::calls::CallRecord;
use crate::store::{StoreCommand, StoreHandle, Transport};
use crate::vowifi::control::reason;
use chrono::{DateTime, Utc};
use std::sync::Mutex;

pub struct AgentObservability {
    reporter: Reporter,
    module_id: String,
    /// `None` when the store could not be opened — reporting must never be
    /// able to take an agent down (FR-018), so a missing store degrades to
    /// "no history for this run" rather than a startup failure.
    store: Option<StoreHandle>,
    /// Reused for every call record (`[bridge].sip_destination`) — the same
    /// destination Agent B dials, since there is exactly one PBX destination
    /// for the whole bridge.
    sip_destination: String,
    /// Which transport this agent's call rows are filed under. Both the VoWiFi
    /// and VoLTE paths run this same code, so it must be told which it is or
    /// their history collapses into one — the store counterpart of the metric
    /// `transport` label.
    transport: Transport,
    state: Mutex<AgentState>,
}

impl AgentObservability {
    pub fn new(
        reporter: Reporter,
        module_id: String,
        store: Option<StoreHandle>,
        sip_destination: String,
        transport: Transport,
    ) -> Self {
        Self {
            reporter,
            module_id,
            store,
            sip_destination,
            transport,
            state: Mutex::new(AgentState::default()),
        }
    }

    fn push(&self, events: Vec<ObservedEvent>) {
        let state = *self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.reporter.report(state, events);
    }

    pub fn set_active_calls(&self, n: u32) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active_calls = Some(n);
        self.push(Vec::new());
    }

    pub fn set_registered(&self, registered: bool) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .registered = Some(registered);
        self.push(Vec::new());
    }

    pub fn set_tunnel_up(&self, up: bool) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tunnel_up = Some(up);
        self.push(Vec::new());
    }

    pub fn report_registration_attempt(&self, status: RegistrationStatus) {
        self.push(vec![ObservedEvent::RegistrationAttempt { status }]);
    }

    /// A call that never reached an answer: declined outright, the PBX
    /// leg failed to establish, the extension rang out, or the caller gave
    /// up while it rang. `status` is `Missed` when the PBX extension was
    /// actually reached and rang (a human could have picked up), `Failed`
    /// when the call never got that far (no usable codec, Agent B
    /// unreachable, an internal error).
    pub fn report_call_not_answered(
        &self,
        status: CallStatus,
        reason: BridgeFailureReason,
        caller: &str,
        started_at: DateTime<Utc>,
    ) {
        self.push(vec![
            ObservedEvent::CallCompleted {
                status,
                duration_seconds: 0.0,
            },
            ObservedEvent::BridgeFailed { reason },
        ]);
        self.insert_call_row(caller, started_at, 0.0, status);
    }

    /// Reports a call that reached an answer and then ended, judged by its
    /// media (FR-017): a call that carried audio both ways is `Answered`; one
    /// that carried one-way or no audio is a **failure**, recorded as `Failed`
    /// with the direction named, never as a success.
    pub fn report_call_answered_and_ended(
        &self,
        caller: &str,
        started_at: DateTime<Utc>,
        duration_seconds: f64,
        media: super::media_stats::DirectionVerdict,
    ) {
        if media.is_success() {
            self.push(vec![ObservedEvent::CallCompleted {
                status: CallStatus::Answered,
                duration_seconds,
            }]);
            self.insert_call_row(caller, started_at, duration_seconds, CallStatus::Answered);
        } else {
            self.push(vec![
                ObservedEvent::CallCompleted {
                    status: CallStatus::Failed,
                    duration_seconds,
                },
                ObservedEvent::BridgeFailed {
                    reason: BridgeFailureReason::OneWayAudio,
                },
            ]);
            self.insert_call_row(caller, started_at, duration_seconds, CallStatus::Failed);
        }
    }

    fn insert_call_row(
        &self,
        caller: &str,
        started_at: DateTime<Utc>,
        duration_seconds: f64,
        status: CallStatus,
    ) {
        let Some(store) = &self.store else {
            return;
        };
        let record = CallRecord {
            module_id: self.module_id.clone(),
            caller_id: caller.to_string(),
            started_at: started_at.to_rfc3339(),
            duration_seconds,
            status: status.as_str().to_string(),
            sip_destination: self.sip_destination.clone(),
            transport: self.transport,
        };
        if let Err(e) = store.sender().send(StoreCommand::InsertCall(record)) {
            tracing::error!(error = %e, "failed to send call record to store");
        }
    }
}

/// Maps the free-form reason strings carried by `vowifi::control`'s
/// `ControlMessage::BridgeFailed`/`CallEnded` (`vowifi::control::reason`)
/// onto the closed `BridgeFailureReason` set (FR-014) — new carrier-facing
/// failure text must never be able to mint a new metric label; anything
/// unrecognised collapses to `BridgeSetupFailed`.
pub fn map_bridge_failure_reason(raw: &str) -> BridgeFailureReason {
    match raw {
        reason::PBX_UNREACHABLE | reason::PBX_REJECTED => BridgeFailureReason::PbxDeclined,
        reason::PBX_NO_ANSWER => BridgeFailureReason::RingTimeout,
        reason::CALLER_CANCELLED => BridgeFailureReason::CallerCancelled,
        reason::VETH_LEG_FAILED => BridgeFailureReason::BridgeSetupFailed,
        reason::TRANSPORT_ERROR => BridgeFailureReason::AgentUnreachable,
        _ => BridgeFailureReason::BridgeSetupFailed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_bridge_failure_reason_covers_every_known_reason_constant() {
        assert_eq!(
            map_bridge_failure_reason(reason::PBX_UNREACHABLE),
            BridgeFailureReason::PbxDeclined
        );
        assert_eq!(
            map_bridge_failure_reason(reason::PBX_REJECTED),
            BridgeFailureReason::PbxDeclined
        );
        assert_eq!(
            map_bridge_failure_reason(reason::PBX_NO_ANSWER),
            BridgeFailureReason::RingTimeout
        );
        assert_eq!(
            map_bridge_failure_reason(reason::CALLER_CANCELLED),
            BridgeFailureReason::CallerCancelled
        );
        assert_eq!(
            map_bridge_failure_reason(reason::VETH_LEG_FAILED),
            BridgeFailureReason::BridgeSetupFailed
        );
        assert_eq!(
            map_bridge_failure_reason(reason::TRANSPORT_ERROR),
            BridgeFailureReason::AgentUnreachable
        );
    }

    #[test]
    fn map_bridge_failure_reason_falls_back_for_unknown_strings() {
        assert_eq!(
            map_bridge_failure_reason("some_future_reason_never_seen_before"),
            BridgeFailureReason::BridgeSetupFailed
        );
    }
}
