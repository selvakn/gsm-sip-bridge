//! Transport-agnostic inbound-call lifecycle: stage machine, end-cause
//! attribution, the single-call-slot admission rule, maintenance deferral, and
//! the derived service-health view.
//!
//! Extracted from `volte::bridge` (specs/017) so **one** model serves both the
//! Wi-Fi and cellular paths through the shared `ims::agent` dispatch loop — the
//! model was already transport-neutral; it just lived in the wrong module and
//! was exercised only by tests. `ims::agent` now drives it at runtime, and
//! reporting (`control::protocol::BridgeFailureReason`/`CallStatus` and the
//! `vowifi::control::reason` strings) is *derived* from it rather than passed
//! as ad-hoc literals at each teardown site.

use crate::control::protocol::CallStatus;
use crate::vowifi::control::reason;
use std::time::{Duration, SystemTime};

/// How far a call got. The basis of FR-016's outcome reporting.
///
/// ```text
/// Offered ──accept──> Answering ──> PbxRinging ──answers──> Bridged
///    │                    │              │                     │
///    └── busy ────────────┴──────────────┴─────────────────────┴──> Ended
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallStage {
    /// The network has offered the call; we have not yet accepted it.
    Offered,
    /// Accepted, placing the telephone-system leg.
    Answering,
    /// The telephone system is ringing; the caller hears real ringback.
    PbxRinging,
    /// Both legs up and relaying. **The only stage that can succeed.**
    Bridged,
    /// Over, for whatever reason.
    Ended,
}

impl CallStage {
    /// Whether this stage may legally be followed by `next`.
    ///
    /// Encoded rather than left implicit because a call that skips
    /// `PbxRinging` straight to `Bridged` would mean we answered the carrier
    /// before a human picked up — which is what makes a caller pay for
    /// silence.
    pub fn can_advance_to(self, next: CallStage) -> bool {
        matches!(
            (self, next),
            (CallStage::Offered, CallStage::Answering)
                | (CallStage::Answering, CallStage::PbxRinging)
                | (CallStage::PbxRinging, CallStage::Bridged)
        ) || next == CallStage::Ended && self != CallStage::Ended
    }

    /// Whether a call ending at this stage was a success.
    ///
    /// Only `Bridged` qualifies, and even then the media verdict can still
    /// demote it — a call carrying audio one way only is a failure (FR-017),
    /// a rule carried forward from feature 016 where it caught a real defect.
    pub fn is_success(self) -> bool {
        self == CallStage::Bridged
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CallStage::Offered => "offered",
            CallStage::Answering => "answering",
            CallStage::PbxRinging => "pbx_ringing",
            CallStage::Bridged => "bridged",
            CallStage::Ended => "ended",
        }
    }
}

/// Who or what ended a call.
///
/// Always set. "The call ended" without a reason is what makes an operator
/// re-run a failure to learn anything (FR-004, FR-011).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndedBy {
    /// The caller hung up.
    Caller,
    /// The telephone system hung up, rejected, or never answered.
    Pbx,
    /// The network attachment was genuinely lost mid-call — **distinct from
    /// the caller hanging up**, because the two demand opposite responses
    /// (FR-011).
    AttachmentLost,
    /// The registration lapsed and could not be recovered.
    RegistrationLost,
    /// We could not set the bridge up at all.
    SetupFailed,
}

impl EndedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            EndedBy::Caller => "caller_hangup",
            EndedBy::Pbx => "pbx_hangup",
            EndedBy::AttachmentLost => "attachment_lost",
            EndedBy::RegistrationLost => "registration_lost",
            EndedBy::SetupFailed => "bridge_setup_failed",
        }
    }

    /// The control-channel reason string to send the telephone-side half when
    /// *this* side originates the teardown. Lets the dispatch loop derive the
    /// wire string from the lifecycle cause instead of naming a
    /// [`reason`] constant by hand at each site — the strings coincide with
    /// [`Self::as_str`] for the causes both vocabularies share (asserted in the
    /// tests), which is exactly why the model can own the attribution.
    pub fn control_reason(self) -> &'static str {
        match self {
            EndedBy::Caller => reason::CALLER_HANGUP,
            EndedBy::Pbx => reason::PBX_HANGUP,
            EndedBy::AttachmentLost => reason::ATTACHMENT_LOST,
            EndedBy::RegistrationLost => reason::TRANSPORT_ERROR,
            EndedBy::SetupFailed => reason::VETH_LEG_FAILED,
        }
    }
}

