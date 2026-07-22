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
        over_registration("+919789063708", "hello"),
        through_modem("+919789063708", "hello", 3),
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
        decide(&mut dedupe, &over_registration("+919789063708", "hello")),
        Disposition::Handle
    );
    assert_eq!(
        decide(&mut dedupe, &through_modem("+919789063708", "hello", 3)),
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
    let msg = over_registration("+919789063708", "hello");
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
        "+CMGL: 1,\"REC UNREAD\",\"+919789063708\",,\"26/07/22,10:00:00+22\"",
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
