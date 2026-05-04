use crate::modules::audio_pipeline::{AudioFrame, AudioPipeline};
use std::sync::Arc;

pub struct AlsaMediaPort {
    pipeline: Arc<AudioPipeline>,
}

impl AlsaMediaPort {
    pub fn new(pipeline: Arc<AudioPipeline>) -> Self {
        Self { pipeline }
    }

    pub fn read_frame(&self) -> AudioFrame {
        self.pipeline.pop_capture_frame().unwrap_or([0i16; 160])
    }

    pub fn write_frame(&self, frame: &AudioFrame) {
        let _ = self.pipeline.push_playback_frame(*frame);
    }
}
