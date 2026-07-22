//! Echo path for the diagnostic call (specs/016-volte-calls, US1).
//!
//! The test signal is the answering party's **own voice, returned to them**.
//! That needs no audio asset — which removes a licensing question and, more
//! importantly, a privacy one, since the only recordings to hand are real
//! calls involving real subscribers — and it is a better test than playing a
//! sample: people notice distortion, delay and dropouts in their own voice far
//! more readily than in a stranger's, the echo carries the degradation of
//! *both* directions at once, and round-trip delay becomes audible.
//!
//! # Why there is also a marker
//!
//! Echo alone would have quietly broken the feature's best diagnostic. If
//! everything sent is derived from what is received, the two directions stop
//! being independent: a total receive failure silences the outbound direction
//! too, so "nothing is reaching us" becomes indistinguishable from "our
//! transmit path is dead". That is exactly the direction attribution that
//! diagnosed the earlier one-way-audio incident.
//!
//! So a small generated marker is emitted on a fixed interval **regardless of
//! what has been received** (FR-029), keeping outbound audio non-zero at all
//! times. An implementation that dropped the marker would still pass every
//! other test in this module.
//!
//! # Feedback
//!
//! Returning audio to a device whose microphone can hear its own speaker forms
//! a loop. Attenuation below unity makes the loop gain converge geometrically;
//! the marker is additionally never echoed, so our own signal is not fed back
//! and amplified. A handset at the far end removes the question entirely, and
//! the operator is told to use one.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Default attenuation applied to returned audio. Below unity so successive
/// round trips decay rather than build.
pub const DEFAULT_ATTENUATION: f32 = 0.6;

/// Default gap between markers. Short enough that outbound audio is never
/// silent for long, long enough not to intrude on the echo.
pub const DEFAULT_MARKER_INTERVAL: Duration = Duration::from_secs(5);

/// How long each marker burst lasts.
const MARKER_DURATION: Duration = Duration::from_millis(400);

/// Bound on buffered far-end audio.
///
/// Echo is deliberately a *delay line*, not a jitter buffer: the delay is the
/// point, because it makes round-trip latency audible. But an unbounded queue
/// would grow without limit if the send loop ever fell behind the receive
/// thread, so the oldest audio is dropped past this bound — dropping stale
/// audio is better than echoing something the speaker said ten seconds ago.
const MAX_BUFFERED: Duration = Duration::from_millis(600);

/// Thread-safe hand-off from the receive thread to the send loop.
///
/// The receive thread decodes far-end audio and pushes it here; the send loop
/// pops what it needs for each outgoing packet. Cloning shares one buffer.
#[derive(Clone)]
pub struct EchoBuffer {
    inner: Arc<Mutex<VecDeque<i16>>>,
    capacity_samples: usize,
}

impl EchoBuffer {
    pub fn new(sample_rate: u32) -> Self {
        let capacity_samples = (MAX_BUFFERED.as_secs_f64() * sample_rate as f64).round() as usize;
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(capacity_samples))),
            capacity_samples: capacity_samples.max(1),
        }
    }

    /// Called by the receive thread with freshly decoded far-end audio.
    pub fn push(&self, samples: &[i16]) {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        q.extend(samples.iter().copied());
        // Drop the oldest rather than grow without bound.
        while q.len() > self.capacity_samples {
            q.pop_front();
        }
    }

    /// Called by the send loop. Returns fewer than `n` samples when the buffer
    /// is short — the caller pads, so a starved buffer produces silence rather
    /// than a stall.
    pub fn take(&self, n: usize) -> Vec<i16> {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let take = n.min(q.len());
        q.drain(..take).collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What the mixer put into one outgoing packet — reported so the operator can
/// tell an echoing call from one that is only emitting markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutgoingKind {
    /// Far-end audio returned.
    Echo,
    /// The independent generated marker (FR-029).
    Marker,
    /// Nothing was buffered and it was not marker time.
    Silence,
}

/// Builds the outgoing audio for each packet.
///
/// Deliberately holds no clock of its own: the caller passes elapsed time, so
/// the whole mixer is deterministic and testable without sleeping.
pub struct EchoMixer {
    buffer: EchoBuffer,
    attenuation: f32,
    marker_interval: Duration,
    sample_rate: u32,
    /// Absolute sample index into the marker pattern, so the marker keeps its
    /// shape across packets.
    marker_sample_index: u64,
    next_marker_at: Duration,
    marker_until: Option<Duration>,
    /// Set while a marker is being emitted, and briefly after, so our own
    /// marker coming back is not echoed and amplified.
    suppress_echo_until: Duration,
}