/// One inbound call and its two legs.
#[derive(Debug, Clone)]
pub struct BridgedCall {
    /// Correlates both legs and the history record.
    pub call_id: String,
    /// E.164 number of the caller, as supplied by the network.
    pub caller: String,
    /// Display name, when the network supplied one — it does (research R1).
    pub caller_name: Option<String>,
    pub stage: CallStage,
    pub ended_by: Option<EndedBy>,
    pub started_at: SystemTime,
    /// High-water mark: whether the call ever reached [`CallStage::Bridged`].
    /// Needed because a *successful* call ends at `Ended` like any other, so
    /// the current stage cannot distinguish "bridged then hung up normally"
    /// from "never got off the ground".
    reached_bridged: bool,
}

impl BridgedCall {
    pub fn new(call_id: String, caller: String, caller_name: Option<String>) -> Self {
        Self {
            call_id,
            caller,
            caller_name,
            stage: CallStage::Offered,
            ended_by: None,
            started_at: SystemTime::now(),
            reached_bridged: false,
        }
    }

    /// Whether the call ever reached [`CallStage::Bridged`].
    pub fn reached_bridged(&self) -> bool {
        self.reached_bridged
    }

    /// Advances the stage, refusing an illegal transition rather than
    /// recording a call state that cannot have happened.
    pub fn advance_to(&mut self, next: CallStage) -> bool {
        if self.stage.can_advance_to(next) {
            self.stage = next;
            self.reached_bridged |= next == CallStage::Bridged;
            true
        } else {
            false
        }
    }

    /// Ends the call, attributing it. Idempotent in the attribution: the
    /// *first* cause wins, because that is the one that actually happened —
    /// a later teardown observing "the leg is gone" must not overwrite it.
    pub fn end(&mut self, by: EndedBy) {
        if self.ended_by.is_none() {
            self.ended_by = Some(by);
        }
        self.stage = CallStage::Ended;
    }

    /// Whether this call should be recorded as a success.
    ///
    /// Two conditions, both required. The call must have reached `Bridged` —
    /// and `media_both_ways`, from the existing `MediaReport` verdict, must
    /// hold: a bridged call that carried audio in one direction only is a
    /// **failure** (FR-017), not a success with a caveat. Carried forward from
    /// feature 016, where exactly this rule caught a real defect.
    ///
    /// Note this reads the *high-water mark*, not the current stage, since a
    /// successful call ends at `Ended` like any other.
    pub fn succeeded(&self, media_both_ways: bool) -> bool {
        self.reached_bridged && media_both_ways
    }

    /// The top-level [`CallStatus`] to record for this call, derived from the
    /// same high-water mark and media verdict [`Self::succeeded`] uses.
    ///
    /// Deliberately coarse — it answers only "answered, missed, or failed". The
    /// finer [`BridgeFailureReason`] a not-answered call also carries is
    /// resolved at the teardown site that has the context to tell a ring
    /// timeout from a decline (`control::protocol::BridgeFailureReason`); the
    /// model does not try to, because [`EndedBy`] cannot distinguish them
    /// without losing fidelity.
    ///
    /// - reached `Bridged` with both-ways audio → [`CallStatus::Answered`]
    /// - reached `Bridged` but one-way audio → [`CallStatus::Failed`]
    /// - never bridged (declined, no answer, cancelled) → [`CallStatus::Missed`]
    pub fn call_status(&self, media_both_ways: bool) -> CallStatus {
        if self.succeeded(media_both_ways) {
            CallStatus::Answered
        } else if self.reached_bridged {
            CallStatus::Failed
        } else {
            CallStatus::Missed
        }
    }
}

/// Arbitrates the single call slot.
///
/// The bridge fronts one subscriber line, so a second concurrent call is
/// **refused as busy rather than queued** (FR-006) — and refusing it must not
/// disturb the call already up, which is the part worth testing.
#[derive(Debug, Default)]
pub struct CallSlot {
    active: Option<BridgedCall>,
}

/// What to do with a newly-offered call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Take it.
    Accept,
    /// Refuse it as busy. The call in progress is untouched.
    RejectBusy,
}

