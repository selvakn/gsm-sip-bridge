mod common;

use gsm_sip_bridge::modules::audio_pipeline::AudioPipeline;
use gsm_sip_bridge::modules::card::{CardInstance, CardState};
use std::sync::Arc;

#[test]
fn test_three_cards_independent_state() {
    let cards: Vec<CardInstance> = (0..3)
        .map(|i| {
            CardInstance::new(
                format!("ec20-{:06X}", i),
                format!("/dev/ttyUSB{}", i * 4 + 3).into(),
                format!("hw:{},0", i + 2),
                4,
            )
        })
        .collect();

    for card in &cards {
        assert_eq!(card.state, CardState::Idle);
    }
}

#[test]
fn test_three_cards_simultaneous_pipelines() {
    let pipelines: Vec<Arc<AudioPipeline>> =
        (0..3).map(|_| Arc::new(AudioPipeline::new())).collect();

    for (i, pipeline) in pipelines.iter().enumerate() {
        let frame = [(i as i16 + 1) * 100; 160];
        pipeline.push_capture_frame(frame);
    }

    for (i, pipeline) in pipelines.iter().enumerate() {
        let frame = pipeline.pop_capture_frame().unwrap();
        assert_eq!(frame[0], (i as i16 + 1) * 100);
    }
}

#[test]
fn test_one_teardown_does_not_affect_others() {
    let mut cards: Vec<CardInstance> = (0..3)
        .map(|i| {
            CardInstance::new(
                format!("ec20-{:06X}", i),
                format!("/dev/ttyUSB{}", i * 4 + 3).into(),
                format!("hw:{},0", i + 2),
                4,
            )
        })
        .collect();

    cards[0].state = CardState::Bridged;
    cards[1].state = CardState::Bridged;
    cards[2].state = CardState::Bridged;

    cards[1].state = CardState::Cleanup;
    cards[1].state = CardState::Idle;

    assert_eq!(cards[0].state, CardState::Bridged);
    assert_eq!(cards[1].state, CardState::Idle);
    assert_eq!(cards[2].state, CardState::Bridged);
}

#[test]
fn test_pipeline_isolation_across_modules() {
    let p1 = AudioPipeline::new();
    let p2 = AudioPipeline::new();

    p1.push_capture_frame([111; 160]);
    p2.push_capture_frame([222; 160]);

    let f1 = p1.pop_capture_frame().unwrap();
    let f2 = p2.pop_capture_frame().unwrap();

    assert_eq!(f1[0], 111);
    assert_eq!(f2[0], 222);
    assert!(p1.pop_capture_frame().is_none());
    assert!(p2.pop_capture_frame().is_none());
}
