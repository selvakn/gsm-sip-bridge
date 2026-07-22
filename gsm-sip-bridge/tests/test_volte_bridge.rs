//! Lifecycle rules for the host-side cellular bridging service
//! (specs/017-volte-inbound-bridge, US1/US2).
//!
//! These are the rules that decide whether the service survives a night
//! unattended, so they are tested against the real types rather than
//! described in a comment. Nothing here needs a carrier, a modem or a
//! telephone system — which is the point: the hazards below are all things
//! that would otherwise only show up hours into a live soak.

use gsm_sip_bridge::volte::bridge::{
    Admission, BridgedCall, CallSlot, CallStage, EndedBy, Maintenance, MaintenanceDecision,
    MaintenancePolicy,
};

fn incoming(call_id: &str, caller: &str) -> BridgedCall {
    BridgedCall::new(call_id.to_string(), caller.to_string(), None)
}

/// A call that got all the way to bridged, as a live one would be.
fn bridged(call_id: &str, caller: &str) -> BridgedCall {
    let mut c = incoming(call_id, caller);
    assert!(c.advance_to(CallStage::Answering));
    assert!(c.advance_to(CallStage::PbxRinging));
    assert!(c.advance_to(CallStage::Bridged));
    c
}

// ---- one call at a time (US1, FR-006) -------------------------------------

#[test]
fn a_second_call_is_refused_busy_while_one_is_bridged() {
    // The bridge fronts a single subscriber line, so a second call is refused
    // rather than queued. What matters as much as the refusal is that the
    // refusal does not disturb the call already up.
    let mut slot = CallSlot::new();
    assert!(slot.accept(bridged("first@carrier", "+919789063708")));

    assert_eq!(slot.admit(), Admission::RejectBusy);
    assert!(!slot.accept(incoming("second@carrier", "+911111111111")));

    let still_up = slot.active().expect("the first call must survive");
    assert_eq!(still_up.call_id, "first@carrier");
    assert_eq!(still_up.stage, CallStage::Bridged);
    assert_eq!(still_up.ended_by, None, "it must not have been ended");
}

#[test]
fn the_line_accepts_again_once_the_call_is_over() {
    let mut slot = CallSlot::new();
    slot.accept(bridged("first@carrier", "+919789063708"));
    slot.active_mut().unwrap().end(EndedBy::Caller);

    let recorded = slot.take_ended().expect("the ended call is handed back");
    assert!(recorded.reached_bridged());
    assert_eq!(recorded.ended_by, Some(EndedBy::Caller));

    assert_eq!(slot.admit(), Admission::Accept);
    assert!(slot.accept(incoming("next@carrier", "+912222222222")));
}

// ---- call stages (US1, FR-016/FR-017) -------------------------------------

#[test]
fn a_call_cannot_be_bridged_without_the_telephone_system_ringing_first() {
    // Answering the carrier before a human picks up means the caller pays for
    // silence and hears no ringback.
    let mut c = incoming("x@carrier", "+919789063708");
    assert!(c.advance_to(CallStage::Answering));
    assert!(!c.advance_to(CallStage::Bridged));
    assert_eq!(c.stage, CallStage::Answering);
}

#[test]
fn a_one_way_call_is_never_reported_as_a_success() {
    // FR-017, carried forward from feature 016 where the rule caught a real
    // defect. Nobody investigates a success.
    let mut c = bridged("x@carrier", "+919789063708");
    c.end(EndedBy::Caller);

    assert!(c.succeeded(true));
    assert!(!c.succeeded(false));
}

#[test]
fn every_ended_call_names_a_cause() {
    for cause in [
        EndedBy::Caller,
        EndedBy::Pbx,
        EndedBy::AttachmentLost,
        EndedBy::RegistrationLost,
        EndedBy::SetupFailed,
    ] {
        let mut c = incoming("x@carrier", "+911");
        c.end(cause);
        assert_eq!(c.ended_by, Some(cause));
        assert!(!c.ended_by.unwrap().as_str().is_empty());
    }
}