impl Admission {
    /// The single admission rule, shared by [`CallSlot`] (the pure model, for
    /// tests) and the `ims::agent` dispatch loop (which keeps its live call in
    /// its own runtime struct, not a `CallSlot`, so it applies the rule to the
    /// lifecycle it holds). One definition means the two cannot drift.
    ///
    /// Any call not yet `Ended` occupies the slot — including one still
    /// `Offered` or `PbxRinging`, since those are calls a human is already
    /// being asked to answer.
    pub fn for_current(current: Option<&BridgedCall>) -> Admission {
        match current {
            Some(call) if call.stage != CallStage::Ended => Admission::RejectBusy,
            _ => Admission::Accept,
        }
    }
}

impl CallSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decides whether an offered call can be taken, via the shared
    /// [`Admission::for_current`] rule.
    pub fn admit(&self) -> Admission {
        Admission::for_current(self.active.as_ref())
    }

    /// Places a call in the slot. Returns `false` — leaving the existing call
    /// **untouched** — if the slot is occupied.
    pub fn accept(&mut self, call: BridgedCall) -> bool {
        if self.admit() == Admission::RejectBusy {
            return false;
        }
        self.active = Some(call);
        true
    }

    pub fn active(&self) -> Option<&BridgedCall> {
        self.active.as_ref().filter(|c| c.stage != CallStage::Ended)
    }

    pub fn active_mut(&mut self) -> Option<&mut BridgedCall> {
        self.active.as_mut().filter(|c| c.stage != CallStage::Ended)
    }

    /// Whether a call is in progress — the input to [`MaintenancePolicy`].
    pub fn is_busy(&self) -> bool {
        self.active().is_some()
    }

    /// Removes the finished call, returning it for recording.
    pub fn take_ended(&mut self) -> Option<BridgedCall> {
        match &self.active {
            Some(c) if c.stage == CallStage::Ended => self.active.take(),
            _ => None,
        }
    }
}

/// Maintenance work that must yield to a call in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Maintenance {
    /// Renew the registration before it expires.
    Renewal,
    /// Re-establish the network attachment the carrier tore down.
    Reattachment,
}

impl Maintenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Maintenance::Renewal => "renewal",
            Maintenance::Reattachment => "reattachment",
        }
    }
}

/// What to do with maintenance that has fallen due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceDecision {
    /// Do it now.
    Proceed,
    /// A call is in progress. Hold it until the call ends.
    Defer,
}

/// Decides whether due maintenance may run.
///
/// # Why re-attachment is the dangerous one
///
/// Renewal deferral is inherited from the Wi-Fi agent, with the reasoning
/// recorded at that site: renewing mid-call tears down the transport the
/// call's own `BYE` still needs.
///
/// **Re-attachment is worse and is new to this path.** Renewal rebuilds the
/// signalling; re-attachment rebuilds the *network attachment underneath it*,
/// taking the media with it. The carrier tears that attachment down roughly
/// every two hours, so an unguarded re-attach is not a rare edge case — it is
/// a call dropped every two hours, indefinitely.
///
/// A call is deliberately allowed to **outlive its registration** rather than
/// be cut short (spec Assumptions): dropping a live conversation to satisfy a
/// timer is worse than a registration lapsing slightly late.
#[derive(Debug, Default)]
pub struct MaintenancePolicy {
    deferred: Option<Maintenance>,
    deferred_since: Option<SystemTime>,
}

impl MaintenancePolicy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decides whether `what` may run now.
    ///
    /// Takes `call_in_progress` rather than reading a slot so the policy is
    /// testable without a carrier, a modem or a telephone system.
    pub fn decide(&mut self, what: Maintenance, call_in_progress: bool) -> MaintenanceDecision {
        if call_in_progress {
            if self.deferred.is_none() {
                self.deferred_since = Some(SystemTime::now());
            }
            // Re-attachment subsumes renewal: the attachment is underneath the
            // registration, so if both fell due we must rebuild the lower
            // layer first or the renewal would only fail again.
            self.deferred = Some(match (self.deferred, what) {
                (Some(Maintenance::Reattachment), _) | (_, Maintenance::Reattachment) => {
                    Maintenance::Reattachment
                }
                _ => Maintenance::Renewal,
            });
            MaintenanceDecision::Defer
        } else {
            MaintenanceDecision::Proceed
        }
    }

    /// Whether maintenance is currently being held back — reported in status
    /// so a deferred renewal reads as deliberate rather than as a stall.
    pub fn deferred(&self) -> Option<Maintenance> {
        self.deferred
    }

    /// How long maintenance has been held back, if it has.
    pub fn deferred_for(&self, now: SystemTime) -> Option<Duration> {
        self.deferred_since.and_then(|t| now.duration_since(t).ok())
    }

    /// Called when a call ends: returns the work that was held back, so the
    /// caller runs it immediately rather than waiting out another poll
    /// interval on a registration that may already have lapsed.
    pub fn release(&mut self) -> Option<Maintenance> {
        self.deferred_since = None;
        self.deferred.take()
    }
}

