//! Goertzel-based detection of the `grid8` tone plan — the standard
//! DTMF-detector recipe applied to a wider, non-DTMF grid (research.md R6).
//!
//! A window's detection must survive three independent gates before it
//! counts: enough of the window's energy is in the two candidate tones
//! (relative energy), the two tones are roughly balanced (twist), and no
//! other bin carries comparable energy (broadband guard — rejects speech,
//! music, ringback). This is what makes "is this a tone or is this noise"
//! actually answerable, rather than a threshold on raw energy.

use std::f64::consts::PI;

use crate::media::tone::{FRAME_SYMBOLS, HIGHS, LOWS, SYMBOL_MS};

const RELATIVE_ENERGY_MIN: f64 = 0.35;
const TWIST_DB_MAX: f64 = 8.0;
const BROADBAND_GUARD_RATIO: f64 = 0.5;

fn goertzel_power(samples: &[i16], freq: f64, sample_rate: u32) -> f64 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    let k = (0.5 + (n as f64 * freq) / sample_rate as f64).floor();
    let omega = 2.0 * PI * k / n as f64;
    let coeff = 2.0 * omega.cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for &x in samples {
        let s0 = x as f64 + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - coeff * s1 * s2
}

/// Attempts to detect one grid8 symbol in a single window. Returns the
/// symbol index only if all three gates pass.
pub fn detect_window(samples: &[i16], audio_hz: u32) -> Option<usize> {
    if samples.is_empty() {
        return None;
    }
    let total_energy: f64 = samples.iter().map(|&x| (x as f64) * (x as f64)).sum();
    if total_energy <= 0.0 {
        return None;
    }

    let low_powers: Vec<f64> = LOWS
        .iter()
        .map(|&f| goertzel_power(samples, f, audio_hz))
        .collect();
    let high_powers: Vec<f64> = HIGHS
        .iter()
        .map(|&f| goertzel_power(samples, f, audio_hz))
        .collect();

    let (mut best_li, mut best_hi, mut best_score) = (0usize, 0usize, -1.0f64);
    for (li, &pl) in low_powers.iter().enumerate() {
        for (hi, &ph) in high_powers.iter().enumerate() {
            let score = pl.min(ph);
            if score > best_score {
                best_score = score;
                best_li = li;
                best_hi = hi;
            }
        }
    }
    let pl = low_powers[best_li];
    let ph = high_powers[best_hi];

    if (pl + ph) / total_energy < RELATIVE_ENERGY_MIN {
        return None;
    }
    let twist_db = 10.0 * (pl.max(1e-9) / ph.max(1e-9)).log10().abs();
    if twist_db > TWIST_DB_MAX {
        return None;
    }
    let floor = pl.min(ph);
    for (i, &p) in low_powers.iter().enumerate() {
        if i != best_li && p > BROADBAND_GUARD_RATIO * floor {
            return None;
        }
    }
    for (i, &p) in high_powers.iter().enumerate() {
        if i != best_hi && p > BROADBAND_GUARD_RATIO * floor {
            return None;
        }
    }

    Some(best_li * 4 + best_hi)
}

/// Accumulates per-window votes across one symbol period (`SYMBOL_MS`) and
/// decides the winner only when at least 3 of the windows agree — absorbs
/// symbol-boundary misalignment between transmitter and receiver without
/// needing any overlap machinery.
pub struct SymbolDecoder {
    audio_hz: u32,
    windows_per_symbol: u32,
    window_count: u32,
    votes: [u32; FRAME_SYMBOLS],
}

const MAJORITY: u32 = 3;

impl SymbolDecoder {
    pub fn new(audio_hz: u32, window_ms: u64) -> Self {
        let windows_per_symbol = (SYMBOL_MS / window_ms.max(1)).max(1) as u32;
        Self {
            audio_hz,
            windows_per_symbol,
            window_count: 0,
            votes: [0; FRAME_SYMBOLS],
        }
    }

    /// Feeds one window's samples. Returns `Some(symbol_index)` when a
    /// symbol boundary is reached with a majority winner.
    pub fn feed(&mut self, samples: &[i16]) -> Option<usize> {
        if let Some(idx) = detect_window(samples, self.audio_hz) {
            self.votes[idx] += 1;
        }
        self.window_count += 1;
        if self.window_count >= self.windows_per_symbol {
            let winner = self
                .votes
                .iter()
                .enumerate()
                .max_by_key(|(_, &v)| v)
                .filter(|(_, &v)| v >= MAJORITY)
                .map(|(i, _)| i);
            self.votes = [0; FRAME_SYMBOLS];
            self.window_count = 0;
            return winner;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::tone;

    fn window_at(sample_index: u64, len: usize, audio_hz: u32) -> Vec<i16> {
        tone::generate(sample_index, len, audio_hz)
    }

    #[test]
    fn a_generated_symbol_window_is_detected_as_itself() {
        let audio_hz = 8000;
        let window = window_at(0, 160, audio_hz); // symbol 0 at t=0
        let detected = detect_window(&window, audio_hz).expect("should detect symbol 0");
        assert_eq!(detected, 0);
    }

    #[test]
    fn white_noise_is_rejected_by_the_relative_energy_gate() {
        // A simple deterministic PRNG-free "noise": alternating extremes, which
        // spreads energy across every bin rather than concentrating it in two.
        let noise: Vec<i16> = (0..160)
            .map(|i| if i % 2 == 0 { 12000 } else { -11000 })
            .collect();
        assert!(detect_window(&noise, 8000).is_none());
    }

    #[test]
    fn silence_is_rejected() {
        let silence = vec![0i16; 160];
        assert!(detect_window(&silence, 8000).is_none());
    }

    #[test]
    fn a_single_off_grid_tone_is_rejected_by_twist() {
        // Pure 1kHz sine — no matching low/high pair, so whichever bins score
        // highest will be wildly imbalanced (twist gate should reject it).
        let audio_hz = 8000;
        let samples: Vec<i16> = (0..160)
            .map(|i| {
                let t = i as f64 / audio_hz as f64;
                (8000.0 * (2.0 * PI * 1000.0 * t).sin()) as i16
            })
            .collect();
        assert!(detect_window(&samples, audio_hz).is_none());
    }

    #[test]
    fn symbol_decoder_needs_a_majority_of_windows_to_agree() {
        let audio_hz = 8000;
        let mut decoder = SymbolDecoder::new(audio_hz, 20);
        // 5 windows per 100ms symbol at 20ms/window. Feed 4 windows of symbol
        // 3, 1 window of silence — majority (4 >= 3) should still decide.
        let mut result = None;
        for i in 0..4u64 {
            // Symbol 3 spans sample indices [3*800, 4*800) at 8kHz/100ms-per-symbol.
            let window = tone::generate(3 * 800 + i * 160, 160, audio_hz);
            result = decoder.feed(&window);
        }
        let silence = vec![0i16; 160];
        result = decoder.feed(&silence).or(result);
        assert_eq!(result, Some(3));
    }

    #[test]
    fn tone_survives_pcmu_round_trip_and_is_still_detected() {
        use crate::media::codec::{decode_pcmu, encode_pcmu};
        let audio_hz = 8000;
        let original = tone::generate(0, 160, audio_hz);
        let decoded = decode_pcmu(&encode_pcmu(&original));
        assert_eq!(detect_window(&decoded, audio_hz), Some(0));
    }
}
