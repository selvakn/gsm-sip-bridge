//! The `grid8` signal plan (data-model.md SignalPlan): 8 non-harmonic
//! frequencies in two groups of 4, one low + one high tone summed per
//! symbol, 16 distinct symbols. A recovered symbol identifies its own
//! transmit time, which is what makes round-trip delay measurable at all —
//! a single sine can only prove *something* arrived, not *when it was sent*.
//!
//! Deliberately not DTMF frequencies (research.md R6): a carrier or PBX in
//! the path may detect real DTMF and regenerate it out-of-band as RFC 2833,
//! destroying the in-band signal being measured.
//!
//! Generation is a pure function of an absolute sample index — never of
//! anything received (FR-023, `ims/echo.rs`'s independence warning).

use std::f64::consts::PI;

pub const LOWS: [f64; 4] = [600.0, 750.0, 900.0, 1050.0];
pub const HIGHS: [f64; 4] = [1300.0, 1500.0, 1700.0, 1900.0];
pub const SYMBOL_MS: u64 = 100;
pub const FRAME_SYMBOLS: usize = 16;
/// Above typical carrier noise gates, below limiter/clipping.
pub const LEVEL_DBFS: f64 = -12.0;

/// The (low, high) frequency pair for symbol `index % 16`.
pub fn symbol_frequencies(index: usize) -> (f64, f64) {
    let s = index % FRAME_SYMBOLS;
    (LOWS[s / 4], HIGHS[s % 4])
}

fn amplitude() -> f64 {
    i16::MAX as f64 * 10f64.powf(LEVEL_DBFS / 20.0)
}

fn symbol_duration_samples(audio_hz: u32) -> u64 {
    (audio_hz as u64 * SYMBOL_MS) / 1000
}

/// Which symbol is "current" at absolute sample index `sample_index`.
pub fn symbol_index_at(sample_index: u64, audio_hz: u32) -> usize {
    let d = symbol_duration_samples(audio_hz).max(1);
    ((sample_index / d) % FRAME_SYMBOLS as u64) as usize
}

/// Generates `n` samples of the grid8 signal starting at absolute sample
/// index `sample_index`.
pub fn generate(sample_index: u64, n: usize, audio_hz: u32) -> Vec<i16> {
    let amp = amplitude();
    (0..n)
        .map(|i| {
            let idx = sample_index + i as u64;
            let symbol = symbol_index_at(idx, audio_hz);
            let (low, high) = symbol_frequencies(symbol);
            let t = idx as f64 / audio_hz as f64;
            let sample = amp * 0.5 * ((2.0 * PI * low * t).sin() + (2.0 * PI * high * t).sin());
            sample.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_frequencies_are_distinct_across_all_sixteen_symbols() {
        let pairs: std::collections::HashSet<(u64, u64)> = (0..FRAME_SYMBOLS)
            .map(|i| {
                let (lo, hi) = symbol_frequencies(i);
                (lo.to_bits(), hi.to_bits())
            })
            .collect();
        assert_eq!(pairs.len(), FRAME_SYMBOLS);
    }

    #[test]
    fn symbol_index_advances_every_symbol_duration_and_wraps_at_sixteen() {
        let audio_hz = 8000;
        let d = symbol_duration_samples(audio_hz);
        assert_eq!(symbol_index_at(0, audio_hz), 0);
        assert_eq!(symbol_index_at(d - 1, audio_hz), 0);
        assert_eq!(symbol_index_at(d, audio_hz), 1);
        assert_eq!(symbol_index_at(d * 16, audio_hz), 0);
    }

    #[test]
    fn generated_signal_stays_within_i16_range_and_is_deterministic() {
        let a = generate(0, 1000, 8000);
        let b = generate(0, 1000, 8000);
        assert_eq!(a, b);
        assert!(a.iter().all(|&s| s != i16::MIN)); // never clips to the rail at -12dBFS
    }
}
