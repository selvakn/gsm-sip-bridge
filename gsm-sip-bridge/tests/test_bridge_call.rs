use gsm_sip_bridge::modules::audio_pipeline::AudioPipeline;
use gsm_sip_bridge::modules::beep::BeepGenerator;
use gsm_sip_bridge::sip::alsa_media_port::AlsaMediaPort;
use std::sync::Arc;

#[test]
fn test_beep_through_pipeline() {
    let pipeline = Arc::new(AudioPipeline::new());
    let media_port = AlsaMediaPort::new(pipeline.clone());
    let mut beep = BeepGenerator::new();
    beep.start();

    let mut frame = [0i16; 160];
    beep.fill_buffer(&mut frame);
    assert!(pipeline.push_capture_frame(frame));

    let read = media_port.read_frame();
    assert_eq!(read[0], frame[0]);
}

#[test]
fn test_bidirectional_audio_flow() {
    let pipeline = Arc::new(AudioPipeline::new());
    let media_port = AlsaMediaPort::new(pipeline.clone());

    let gsm_frame = [500i16; 160];
    pipeline.push_capture_frame(gsm_frame);
    let sip_receives = media_port.read_frame();
    assert_eq!(sip_receives[0], 500);

    let sip_frame = [300i16; 160];
    media_port.write_frame(&sip_frame);
    let gsm_receives = pipeline.pop_playback_frame().unwrap();
    assert_eq!(gsm_receives[0], 300);
}

#[test]
fn test_audio_silence_on_empty() {
    let pipeline = Arc::new(AudioPipeline::new());
    let media_port = AlsaMediaPort::new(pipeline.clone());

    let frame = media_port.read_frame();
    assert_eq!(frame, [0i16; 160]);
}

#[test]
fn test_multiple_frames_in_sequence() {
    let pipeline = Arc::new(AudioPipeline::new());
    let media_port = AlsaMediaPort::new(pipeline.clone());

    for i in 0..10 {
        let frame = [i as i16; 160];
        pipeline.push_capture_frame(frame);
    }

    for i in 0..10 {
        let read = media_port.read_frame();
        assert_eq!(read[0], i as i16);
    }
}
