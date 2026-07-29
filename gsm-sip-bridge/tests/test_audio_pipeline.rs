use gsm_sip_bridge::modules::audio_pipeline::AudioPipeline;

#[test]
fn test_push_pop_capture_ring() {
    let pipeline = AudioPipeline::new();
    let frame = [42i16; 160];

    assert!(pipeline.push_capture_frame(frame));
    let popped = pipeline.pop_capture_frame().unwrap();
    assert_eq!(popped[0], 42);
    assert_eq!(popped[159], 42);
}

#[test]
fn test_push_pop_playback_ring() {
    let pipeline = AudioPipeline::new();
    let frame = [100i16; 160];

    assert!(pipeline.push_playback_frame(frame));
    let popped = pipeline.pop_playback_frame().unwrap();
    assert_eq!(popped[0], 100);
}

#[test]
fn test_empty_pop_returns_none() {
    let pipeline = AudioPipeline::new();
    assert!(pipeline.pop_capture_frame().is_none());
    assert!(pipeline.pop_playback_frame().is_none());
}

#[test]
fn test_overrun_drops_oldest() {
    let pipeline = AudioPipeline::new();
    for i in 0..60 {
        let frame = [i as i16; 160];
        let _ = pipeline.push_capture_frame(frame);
    }
    let popped = pipeline.pop_capture_frame().unwrap();
    assert_eq!(popped[0], 0);
}

#[test]
fn test_start_stop_lifecycle() {
    let pipeline = AudioPipeline::new();
    assert!(!pipeline.is_running());

    pipeline.start("null").unwrap();
    assert!(pipeline.is_running());

    pipeline.stop();
    assert!(!pipeline.is_running());
}

#[test]
fn test_steady_state_no_loss() {
    let pipeline = AudioPipeline::new();
    let frame_count = 40;

    for i in 0..frame_count {
        let frame = [i as i16; 160];
        assert!(pipeline.push_capture_frame(frame));
    }

    for i in 0..frame_count {
        let popped = pipeline.pop_capture_frame().unwrap();
        assert_eq!(popped[0], i as i16);
    }
}
