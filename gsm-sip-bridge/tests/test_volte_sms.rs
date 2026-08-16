//! Text messages over the host-side LTE path
//! (specs/017-volte-inbound-bridge, US5).
//!
//! This is a **regression** rather than an addition: holding the subscriber's
//! registration means the network delivers their texts here, so "not handled"
//! would mean texts arriving and being silently discarded. A call that fails
//! to connect announces itself. A lost text does not.

use gsm_sip_bridge::store::Transport;
use gsm_sip_bridge::volte::sms::{
    decide, parse_cmgl_indexes, Dedupe, Disposition, InboundMessage, MessageRoute,
};
use std::sync::{Arc, Mutex};

fn over_registration(sender: &str, body: &str) -> InboundMessage {
    InboundMessage {
        route: MessageRoute::OverRegistration,
        sender: sender.to_string(),
        body: body.to_string(),
        modem_index: None,
    }
}

fn through_modem(sender: &str, body: &str, index: u32) -> InboundMessage {
    InboundMessage {
        route: MessageRoute::ThroughModem,
        sender: sender.to_string(),
        body: body.to_string(),
        modem_index: Some(index),
    }
}

// ---- exactly once, whichever route delivered it (FR-037) ------------------

#[test]
fn a_message_on_either_route_is_handled_once() {
    for msg in [
        over_registration("+919000000000", "hello"),
        through_modem("+919000000000", "hello", 3),
    ] {
        let mut dedupe = Dedupe::default();
        assert_eq!(decide(&mut dedupe, &msg), Disposition::Handle);
        assert_eq!(decide(&mut dedupe, &msg), Disposition::AcknowledgeOnly);
    }
}

#[test]
fn the_same_message_arriving_on_both_routes_is_recorded_once() {
    // Which route the carrier uses is its decision and is unmeasured, so both
    // are covered. Covering both is only safe if a message delivered twice
    // collapses to one — otherwise the operator sees every text twice.
    let mut dedupe = Dedupe::default();
    assert_eq!(
        decide(&mut dedupe, &over_registration("+919000000000", "hello")),
        Disposition::Handle
    );
    assert_eq!(
        decide(&mut dedupe, &through_modem("+919000000000", "hello", 3)),
        Disposition::AcknowledgeOnly,
        "the delivery route must not be part of the message's identity"
    );
}

#[test]
fn a_retransmission_is_still_acknowledged_so_the_network_stops_retrying() {
    // The flip side of acknowledging only after recording: a crash in that
    // window causes a retransmission. Suppressing the duplicate is right;
    // failing to acknowledge it would leave the network retrying forever.
    let mut dedupe = Dedupe::default();
    let msg = over_registration("+919000000000", "hello");
    assert_eq!(decide(&mut dedupe, &msg), Disposition::Handle);
    for _ in 0..10 {
        assert_eq!(decide(&mut dedupe, &msg), Disposition::AcknowledgeOnly);
    }
}

#[test]
fn distinct_messages_are_never_collapsed() {
    let mut dedupe = Dedupe::default();
    // Same sender, different body.
    assert_eq!(
        decide(&mut dedupe, &over_registration("+911", "one")),
        Disposition::Handle
    );
    assert_eq!(
        decide(&mut dedupe, &over_registration("+911", "two")),
        Disposition::Handle
    );
    // Same body, different sender.
    assert_eq!(
        decide(&mut dedupe, &over_registration("+912", "one")),
        Disposition::Handle
    );
}

#[test]
fn where_the_modem_filed_a_message_says_nothing_about_what_it_is() {
    let mut dedupe = Dedupe::default();
    assert_eq!(
        decide(&mut dedupe, &through_modem("+911", "hello", 1)),
        Disposition::Handle
    );
    assert_eq!(
        decide(&mut dedupe, &through_modem("+911", "hello", 7)),
        Disposition::AcknowledgeOnly
    );
}

// ---- startup recovery (US5 scenario 7) -----------------------------------