#[test]
fn losing_the_attachment_mid_call_is_not_recorded_as_a_hangup() {
    // The two demand opposite responses: one is normal, the other means the
    // network attachment needs rebuilding. Collapsing them would hide the
    // failure this feature most needs to see (FR-011).
    let mut c = bridged("x@carrier", "+919789063708");
    c.end(EndedBy::AttachmentLost);

    assert_eq!(c.ended_by, Some(EndedBy::AttachmentLost));
    assert_ne!(c.ended_by, Some(EndedBy::Caller));

    // And a later teardown noticing the leg is gone must not overwrite it.
    c.end(EndedBy::Caller);
    assert_eq!(
        c.ended_by,
        Some(EndedBy::AttachmentLost),
        "the first cause is the one that actually happened"
    );
}

// ---- maintenance deferral (US2, FR-009) -----------------------------------

#[test]
fn renewal_falling_due_during_a_call_waits_for_the_call_to_end() {
    // Renewing mid-call tears down the transport the call's own BYE needs.
    let mut slot = CallSlot::new();
    let mut policy = MaintenancePolicy::new();
    slot.accept(bridged("x@carrier", "+919789063708"));

    assert_eq!(
        policy.decide(Maintenance::Renewal, slot.is_busy()),
        MaintenanceDecision::Defer
    );

    slot.active_mut().unwrap().end(EndedBy::Caller);
    slot.take_ended();

    assert_eq!(
        policy.release(),
        Some(Maintenance::Renewal),
        "the held-back work runs as soon as the line is free"
    );
    assert_eq!(
        policy.decide(Maintenance::Renewal, slot.is_busy()),
        MaintenanceDecision::Proceed
    );
}

#[test]
fn reattachment_falling_due_during_a_call_waits_too() {
    // The hazard this feature adds, and the one the existing implementation
    // did not cover. The carrier tears the LTE attachment down roughly every
    // two hours, so an unguarded re-attach is not a rare edge case — it is a
    // dropped call every two hours, indefinitely.
    let mut slot = CallSlot::new();
    let mut policy = MaintenancePolicy::new();
    slot.accept(bridged("x@carrier", "+919789063708"));

    assert_eq!(
        policy.decide(Maintenance::Reattachment, slot.is_busy()),
        MaintenanceDecision::Defer
    );

    slot.active_mut().unwrap().end(EndedBy::Caller);
    slot.take_ended();
    assert_eq!(policy.release(), Some(Maintenance::Reattachment));
}

#[test]
fn the_attachment_is_rebuilt_before_the_registration_when_both_are_pending() {
    // The attachment sits underneath the registration; renewing over a dead
    // one only fails again. Arrival order must not change that.
    for order in [
        [Maintenance::Renewal, Maintenance::Reattachment],
        [Maintenance::Reattachment, Maintenance::Renewal],
    ] {
        let mut policy = MaintenancePolicy::new();
        for what in order {
            policy.decide(what, true);
        }
        assert_eq!(
            policy.release(),
            Some(Maintenance::Reattachment),
            "{order:?}"
        );
    }
}

#[test]
fn a_long_call_is_allowed_to_outlive_its_registration() {
    // Deliberate (spec Assumptions): dropping a live conversation to satisfy
    // a timer is worse than a registration lapsing slightly late.
    let mut policy = MaintenancePolicy::new();
    for _ in 0..500 {
        assert_eq!(
            policy.decide(Maintenance::Renewal, true),
            MaintenanceDecision::Defer
        );
    }
    assert_eq!(policy.deferred(), Some(Maintenance::Renewal));
}