/// What a live status query answers (FR-014, FR-033).
///
/// Assembled from parts that already exist rather than tracked separately,
/// so it cannot disagree with the thing it describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceHealth {
    /// Whether the registration is currently accepted.
    pub registered: bool,
    /// Whether the network attachment is up **and routable** — attached but
    /// unrouted is the failure mode specs/015 R15 spent two hours proving is
    /// real, so "attached" alone is not enough.
    pub attached: bool,
    /// Whether the telephone-side half holds the PBX registration the outbound
    /// bridge leg needs. A bridged call needs *both* legs, so a call cannot be
    /// answered when this is false however healthy the carrier side is.
    pub pbx_registered: bool,
    /// Whether a call is in progress.
    pub busy: bool,
    /// Maintenance currently being held back for a call, if any.
    pub deferred: Option<Maintenance>,
}

impl ServiceHealth {
    /// Whether an incoming call could actually be answered right now.
    ///
    /// # Why this must never be optimistic
    ///
    /// Card assignment is exclusive (FR-034): a card on this path has **no
    /// circuit-switched fallback**, so when the path is down that card takes
    /// no calls at all. A `can_answer` that says yes when the answer is no
    /// does not merely mislead a dashboard — it means calls are being missed
    /// and nothing is reporting it (SC-009).
    ///
    /// So every condition must hold, and each is checked independently rather
    /// than inferred from another. In particular `registered` does not imply
    /// `attached`: the registration is allowed to outlive the attachment
    /// briefly, which is exactly when an optimistic answer would be wrong.
    pub fn can_answer(&self) -> bool {
        self.registered && self.attached && self.pbx_registered && !self.busy
    }