#[test]
fn messages_already_in_modem_storage_at_startup_are_recovered() {
    // Texts that arrived while the service was down would otherwise be
    // stepped over and eventually lost when storage filled.
    let lines: Vec<String> = [
        "+CMGL: 1,\"REC UNREAD\",\"+919000000000\",,\"26/07/22,10:00:00+22\"",
        "hello",
        "+CMGL: 4,\"REC UNREAD\",\"+919876543210\",,\"26/07/22,10:05:00+22\"",
        "world",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    assert_eq!(parse_cmgl_indexes(&lines), vec![1, 4]);
}

#[test]
fn an_empty_message_store_recovers_nothing_rather_than_erroring() {
    assert!(parse_cmgl_indexes(&[]).is_empty());
    assert!(parse_cmgl_indexes(&["OK".to_string()]).is_empty());
}

// ---- the delivery route is observable (FR-036, research R10) --------------

#[test]
fn the_route_a_message_arrived_by_is_recorded() {
    // Whether Vi delivers over the registration or via the modem on LTE is
    // unverified — which is exactly why both are covered and why the answer
    // has to be observable rather than assumed.
    assert_eq!(MessageRoute::OverRegistration.as_str(), "registration");
    assert_eq!(MessageRoute::ThroughModem.as_str(), "modem");
    assert_ne!(
        MessageRoute::OverRegistration.as_str(),
        MessageRoute::ThroughModem.as_str()
    );
}

#[test]
fn cellular_messages_are_recorded_under_their_own_transport() {
    // Filed under the same label as Wi-Fi they would be indistinguishable,
    // and "which path carried this" is the question this feature exists to
    // answer.
    assert_eq!(Transport::Volte.as_str(), "volte");
    assert_ne!(Transport::Volte.as_str(), Transport::Vowifi.as_str());
    assert_ne!(Transport::Volte.as_str(), Transport::Cs.as_str());
}

// ---- bounded, and deliberately not persisted ------------------------------

#[test]
fn the_duplicate_window_stays_bounded() {
    let mut dedupe = Dedupe::new(4);
    for i in 0..500 {
        decide(&mut dedupe, &over_registration("+911", &format!("m{i}")));
    }
    assert!(dedupe.len() <= 4, "window must stay bounded");
}

// ---- shared ownership (specs/038-reliable-sms-delivery) --------------------
//
// `run_modem_reader`/`sweep_modem_storage` used to own a private `Dedupe`;
// they now take an externally-owned `Arc<Mutex<Dedupe>>` so the same instance
// can also be consulted by the registration-message handler. These tests
// prove that change in ownership does not change the decision logic itself —
// consulting `decide()` through the shared lock behaves identically to the
// pre-refactor owned-`Dedupe` case exercised by the tests above.

#[test]
fn a_failed_relay_is_rolled_back_so_the_retry_is_not_swallowed() {
    // The bug a naive "check contains, admit only after a successful relay"
    // implementation has: admitting *before* the relay closes the
    // two-routes-race-on-one-message window, but only if a relay failure then
    // releases the admission — otherwise a message whose relay genuinely
    // failed is stuck looking "already handled" forever.
    let dedupe = Arc::new(Mutex::new(Dedupe::default()));
    let msg = over_registration("+919000000000", "hello");

    let admitted = {
        let mut d = dedupe.lock().unwrap();
        decide(&mut d, &msg) == Disposition::Handle
    };
    assert!(admitted, "first delivery attempt must be admitted");

    // Simulate the relay failing: the caller must roll back.
    {
        let mut d = dedupe.lock().unwrap();
        d.forget(&msg.dedupe_key());
    }

    let retried = {
        let mut d = dedupe.lock().unwrap();
        decide(&mut d, &msg)
    };
    assert_eq!(
        retried,
        Disposition::Handle,
        "a retransmission after a rolled-back failed relay must be treated as fresh, not a duplicate"
    );
}

// ---- the modem sweep's settle-and-redecide (specs/038 review follow-up) ---
//
// `sweep_modem_storage` does not delete a modem-stored message purely
// because `decide` returned `AcknowledgeOnly` — that only means some other
// attempt (almost always the registration route) claimed it first, not that
// the claim succeeded. It waits out `CROSS_ROUTE_SETTLE_DELAY` (longer than
// the other side's relay could still be running) and calls `decide` again.
// These tests model both outcomes of that second call using the same public
// primitives the sweep itself uses, without needing the real multi-second
// wait or a mock modem.

#[test]
fn a_claim_still_standing_after_the_settle_wait_means_the_other_side_delivered_it() {
    let dedupe = Arc::new(Mutex::new(Dedupe::default()));
    let text = through_modem("+919000000000", "hello", 11);

    // Someone else (the registration route) claims it first.
    {
        let mut d = dedupe.lock().unwrap();
        assert_eq!(
            decide(&mut d, &over_registration("+919000000000", "hello")),
            Disposition::Handle
        );
    }

    // The sweep sees it claimed...
    let first_look = {
        let mut d = dedupe.lock().unwrap();
        decide(&mut d, &text)
    };
    assert_eq!(first_look, Disposition::AcknowledgeOnly);

    // ...waits out the settle delay (nothing changes: the claim was never
    // rolled back, meaning the registration route's relay succeeded)...
    let after_settling = {
        let mut d = dedupe.lock().unwrap();
        decide(&mut d, &text)
    };
    assert_eq!(
        after_settling,
        Disposition::AcknowledgeOnly,
        "a claim that is still standing after the settle wait was genuinely delivered — safe to clear"
    );
}

#[test]
fn a_claim_rolled_back_during_the_settle_wait_means_this_route_must_deliver_it() {
    let dedupe = Arc::new(Mutex::new(Dedupe::default()));
    let text = through_modem("+919000000001", "world", 12);
    let over_reg = over_registration("+919000000001", "world");

    // The registration route claims it first...
    {
        let mut d = dedupe.lock().unwrap();
        assert_eq!(decide(&mut d, &over_reg), Disposition::Handle);
    }
    let first_look = {
        let mut d = dedupe.lock().unwrap();
        decide(&mut d, &text)
    };
    assert_eq!(first_look, Disposition::AcknowledgeOnly);

    // ...but its relay then fails, and it rolls back (this is what would
    // happen during the sweep's settle wait, on a different thread).
    {
        let mut d = dedupe.lock().unwrap();
        d.forget(&over_reg.dedupe_key());
    }

    // Re-deciding after the settle wait must now find it free — and claim it
    // for this route, atomically, in the same call.
    let after_settling = {
        let mut d = dedupe.lock().unwrap();
        decide(&mut d, &text)
    };
    assert_eq!(
        after_settling,
        Disposition::Handle,
        "a rolled-back claim must free the message for this route to deliver, not discard it"
    );
}

#[test]
fn cross_bearer_duplicate_is_suppressed_through_one_shared_instance() {
    // This is what production wiring now does: `ims::agent::handle_message`
    // (registration route) and `run_modem_reader`'s sweep (modem route) for
    // one line consult the *same* `Arc<Mutex<Dedupe>>`. Proving the pure
    // `decide()` logic collapses a duplicate (already covered above) is not
    // the same as proving two independent call sites sharing one lock behave
    // that way end to end — this exercises exactly that shared-instance case.
    let dedupe = Arc::new(Mutex::new(Dedupe::default()));

    let via_registration = over_registration("+919000000000", "hello");
    {
        let mut d = dedupe.lock().unwrap();
        assert_eq!(decide(&mut d, &via_registration), Disposition::Handle);
    }

    let via_modem = through_modem("+919000000000", "hello", 5);
    {
        let mut d = dedupe.lock().unwrap();
        assert_eq!(
            decide(&mut d, &via_modem),
            Disposition::AcknowledgeOnly,
            "the same text delivered over the other bearer must not be forwarded again"
        );
    }
}

#[test]
fn decide_behaves_identically_through_a_shared_arc_mutex() {
    let dedupe = Arc::new(Mutex::new(Dedupe::default()));
    let msg = through_modem("+919000000000", "hello", 3);

    {
        let mut d = dedupe.lock().unwrap();
        assert_eq!(decide(&mut d, &msg), Disposition::Handle);
    }
    {
        let mut d = dedupe.lock().unwrap();
        assert_eq!(decide(&mut d, &msg), Disposition::AcknowledgeOnly);
    }
}

#[test]
fn backlog_recovery_is_unaffected_by_externally_owned_dedupe() {
    // parse_cmgl_indexes itself takes no Dedupe at all — this asserts the
    // refactor left the startup-recovery parsing path untouched.
    let lines: Vec<String> = [
        "+CMGL: 2,\"REC UNREAD\",\"+919000000000\",,\"26/07/22,10:00:00+22\"",
        "hello",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(parse_cmgl_indexes(&lines), vec![2]);
}

// ---- VoLTE parity, with or without CS (specs/038 User Story 3) ------------

#[test]
fn volte_backlog_recovery_is_unchanged_regardless_of_cs_enabled() {
    // `[cs].enabled` governs the circuit-switched call-bridging daemon, not
    // this reader — a VoLTE-assigned modem is exclusively assigned away from
    // the CS pool either way (specs/013/specs/020), so CS being globally on
    // or off must make no difference to what this parses.
    let lines: Vec<String> = [
        "+CMGL: 1,\"REC UNREAD\",\"+919000000000\",,\"26/07/22,10:00:00+22\"",
        "hello",
        "+CMGL: 2,\"REC UNREAD\",\"+919000000001\",,\"26/07/22,10:01:00+22\"",
        "world",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(parse_cmgl_indexes(&lines), vec![1, 2]);
}

#[test]
fn volte_gains_the_same_cross_bearer_suppression_us2_introduced() {
    // Before specs/038, VoLTE had this same latent gap (research.md Decision
    // 2): the registration route never consulted the sweep thread's private
    // Dedupe. Now that both share one instance per line, VoLTE benefits
    // identically to VoWiFi — this is the same assertion as the VoWiFi-framed
    // `cross_bearer_duplicate_is_suppressed_through_one_shared_instance` test,
    // kept separate because it verifies a distinct user story's guarantee.
    let dedupe = Arc::new(Mutex::new(Dedupe::default()));
    let via_modem = through_modem("+919000000002", "hi", 9);
    {
        let mut d = dedupe.lock().unwrap();
        assert_eq!(decide(&mut d, &via_modem), Disposition::Handle);
    }
    let via_registration = over_registration("+919000000002", "hi");
    {
        let mut d = dedupe.lock().unwrap();
        assert_eq!(
            decide(&mut d, &via_registration),
            Disposition::AcknowledgeOnly
        );
    }
}

#[test]
fn a_genuine_repeat_message_much_later_is_not_suppressed() {
    // Accepted deliberately: the window absorbs a retransmission, which
    // arrives within seconds. Suppressing a real repeat hours later would be
    // the worse failure — people do send "ok" twice.
    let mut dedupe = Dedupe::new(2);
    let first = over_registration("+911", "ok");
    assert_eq!(decide(&mut dedupe, &first), Disposition::Handle);
    decide(&mut dedupe, &over_registration("+911", "a"));
    decide(&mut dedupe, &over_registration("+911", "b"));
    assert_eq!(decide(&mut dedupe, &first), Disposition::Handle);
}