#[test]
fn deferred_maintenance_is_visible_rather_than_silent() {
    // A registration that is deliberately late must not read the same as one
    // that is stuck (FR-013).
    let mut policy = MaintenancePolicy::new();
    assert_eq!(policy.deferred(), None);
    policy.decide(Maintenance::Reattachment, true);
    assert_eq!(policy.deferred(), Some(Maintenance::Reattachment));
    policy.release();
    assert_eq!(policy.deferred(), None);
}

#[test]
fn maintenance_while_the_line_is_idle_is_never_held_back() {
    let mut policy = MaintenancePolicy::new();
    let slot = CallSlot::new();
    for what in [Maintenance::Renewal, Maintenance::Reattachment] {
        assert_eq!(
            policy.decide(what, slot.is_busy()),
            MaintenanceDecision::Proceed
        );
    }
    assert_eq!(policy.deferred(), None);
}

// ---- live status (US3, FR-014/SC-009) -------------------------------------

use gsm_sip_bridge::volte::bridge::ServiceHealth;

fn healthy() -> ServiceHealth {
    ServiceHealth {
        registered: true,
        attached: true,
        busy: false,
        deferred: None,
    }
}

#[test]
fn a_healthy_idle_service_can_answer() {
    assert!(healthy().can_answer());
    assert_eq!(healthy().blocked_reason(), None);
}

#[test]
fn can_answer_is_false_whenever_the_service_could_not_in_fact_answer() {
    // Exclusive card assignment removes the circuit-switched fallback, so a
    // wrong answer here means silently missed calls rather than a merely
    // misleading dashboard (SC-009).
    for (health, what) in [
        (
            ServiceHealth {
                registered: false,
                ..healthy()
            },
            "unregistered",
        ),
        (
            ServiceHealth {
                attached: false,
                ..healthy()
            },
            "unattached",
        ),
        (
            ServiceHealth {
                busy: true,
                ..healthy()
            },
            "busy",
        ),
    ] {
        assert!(!health.can_answer(), "{what} must not report can_answer");
        assert!(
            health.blocked_reason().is_some(),
            "{what} must say why it cannot answer"
        );
    }
}

#[test]
fn being_registered_does_not_by_itself_imply_being_able_to_answer() {
    // The registration is allowed to outlive the attachment briefly, which is
    // precisely the window where inferring one from the other would be wrong.
    let health = ServiceHealth {
        registered: true,
        attached: false,
        busy: false,
        deferred: None,
    };
    assert!(!health.can_answer());
    assert_eq!(
        health.blocked_reason(),
        Some("the network attachment is down"),
        "the attachment is named, since that is what has to be fixed"
    );
}

#[test]
fn deferred_maintenance_does_not_stop_the_service_answering() {
    // Deferral means a call is up, not that the service is unhealthy — the
    // busy flag is what makes it unable to take a *second* call.
    let health = ServiceHealth {
        busy: false,
        deferred: Some(Maintenance::Reattachment),
        ..healthy()
    };
    assert!(health.can_answer());
}

// ---- the two halves must agree on where to meet --------------------------

#[test]
fn the_halves_agree_on_the_leg_port() {
    // Found live: the telephone-side half dialled LOOPBACK_SIP_PORT while the
    // carrier-side listener still bound the Wi-Fi path's hardcoded
    // VETH_SIP_PORT. The symptom was not an obvious failure — the INVITE
    // arrived, both legs were placed, the PBX rang and a human ANSWERED, and
    // only then did it time out, leaving the caller listening to ringback for
    // a call that was never going to connect.
    //
    // Both sides now read this one constant. This test exists so that stays
    // true.
    use gsm_sip_bridge::volte::bridge::{LOOPBACK_CONTROL_PORT, LOOPBACK_SIP_PORT, SIP_LOCAL_PORT};

    assert_ne!(
        LOOPBACK_SIP_PORT, SIP_LOCAL_PORT,
        "the leg port and the PJSIP local port must not collide"
    );
    assert_ne!(
        LOOPBACK_SIP_PORT, LOOPBACK_CONTROL_PORT,
        "SIP signalling and the control channel need separate ports"
    );
}