    /// Why the service cannot answer, for an operator who needs to fix it
    /// rather than merely observe it. `None` when it can.
    pub fn blocked_reason(&self) -> Option<&'static str> {
        if !self.attached {
            Some("the network attachment is down")
        } else if !self.registered {
            Some("not registered")
        } else if !self.pbx_registered {
            Some("the PBX registration is down")
        } else if self.busy {
            Some("a call is already in progress")
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call() -> BridgedCall {
        BridgedCall::new(
            "abc@carrier".to_string(),
            "+919789063708".to_string(),
            Some("Selvakumar Natesan".to_string()),
        )
    }

    // ---- call stages ------------------------------------------------------

    #[test]
    fn a_call_walks_the_expected_stages() {
        let mut c = call();
        assert_eq!(c.stage, CallStage::Offered);
        assert!(c.advance_to(CallStage::Answering));
        assert!(c.advance_to(CallStage::PbxRinging));
        assert!(c.advance_to(CallStage::Bridged));
        assert!(c.stage.is_success());
    }

    #[test]
    fn a_call_cannot_reach_bridged_without_the_pbx_ringing() {
        // Skipping PbxRinging would mean answering the carrier before a human
        // picked up — the caller then pays for silence.
        let mut c = call();
        c.advance_to(CallStage::Answering);
        assert!(!c.advance_to(CallStage::Bridged));
        assert_eq!(c.stage, CallStage::Answering);
    }

    #[test]
    fn only_bridged_counts_as_success() {
        for stage in [
            CallStage::Offered,
            CallStage::Answering,
            CallStage::PbxRinging,
            CallStage::Ended,
        ] {
            assert!(!stage.is_success(), "{stage:?} must not be a success");
        }
        assert!(CallStage::Bridged.is_success());
    }

    #[test]
    fn a_bridged_call_carrying_audio_one_way_is_not_a_success() {
        // FR-017, carried forward from feature 016 where this rule caught a
        // real defect. Reporting a one-way call as successful is worse than
        // reporting a failure, because nobody investigates a success.
        let mut c = call();
        c.advance_to(CallStage::Answering);
        c.advance_to(CallStage::PbxRinging);
        c.advance_to(CallStage::Bridged);
        c.end(EndedBy::Caller);

        assert!(c.succeeded(true));
        assert!(
            !c.succeeded(false),
            "one-way audio must never be reported as a successful call"
        );
    }

    #[test]
    fn a_call_that_never_bridged_is_not_a_success_even_with_good_media() {
        let mut c = call();
        c.advance_to(CallStage::Answering);
        c.end(EndedBy::Pbx);
        assert!(!c.succeeded(true));
    }

    #[test]
    fn ending_normally_does_not_erase_that_the_call_was_bridged() {
        // A successful call ends at `Ended` like any other, so the current
        // stage alone cannot tell success from a call that never connected.
        let mut c = call();
        c.advance_to(CallStage::Answering);
        c.advance_to(CallStage::PbxRinging);
        c.advance_to(CallStage::Bridged);
        c.end(EndedBy::Caller);

        assert_eq!(c.stage, CallStage::Ended);
        assert!(c.reached_bridged());
    }

    #[test]
    fn a_call_can_end_from_any_stage() {
        for stage in [
            CallStage::Offered,
            CallStage::Answering,
            CallStage::PbxRinging,
            CallStage::Bridged,
        ] {
            assert!(stage.can_advance_to(CallStage::Ended));
        }
    }

    #[test]
    fn the_first_cause_of_ending_wins() {
        // A teardown that later notices "the leg is gone" must not overwrite
        // the attachment loss that actually caused it.
        let mut c = call();
        c.end(EndedBy::AttachmentLost);
        c.end(EndedBy::Caller);
        assert_eq!(c.ended_by, Some(EndedBy::AttachmentLost));
    }

    #[test]
    fn attachment_loss_is_distinguishable_from_a_hangup() {
        // The two demand opposite responses, so they must never collapse.
        assert_ne!(EndedBy::AttachmentLost.as_str(), EndedBy::Caller.as_str());
        assert_eq!(EndedBy::AttachmentLost.as_str(), "attachment_lost");
    }

    #[test]
    fn the_control_reason_matches_the_end_cause_string_where_they_overlap() {
        // The whole basis for the model owning the attribution: the wire
        // reason the dispatch loop sends the telephone-side half is the same
        // string `EndedBy::as_str` records, for every cause both carry. If
        // these ever diverged, a call's history row and the control-channel
        // teardown would disagree about why it ended.
        for e in [EndedBy::Caller, EndedBy::Pbx, EndedBy::AttachmentLost] {
            assert_eq!(e.control_reason(), e.as_str(), "{e:?}");
        }
        // And the control reason is always one the reason vocabulary defines.
        assert_eq!(
            EndedBy::AttachmentLost.control_reason(),
            reason::ATTACHMENT_LOST
        );
    }

    #[test]
    fn call_status_follows_bridged_and_media() {
        // Never bridged -> Missed, whatever the media verdict claims.
        let mut c = call();
        c.advance_to(CallStage::Answering);
        c.end(EndedBy::Pbx);
        assert_eq!(c.call_status(true), CallStatus::Missed);

        // Bridged with both-ways audio -> Answered.
        let mut ok = call();
        ok.advance_to(CallStage::Answering);
        ok.advance_to(CallStage::PbxRinging);
        ok.advance_to(CallStage::Bridged);
        ok.end(EndedBy::Caller);
        assert_eq!(ok.call_status(true), CallStatus::Answered);

        // Bridged but one-way audio -> Failed, not a success with a caveat.
        assert_eq!(ok.call_status(false), CallStatus::Failed);
    }

    // ---- the single call slot ---------------------------------------------

    #[test]
    fn a_second_call_is_rejected_busy_and_the_first_is_undisturbed() {
        let mut slot = CallSlot::new();
        let mut first = call();
        first.advance_to(CallStage::Answering);
        first.advance_to(CallStage::PbxRinging);
        first.advance_to(CallStage::Bridged);
        assert!(slot.accept(first));

        assert_eq!(slot.admit(), Admission::RejectBusy);
        let second = BridgedCall::new("second@carrier".into(), "+911111111111".into(), None);
        assert!(
            !slot.accept(second),
            "the second call must not take the slot"
        );

        let active = slot.active().expect("the first call must still be there");
        assert_eq!(active.call_id, "abc@carrier");
        assert_eq!(active.stage, CallStage::Bridged);
    }

    #[test]
    fn a_still_ringing_call_also_occupies_the_slot() {
        // A human is already being asked to answer it.
        let mut slot = CallSlot::new();
        let mut c = call();
        c.advance_to(CallStage::Answering);
        slot.accept(c);
        assert_eq!(slot.admit(), Admission::RejectBusy);
    }

    #[test]
    fn the_slot_frees_once_the_call_ends() {
        let mut slot = CallSlot::new();
        slot.accept(call());
        slot.active_mut().unwrap().end(EndedBy::Caller);

        assert!(!slot.is_busy());
        assert_eq!(slot.admit(), Admission::Accept);
        let ended = slot
            .take_ended()
            .expect("the ended call is returned for recording");
        assert_eq!(ended.ended_by, Some(EndedBy::Caller));
        assert!(slot.accept(BridgedCall::new("next".into(), "+912".into(), None)));
    }

    // ---- maintenance deferral ---------------------------------------------

    #[test]
    fn renewal_due_while_idle_proceeds() {
        let mut p = MaintenancePolicy::new();
        assert_eq!(
            p.decide(Maintenance::Renewal, false),
            MaintenanceDecision::Proceed
        );
        assert_eq!(p.deferred(), None);
    }

    #[test]
    fn renewal_due_during_a_call_is_deferred_until_it_ends() {
        let mut p = MaintenancePolicy::new();
        assert_eq!(
            p.decide(Maintenance::Renewal, true),
            MaintenanceDecision::Defer,
            "renewing mid-call tears down the transport the call's own BYE needs"
        );
        assert_eq!(p.release(), Some(Maintenance::Renewal));
        assert_eq!(p.deferred(), None, "released work is not deferred twice");
    }

    #[test]
    fn reattachment_due_during_a_call_is_deferred() {
        // The hazard this feature adds. Unguarded this drops a call roughly
        // every two hours, because that is how often the carrier tears the
        // attachment down.
        let mut p = MaintenancePolicy::new();
        assert_eq!(
            p.decide(Maintenance::Reattachment, true),
            MaintenanceDecision::Defer
        );
        assert_eq!(p.release(), Some(Maintenance::Reattachment));
    }

    #[test]
    fn reattachment_outranks_renewal_when_both_were_deferred() {
        // The attachment is underneath the registration: renewing first would
        // only fail again.
        let mut p = MaintenancePolicy::new();
        p.decide(Maintenance::Renewal, true);
        p.decide(Maintenance::Reattachment, true);
        assert_eq!(p.release(), Some(Maintenance::Reattachment));

        let mut p = MaintenancePolicy::new();
        p.decide(Maintenance::Reattachment, true);
        p.decide(Maintenance::Renewal, true);
        assert_eq!(
            p.release(),
            Some(Maintenance::Reattachment),
            "order of arrival must not change which layer is rebuilt first"
        );
    }

    #[test]
    fn a_call_may_outlive_its_registration() {
        // Deliberate: dropping a live conversation to satisfy a timer is worse
        // than a registration lapsing slightly late.
        let mut p = MaintenancePolicy::new();
        for _ in 0..100 {
            assert_eq!(
                p.decide(Maintenance::Renewal, true),
                MaintenanceDecision::Defer
            );
        }
        assert!(p.deferred().is_some());
    }

    #[test]
    fn deferral_is_visible_so_it_reads_as_deliberate_not_as_a_stall() {
        let mut p = MaintenancePolicy::new();
        let before = SystemTime::now();
        p.decide(Maintenance::Renewal, true);
        assert_eq!(p.deferred(), Some(Maintenance::Renewal));
        assert!(p.deferred_for(before + Duration::from_secs(30)).is_some());
    }

    #[test]
    fn maintenance_resumes_after_the_call_ends() {
        let mut p = MaintenancePolicy::new();
        p.decide(Maintenance::Renewal, true);
        p.release();
        assert_eq!(
            p.decide(Maintenance::Renewal, false),
            MaintenanceDecision::Proceed
        );
    }
}
