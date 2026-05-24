use crate::modules::audio_pipeline::AudioPipeline;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardState {
    Idle,
    Ringing,
    Answering,
    Bridged,
    Cleanup,
}

pub struct CardInstance {
    pub id: String,
    pub serial_port: std::path::PathBuf,
    pub audio_device: String,
    pub state: CardState,
    pub pipeline: AudioPipeline,
}

impl CardInstance {
    pub fn new(
        id: String,
        serial_port: std::path::PathBuf,
        audio_device: String,
        ring_capacity: usize,
    ) -> Self {
        Self {
            id,
            serial_port,
            audio_device,
            state: CardState::Idle,
            pipeline: AudioPipeline::with_capacity(ring_capacity),
        }
    }
}
