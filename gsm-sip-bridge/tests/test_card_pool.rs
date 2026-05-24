mod common;

use gsm_sip_bridge::modules::card::{CardInstance, CardState};

const TEST_RING: usize = 4;

#[test]
fn test_card_instance_initial_state() {
    let card =
        CardInstance::new("ec20-A1B2C3".into(), "/dev/ttyUSB3".into(), "hw:2,0".into(), TEST_RING);
    assert_eq!(card.state, CardState::Idle);
    assert_eq!(card.id, "ec20-A1B2C3");
}

#[test]
fn test_card_state_transitions() {
    let mut card =
        CardInstance::new("ec20-D4E5F6".into(), "/dev/ttyUSB7".into(), "hw:3,0".into(), TEST_RING);

    assert_eq!(card.state, CardState::Idle);
    card.state = CardState::Ringing;
    assert_eq!(card.state, CardState::Ringing);
    card.state = CardState::Answering;
    assert_eq!(card.state, CardState::Answering);
    card.state = CardState::Bridged;
    assert_eq!(card.state, CardState::Bridged);
    card.state = CardState::Cleanup;
    assert_eq!(card.state, CardState::Cleanup);
    card.state = CardState::Idle;
    assert_eq!(card.state, CardState::Idle);
}

#[test]
fn test_multiple_cards_independent() {
    let card1 =
        CardInstance::new("ec20-111111".into(), "/dev/ttyUSB3".into(), "hw:2,0".into(), TEST_RING);
    let card2 =
        CardInstance::new("ec20-222222".into(), "/dev/ttyUSB7".into(), "hw:3,0".into(), TEST_RING);
    let card3 = CardInstance::new(
        "ec20-333333".into(),
        "/dev/ttyUSB11".into(),
        "hw:4,0".into(),
        TEST_RING,
    );

    assert_eq!(card1.state, CardState::Idle);
    assert_eq!(card2.state, CardState::Idle);
    assert_eq!(card3.state, CardState::Idle);
    assert_ne!(card1.id, card2.id);
    assert_ne!(card2.id, card3.id);
}

#[test]
fn test_failed_recovery_path() {
    let mut card = CardInstance::new(
        "ec20-FAILED".into(),
        "/dev/ttyUSB99".into(),
        "hw:99,0".into(),
        TEST_RING,
    );

    card.state = CardState::Cleanup;
    card.state = CardState::Idle;
    assert_eq!(card.state, CardState::Idle);
}
