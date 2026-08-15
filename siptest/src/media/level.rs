//! Received-energy profile, reported independently of tone detection
//! (FR-022). `media_stats.rs`'s own reasoning — a quiet party still produces
//! frames, so packet counts and loudness answer different questions — is
//! why this exists at all: "nothing arrived" and "something arrived that
//! wasn't ours" must stay distinguishable, and only a level measurement can
//! tell them apart once packets are confirmed present.

const SILENT_THRESHOLD_DBFS: f64 = -60.0;
const NOISE_FLOOR_WINDOW_CAP: usize = 1500; // ~30s at 20ms windows

pub struct LevelMeter {
    peak_abs: f64,
    sum_sq: f64,
    count: u64,
    window_rms_dbfs: Vec<f64>,
    silent_windows: u64,
    total_windows: u64,
}

impl Default for LevelMeter {
    fn default() -> Self {
        Self {
            peak_abs: 0.0,
            sum_sq: 0.0,
            count: 0,
            window_rms_dbfs: Vec::new(),
            silent_windows: 0,
            total_windows: 0,
        }
    }
}

fn to_dbfs(linear: f64) -> f64 {
    20.0 * linear.max(1e-9).log10()
}

impl LevelMeter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one window (e.g. one decoded RTP frame's worth of samples).
    pub fn feed(&mut self, samples: &[i16]) {
        if samples.is_empty() {
            return;
        }
        let mut window_sum_sq = 0.0;
        for &s in samples {
            let v = s as f64;
            self.peak_abs = self.peak_abs.max(v.abs());
            self.sum_sq += v * v;
            window_sum_sq += v * v;
            self.count += 1;
        }
        let window_rms = (window_sum_sq / samples.len() as f64).sqrt() / i16::MAX as f64;
        let window_dbfs = to_dbfs(window_rms);
        if self.window_rms_dbfs.len() >= NOISE_FLOOR_WINDOW_CAP {
            self.window_rms_dbfs.remove(0);
        }
        self.window_rms_dbfs.push(window_dbfs);

        self.total_windows += 1;
        if window_dbfs < SILENT_THRESHOLD_DBFS {
            self.silent_windows += 1;
        }
    }

    pub fn peak_dbfs(&self) -> f64 {
        to_dbfs(self.peak_abs / i16::MAX as f64)
    }

    pub fn mean_dbfs(&self) -> f64 {
        if self.count == 0 {
            return SILENT_THRESHOLD_DBFS;
        }
        let rms = (self.sum_sq / self.count as f64).sqrt() / i16::MAX as f64;
        to_dbfs(rms)
    }

    /// 10th percentile of per-window RMS — a running estimate of the noise
    /// floor, used only for reporting (not as a detection gate; the Goertzel
    /// gates in `goertzel.rs` are self-contained per window).
    pub fn noise_floor_dbfs(&self) -> f64 {
        if self.window_rms_dbfs.is_empty() {
            return SILENT_THRESHOLD_DBFS;
        }
        let mut sorted = self.window_rms_dbfs.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let idx = ((sorted.len() as f64) * 0.10) as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    pub fn silent_frame_pct(&self) -> f64 {
        if self.total_windows == 0 {
            return 100.0;
        }
        (self.silent_windows as f64) * 100.0 / (self.total_windows as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_scale_sine_reads_close_to_zero_dbfs() {
        let mut meter = LevelMeter::new();
        let samples: Vec<i16> = (0..160)
            .map(|i| {
                let t = i as f64 / 8000.0;
                (i16::MAX as f64 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i16
            })
            .collect();
        meter.feed(&samples);
        assert!(meter.peak_dbfs() > -1.0, "peak was {}", meter.peak_dbfs());
    }

    #[test]
    fn silence_clamps_to_the_silent_threshold_and_is_flagged_silent() {
        let mut meter = LevelMeter::new();
        meter.feed(&vec![0i16; 160]);
        assert!(meter.mean_dbfs() <= SILENT_THRESHOLD_DBFS);
        assert_eq!(meter.silent_frame_pct(), 100.0);
    }

    #[test]
    fn a_loud_window_after_silent_ones_is_not_flagged_silent() {
        let mut meter = LevelMeter::new();
        meter.feed(&vec![0i16; 160]);
        meter.feed(&vec![0i16; 160]);
        meter.feed(&vec![20000i16; 160]);
        assert!(meter.silent_frame_pct() < 100.0);
        assert!(meter.silent_frame_pct() > 0.0);
    }
}
