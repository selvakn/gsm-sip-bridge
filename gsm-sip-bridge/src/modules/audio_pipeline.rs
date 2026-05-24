use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const FRAME_SIZE: usize = 160; // 20ms at 8kHz mono

// Large enough for tests that push many frames without caring about profile capacity.
const TEST_RING_CAPACITY: usize = 50;

pub type AudioFrame = [i16; FRAME_SIZE];

pub struct AudioPipeline {
    pub capture_ring: Arc<ArrayQueue<AudioFrame>>,
    pub playback_ring: Arc<ArrayQueue<AudioFrame>>,
    running: Arc<AtomicBool>,
}

impl AudioPipeline {
    /// Production constructor — capacity comes from the active `AudioProfileSettings`.
    pub fn with_capacity(ring_capacity: usize) -> Self {
        Self {
            capture_ring: Arc::new(ArrayQueue::new(ring_capacity)),
            playback_ring: Arc::new(ArrayQueue::new(ring_capacity)),
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Convenience constructor for tests; uses a large fixed capacity so tests are
    /// not coupled to any particular profile's ring size.
    pub fn new() -> Self {
        Self::with_capacity(TEST_RING_CAPACITY)
    }

    pub fn start(&self, _audio_device: &str) -> Result<(), String> {
        self.running.store(true, Ordering::SeqCst);
        tracing::info!("audio pipeline started (ALSA threads not yet spawned)");
        Ok(())
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn push_capture_frame(&self, frame: AudioFrame) -> bool {
        self.capture_ring.push(frame).is_ok()
    }

    pub fn pop_capture_frame(&self) -> Option<AudioFrame> {
        self.capture_ring.pop()
    }

    pub fn push_playback_frame(&self, frame: AudioFrame) -> bool {
        self.playback_ring.push(frame).is_ok()
    }

    pub fn pop_playback_frame(&self) -> Option<AudioFrame> {
        self.playback_ring.pop()
    }
}

impl Default for AudioPipeline {
    fn default() -> Self {
        Self::new()
    }
}