impl EchoMixer {
    pub fn new(
        buffer: EchoBuffer,
        sample_rate: u32,
        attenuation: f32,
        marker_interval: Duration,
    ) -> Self {
        Self {
            buffer,
            // Clamp below unity: an attenuation of 1.0 or more makes the
            // feedback loop non-convergent.
            attenuation: attenuation.clamp(0.0, 0.95),
            marker_interval,
            sample_rate,
            marker_sample_index: 0,
            // Emit one immediately, so outbound audio is non-zero from the
            // first packet even if nothing is ever received.
            next_marker_at: Duration::ZERO,
            marker_until: None,
            suppress_echo_until: Duration::ZERO,
        }
    }

    /// Produces exactly `samples` of outgoing audio for the packet starting at
    /// `elapsed` since the call was answered.
    pub fn next_packet(
        &mut self,
        elapsed: Duration,
        samples: usize,
        marker: &dyn MarkerSource,
    ) -> (Vec<i16>, OutgoingKind) {
        if elapsed >= self.next_marker_at && self.marker_until.is_none() {
            self.marker_until = Some(elapsed + MARKER_DURATION);
            self.marker_sample_index = 0;
        }

        if let Some(until) = self.marker_until {
            if elapsed < until {
                let pcm: Vec<i16> = (0..samples)
                    .map(|i| {
                        marker.sample_at(self.marker_sample_index + i as u64, self.sample_rate)
                    })
                    .collect();
                self.marker_sample_index += samples as u64;
                // Do not echo while we are transmitting, nor for a moment
                // after: what comes back during that window is likely our own
                // marker, and returning it would feed the loop.
                self.suppress_echo_until = until + MARKER_DURATION;
                return (pcm, OutgoingKind::Marker);
            }
            self.marker_until = None;
            self.next_marker_at = elapsed + self.marker_interval;
        }

        if elapsed < self.suppress_echo_until {
            // Discard rather than buffer: this audio is probably our own
            // marker returning, and holding it would only delay the loop.
            let _ = self.buffer.take(samples);
            return (vec![0; samples], OutgoingKind::Silence);
        }

        let mut pcm = self.buffer.take(samples);
        if pcm.is_empty() {
            return (vec![0; samples], OutgoingKind::Silence);
        }
        for s in pcm.iter_mut() {
            *s = (*s as f32 * self.attenuation) as i16;
        }
        pcm.resize(samples, 0);
        (pcm, OutgoingKind::Echo)
    }

    pub fn attenuation(&self) -> f32 {
        self.attenuation
    }
}

/// A generated signal the mixer can emit independently of what is received.
///
/// A trait so the existing three-tone pattern in `ims::call` can serve as the
/// marker rather than generating a second signal — reuse, and it is already
/// designed to make dropouts audible.
pub trait MarkerSource {
    fn sample_at(&self, sample_index: u64, sample_rate: u32) -> i16;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed-amplitude stand-in, so assertions are about the mixer rather than
    /// about a particular waveform.
    struct FlatMarker(i16);
    impl MarkerSource for FlatMarker {
        fn sample_at(&self, _i: u64, _sr: u32) -> i16 {
            self.0
        }
    }

    fn mixer(attenuation: f32, interval: Duration) -> EchoMixer {
        EchoMixer::new(EchoBuffer::new(16000), 16000, attenuation, interval)
    }

    #[test]
    fn echoed_audio_is_attenuated_below_unity() {
        let buf = EchoBuffer::new(16000);
        let mut m = EchoMixer::new(buf.clone(), 16000, 0.5, Duration::from_secs(60));
        // Step past the initial marker.
        m.next_packet(Duration::ZERO, 320, &FlatMarker(0));
        buf.push(&vec![1000i16; 320]);

        let (pcm, kind) = m.next_packet(Duration::from_secs(2), 320, &FlatMarker(0));

        assert_eq!(kind, OutgoingKind::Echo);
        assert_eq!(pcm[0], 500, "0.5 attenuation applied");
    }

    #[test]
    fn attenuation_is_clamped_below_unity() {
        // A loop gain of 1.0 or more never converges, so it must not be
        // reachable through configuration.
        assert!(mixer(1.0, Duration::from_secs(5)).attenuation() < 1.0);
        assert!(mixer(9.9, Duration::from_secs(5)).attenuation() < 1.0);
    }

    #[test]
    fn a_marker_is_emitted_immediately_so_outbound_is_never_silent_from_the_start() {
        let mut m = mixer(0.6, Duration::from_secs(5));

        let (pcm, kind) = m.next_packet(Duration::ZERO, 320, &FlatMarker(5000));

        assert_eq!(kind, OutgoingKind::Marker);
        assert!(pcm.iter().any(|&s| s != 0));
    }

    /// **The FR-029 invariant.**
    ///
    /// With nothing ever received, outbound audio must still be non-zero —
    /// otherwise a total receive failure silences both directions and becomes
    /// indistinguishable from a dead transmit path, destroying the direction
    /// attribution that diagnosed the previous one-way-audio incident.
    #[test]
    fn with_a_zero_receive_stream_outbound_is_still_non_zero() {
        let mut m = mixer(0.6, Duration::from_secs(1));
        let marker = FlatMarker(8000);
        let mut sent_samples: u64 = 0;
        let mut nonzero_samples: u64 = 0;

        // 30 seconds of packets, receiving absolutely nothing.
        for p in 0..1500u64 {
            let (pcm, _) = m.next_packet(Duration::from_millis(p * 20), 320, &marker);
            sent_samples += pcm.len() as u64;
            nonzero_samples += pcm.iter().filter(|&&s| s != 0).count() as u64;
        }

        assert!(
            nonzero_samples > 0,
            "outbound must never be entirely silent"
        );

        // And the verdict this produces must be SendOnly, never Neither.
        let v = crate::ims::media_stats::verdict(nonzero_samples, 0, 10);
        assert_eq!(v, crate::ims::media_stats::DirectionVerdict::SendOnly);
        assert!(sent_samples > 0);
    }

    #[test]
    fn the_marker_is_not_echoed_back_into_the_loop() {
        let buf = EchoBuffer::new(16000);
        let mut m = EchoMixer::new(buf.clone(), 16000, 0.6, Duration::from_secs(60));

        // Marker goes out at t=0.
        let (_, kind) = m.next_packet(Duration::ZERO, 320, &FlatMarker(9000));
        assert_eq!(kind, OutgoingKind::Marker);

        // Our own marker comes straight back while suppression is active.
        buf.push(&vec![9000i16; 320]);
        let (pcm, kind) = m.next_packet(Duration::from_millis(100), 320, &FlatMarker(9000));

        assert_eq!(kind, OutgoingKind::Marker, "still mid-marker");
        // Once the marker ends, suppression keeps the returned copy out.
        let (pcm2, kind2) = m.next_packet(Duration::from_millis(500), 320, &FlatMarker(9000));
        assert_eq!(
            kind2,
            OutgoingKind::Silence,
            "returned marker not re-echoed"
        );
        assert!(pcm2.iter().all(|&s| s == 0));
        let _ = pcm;
    }

    #[test]
    fn an_empty_buffer_produces_silence_of_the_right_length() {
        let mut m = mixer(0.6, Duration::from_secs(60));
        m.next_packet(Duration::ZERO, 320, &FlatMarker(0)); // consume the marker

        let (pcm, kind) = m.next_packet(Duration::from_secs(2), 320, &FlatMarker(0));

        assert_eq!(kind, OutgoingKind::Silence);
        assert_eq!(
            pcm.len(),
            320,
            "a starved buffer must not shorten the packet"
        );
    }

    #[test]
    fn a_short_buffer_is_padded_rather_than_truncated() {
        let buf = EchoBuffer::new(16000);
        let mut m = EchoMixer::new(buf.clone(), 16000, 1.0, Duration::from_secs(60));
        m.next_packet(Duration::ZERO, 320, &FlatMarker(0));
        buf.push(&vec![1000i16; 100]);

        let (pcm, _) = m.next_packet(Duration::from_secs(2), 320, &FlatMarker(0));

        assert_eq!(pcm.len(), 320);
        assert!(pcm[100..].iter().all(|&s| s == 0));
    }

    #[test]
    fn the_buffer_drops_the_oldest_audio_rather_than_growing_without_bound() {
        let buf = EchoBuffer::new(16000);
        // 600ms cap at 16kHz = 9600 samples.
        buf.push(&vec![1i16; 20000]);

        assert!(
            buf.len() <= 9600,
            "buffer must stay bounded, got {}",
            buf.len()
        );
    }

    #[test]
    fn the_buffer_is_shared_between_clones() {
        let a = EchoBuffer::new(16000);
        let b = a.clone();

        a.push(&[1, 2, 3]);

        assert_eq!(
            b.len(),
            3,
            "receive thread and send loop must share one buffer"
        );
        assert_eq!(b.take(3), vec![1, 2, 3]);
        assert!(a.is_empty());
    }

    #[test]
    fn echo_resumes_after_the_suppression_window() {
        let buf = EchoBuffer::new(16000);
        let mut m = EchoMixer::new(buf.clone(), 16000, 0.5, Duration::from_secs(60));
        m.next_packet(Duration::ZERO, 320, &FlatMarker(0));

        buf.push(&vec![2000i16; 320]);
        let (_, kind) = m.next_packet(Duration::from_secs(5), 320, &FlatMarker(0));

        assert_eq!(
            kind,
            OutgoingKind::Echo,
            "echo must resume once suppression lapses"
        );
    }
}
