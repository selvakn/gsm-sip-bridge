//! ITU-T G.722 mode 1 (64 kbit/s) sub-band ADPCM, ~400 lines in-crate rather
//! than an external dependency — research.md R7's conclusion, after the
//! crates.io survey came out poorly (`ezk-g722` mandatorily pulls a whole
//! SIP framework; `audio-codec` pulls an unrelated FFI-flavoured codec;
//! `oxideav-g722` has 540 total downloads). Deferred until the tone/Goertzel
//! pipeline was proven on PCMU (US4), so a codec bug would be easy to
//! isolate — it landed after that pipeline's own tests passed.
//!
//! The numeric tables below (QMF taps, quantizer/scale-factor tables) are
//! the constants ITU-T Recommendation G.722 mandates for interoperability —
//! every compliant implementation reproduces the same numbers, which is why
//! they appear identically across FFmpeg, SpanDSP, and PJSIP's own G.722.
//! The code structure here is original, written from that specification
//! (cross-checked against FFmpeg's `libavcodec/g722.c` for numeric accuracy
//! — LGPL, not copied: only the standard's own required constants were
//! taken from it, not its expression).
//!
//! **The trap this file exists to get right** (research.md, `media/codec.rs`):
//! G.722's RTP clock advances at 8 kHz (per `a=rtpmap:9 G722/8000`, RFC
//! 3551's historical error) while the audio it carries is 16 kHz — one byte
//! encodes *two* 16 kHz samples. `CodecProfile::G722` keeps `rtp_clock_hz`
//! and `audio_hz` as separate fields specifically so nothing downstream can
//! confuse the two.

const QMF_TAPS: [i32; 12] = [3, -11, 12, 32, -210, 951, 3876, -805, 362, -156, 53, -11];

const INV_LOG2_TABLE: [i32; 32] = [
    2048, 2093, 2139, 2186, 2233, 2282, 2332, 2383, 2435, 2489, 2543, 2599, 2656, 2714, 2774, 2834,
    2896, 2960, 3025, 3091, 3158, 3228, 3298, 3371, 3444, 3520, 3597, 3676, 3756, 3838, 3922, 4008,
];

const HIGH_LOG_FACTOR_STEP: [i32; 2] = [798, -214];
const HIGH_INV_QUANT: [i32; 4] = [-926, -202, 926, 202];

const LOW_LOG_FACTOR_STEP: [i32; 16] = [
    -60, 3042, 1198, 538, 334, 172, 58, -30, 3042, 1198, 538, 334, 172, 58, -30, -60,
];
const LOW_INV_QUANT4: [i32; 16] = [
    0, -2557, -1612, -1121, -786, -530, -323, -150, 2557, 1612, 1121, 786, 530, 323, 150, 0,
];
const LOW_INV_QUANT6: [i32; 64] = [
    -17, -17, -17, -17, -3101, -2738, -2376, -2088, -1873, -1689, -1535, -1399, -1279, -1170,
    -1072, -982, -899, -822, -750, -682, -618, -558, -501, -447, -396, -347, -300, -254, -211,
    -170, -130, -91, 3101, 2738, 2376, 2088, 1873, 1689, 1535, 1399, 1279, 1170, 1072, 982, 899,
    822, 750, 682, 618, 558, 501, 447, 396, 347, 300, 254, 211, 170, 130, 91, 54, 17, -54, -17,
];
/// Forward-quantizer decision thresholds for the low band (29 of the 33
/// slots the reference table reserves are ever read; the rest are
/// unreachable by construction of the search below).
const LOW_QUANT: [i32; 29] = [
    35, 72, 110, 150, 190, 233, 276, 323, 370, 422, 473, 530, 587, 650, 714, 786, 858, 940, 1023,
    1121, 1219, 1339, 1458, 1612, 1765, 1980, 2195, 2557, 2919,
];

#[derive(Clone, Copy, Default)]
struct Band {
    s_predictor: i32,
    s_zero: i32,
    part_reconst_mem: [i32; 2],
    prev_qtzd_reconst: i32,
    pole_mem: [i32; 2],
    diff_mem: [i32; 6],
    zero_mem: [i32; 6],
    log_factor: i32,
    scale_factor: i32,
}

/// Persistent codec state: the two sub-band predictors plus a 24-sample QMF
/// history. Must be held across the whole call, not reset per packet — the
/// ADPCM adaptation is only meaningful as a running state machine.
#[derive(Clone)]
pub struct G722State {
    low: Band,
    high: Band,
    /// Oldest sample at index 0, newest at index 23.
    history: [i32; 24],
}

impl Default for G722State {
    fn default() -> Self {
        Self::new()
    }
}

impl G722State {
    pub fn new() -> Self {
        Self {
            low: Band {
                scale_factor: 8,
                ..Default::default()
            },
            high: Band {
                scale_factor: 2,
                ..Default::default()
            },
            history: [0; 24],
        }
    }
}

fn clip_int16(x: i32) -> i32 {
    x.clamp(i16::MIN as i32, i16::MAX as i32)
}

fn clip_intp2(x: i32, p: u32) -> i32 {
    let hi = (1i32 << p) - 1;
    let lo = -(1i32 << p);
    x.clamp(lo, hi)
}

fn linear_scale_factor(log_factor: i32) -> i32 {
    let wd1 = INV_LOG2_TABLE[((log_factor >> 6) & 31) as usize];
    let shift = log_factor >> 11;
    if shift < 0 {
        wd1 >> (-shift)
    } else {
        wd1 << shift
    }
}

/// The seventh-order zero-section adaptive filter (ITU-T G.722 §3.4). Each
/// call shifts a 6-entry delay line of past `cur_diff` values and adapts
/// `zero_mem` by the sign correlation between the delayed value and the
/// current one — a standard sign-sign LMS-style update.
fn update_s_zero(band: &mut Band, cur_diff: i32) {
    let has_diff = i32::from(cur_diff != 0);
    // Captured before any mutation: index 0 feeds from the oldest retained
    // difference, index 5 is the newest (`cur_diff` itself, doubled).
    let inputs = [
        band.diff_mem[4],
        band.diff_mem[3],
        band.diff_mem[2],
        band.diff_mem[1],
        band.diff_mem[0],
        cur_diff * 2,
    ];
    let mut s_zero: i32 = 0;
    for (k, &x) in inputs.iter().enumerate() {
        let sign_term = if (band.diff_mem[k] ^ cur_diff) < 0 {
            -128
        } else {
            128
        };
        band.zero_mem[k] = ((band.zero_mem[k] * 255) >> 8) + has_diff * sign_term;
        band.diff_mem[k] = x;
        s_zero += (x * band.zero_mem[k]) >> 15;
    }
    band.s_zero = s_zero;
}

/// The two-pole adaptive predictor (ITU-T G.722 §3.5–3.6): combines the
/// zero-section output with a second-order pole section, producing the next
/// predicted sample.
fn adaptive_predict(band: &mut Band, cur_diff: i32) {
    let cur_part_reconst = i32::from(band.s_zero + cur_diff < 0);
    let sg0 = if cur_part_reconst != band.part_reconst_mem[0] {
        1
    } else {
        -1
    };
    let sg1 = if cur_part_reconst == band.part_reconst_mem[1] {
        1
    } else {
        -1
    };
    band.part_reconst_mem[1] = band.part_reconst_mem[0];
    band.part_reconst_mem[0] = cur_part_reconst;

    let pole0_clamped = band.pole_mem[0].clamp(-8191, 8191);
    band.pole_mem[1] = ((sg0 * pole0_clamped) >> 5) + sg1 * 128 + ((band.pole_mem[1] * 127) >> 7);
    band.pole_mem[1] = band.pole_mem[1].clamp(-12288, 12288);

    let limit = 15360 - band.pole_mem[1];
    band.pole_mem[0] = (-192 * sg0 + ((band.pole_mem[0] * 255) >> 8)).clamp(-limit, limit);

    update_s_zero(band, cur_diff);

    let cur_qtzd_reconst = clip_int16((band.s_predictor + cur_diff) * 2);
    band.s_predictor = clip_int16(
        band.s_zero
            + ((band.pole_mem[0] * cur_qtzd_reconst) >> 15)
            + ((band.pole_mem[1] * band.prev_qtzd_reconst) >> 15),
    );
    band.prev_qtzd_reconst = cur_qtzd_reconst;
}

/// `ilow4` is the coarser 4-bit index (`ilow6 >> 2`) — the standard
/// deliberately updates the adaptive predictor from a reduced-precision
/// quantization even though reconstruction uses the full 6-bit value.
fn update_low_predictor(band: &mut Band, ilow4: usize) {
    let diff = (band.scale_factor * LOW_INV_QUANT4[ilow4]) >> 10;
    adaptive_predict(band, diff);
    band.log_factor = (((band.log_factor * 127) >> 7) + LOW_LOG_FACTOR_STEP[ilow4]).clamp(0, 18432);
    band.scale_factor = linear_scale_factor(band.log_factor - (8 << 11));
}

fn update_high_predictor(band: &mut Band, dhigh: i32, ihigh: usize) {
    adaptive_predict(band, dhigh);
    band.log_factor =
        (((band.log_factor * 127) >> 7) + HIGH_LOG_FACTOR_STEP[ihigh & 1]).clamp(0, 22528);
    band.scale_factor = linear_scale_factor(band.log_factor - (10 << 11));
}

/// `diff ^ (diff >> 31)`: 0 for a non-negative `diff`, `-diff - 1` for a
/// negative one. The standard's usual trick to fold sign handling into one
/// magnitude-like value without a branch that could mishandle `i32::MIN`.
fn folded_magnitude(diff: i32) -> i32 {
    diff ^ (diff >> 31)
}

fn encode_low(band: &Band, xlow: i32) -> usize {
    let diff = clip_int16(xlow - band.s_predictor);
    let limit = (folded_magnitude(diff) + 1) << 10;
    let mut i: i32 = 0;
    if limit > LOW_QUANT[8] * band.scale_factor {
        i = 9;
    }
    while (i as usize) < 29 && limit > LOW_QUANT[i as usize] * band.scale_factor {
        i += 1;
    }
    let base = if diff < 0 {
        if i < 2 {
            63
        } else {
            33
        }
    } else {
        61
    };
    (base - i) as usize
}

fn encode_high(band: &Band, xhigh: i32) -> usize {
    let diff = clip_int16(xhigh - band.s_predictor);
    let pred = (141 * band.scale_factor) >> 8;
    let base = i32::from(folded_magnitude(diff) < pred);
    (base + 2 * i32::from(diff >= 0)) as usize
}

/// QMF analysis/synthesis filter (ITU-T G.722 Table 11 taps, applied as a
/// polyphase even/odd decomposition): the low output uses the 12 even-index
/// history samples in tap order, the high output the 12 odd-index samples
/// in reverse tap order. The same function serves both encode (splitting
/// raw input into sub-bands) and decode (recombining reconstructed sub-band
/// values) — QMF's perfect-reconstruction property is what makes that work.
fn apply_qmf(history: &[i32; 24]) -> (i32, i32) {
    let mut even_sum: i64 = 0;
    let mut odd_sum: i64 = 0;
    for k in 0..12 {
        even_sum += history[2 * k] as i64 * QMF_TAPS[k] as i64;
        odd_sum += history[2 * k + 1] as i64 * QMF_TAPS[11 - k] as i64;
    }
    (odd_sum as i32, even_sum as i32)
}

fn push_history(history: &mut [i32; 24], a: i32, b: i32) {
    history.copy_within(2.., 0);
    history[22] = a;
    history[23] = b;
}

/// Encodes 16 kHz linear PCM into G.722 mode-1 bytes — one byte per input
/// sample *pair*. An odd trailing sample is padded by repeating itself.
pub fn encode(state: &mut G722State, samples: &[i16], out: &mut Vec<u8>) {
    let mut i = 0;
    while i < samples.len() {
        let s0 = samples[i] as i32;
        let s1 = if i + 1 < samples.len() {
            samples[i + 1] as i32
        } else {
            s0
        };
        push_history(&mut state.history, s0, s1);
        let (xout0, xout1) = apply_qmf(&state.history);
        let xlow = (xout0 + xout1) >> 14;
        let xhigh = (xout0 - xout1) >> 14;

        let ihigh = encode_high(&state.high, xhigh);
        let ilow = encode_low(&state.low, xlow);

        let dhigh = (state.high.scale_factor * HIGH_INV_QUANT[ihigh]) >> 10;
        update_high_predictor(&mut state.high, dhigh, ihigh);
        update_low_predictor(&mut state.low, ilow >> 2);

        out.push(((ihigh << 6) | ilow) as u8);
        i += 2;
    }
}

/// Decodes G.722 mode-1 bytes back to 16 kHz linear PCM — two samples per
/// input byte.
pub fn decode(state: &mut G722State, data: &[u8], out: &mut Vec<i16>) {
    for &byte in data {
        let ihigh = (byte >> 6) as usize;
        let ilow = (byte & 0x3f) as usize;

        let rlow = clip_intp2(
            ((state.low.scale_factor * LOW_INV_QUANT6[ilow]) >> 10) + state.low.s_predictor,
            14,
        );
        update_low_predictor(&mut state.low, ilow >> 2);

        let dhigh = (state.high.scale_factor * HIGH_INV_QUANT[ihigh]) >> 10;
        let rhigh = clip_intp2(dhigh + state.high.s_predictor, 14);
        update_high_predictor(&mut state.high, dhigh, ihigh);

        push_history(&mut state.history, rlow + rhigh, rlow - rhigh);
        let (xout0, xout1) = apply_qmf(&state.history);
        out.push(clip_int16(xout0 >> 11) as i16);
        out.push(clip_int16(xout1 >> 11) as i16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(n: usize, freq: f64, hz: f64, amp: f64) -> Vec<i16> {
        (0..n)
            .map(|i| {
                let t = i as f64 / hz;
                (amp * (2.0 * std::f64::consts::PI * freq * t).sin()) as i16
            })
            .collect()
    }

    #[test]
    fn one_byte_encodes_exactly_two_samples_and_decodes_back_to_two() {
        let mut enc = G722State::new();
        let mut dec = G722State::new();
        let samples = sine(320, 1000.0, 16000.0, 8000.0);
        let mut bytes = Vec::new();
        encode(&mut enc, &samples, &mut bytes);
        assert_eq!(
            bytes.len(),
            160,
            "320 samples must encode to 160 bytes at mode 1"
        );

        let mut decoded = Vec::new();
        decode(&mut dec, &bytes, &mut decoded);
        assert_eq!(decoded.len(), 320);
    }

    #[test]
    fn a_1khz_tone_round_trips_with_low_quantisation_error() {
        let mut enc = G722State::new();
        let mut dec = G722State::new();
        let samples = sine(1600, 1000.0, 16000.0, 8000.0);
        let mut bytes = Vec::new();
        encode(&mut enc, &samples, &mut bytes);
        let mut decoded = Vec::new();
        decode(&mut dec, &bytes, &mut decoded);

        // The QMF analysis/synthesis pair carries an inherent algorithmic
        // delay of 22 samples at 16kHz (~1.4ms) — the depth of the 24-tap
        // history window each stage needs before its first real output.
        // This is a property of G.722 itself (FFmpeg's own encoder
        // documents it as `initial_padding = 22`), not a bug: comparing
        // `decoded[i]` against `samples[i]` without this offset compares
        // samples that were never meant to line up and reports spurious
        // near-total decorrelation on a fast-changing signal like a 1kHz
        // tone, even from a bit-correct codec.
        const CODEC_DELAY: usize = 22;
        // Also skip the first 200 samples of the *delay-compensated* signal:
        // ADPCM predictors start at zero and need a short run-in before
        // scale factors converge.
        let skip = 200;
        let signal: Vec<f64> = samples[skip..1600 - CODEC_DELAY]
            .iter()
            .map(|&s| s as f64)
            .collect();
        let recon: Vec<f64> = decoded[skip + CODEC_DELAY..1600]
            .iter()
            .map(|&s| s as f64)
            .collect();

        let signal_energy: f64 = signal.iter().map(|s| s * s).sum();
        let error_energy: f64 = signal
            .iter()
            .zip(recon.iter())
            .map(|(a, b)| (a - b) * (a - b))
            .sum();
        let snr_db = 10.0 * (signal_energy / error_energy.max(1.0)).log10();
        assert!(
            snr_db > 20.0,
            "expected a clean tone to round-trip with SNR > 20dB, got {snr_db:.1}dB"
        );
    }

    #[test]
    fn silence_round_trips_to_silence() {
        let mut enc = G722State::new();
        let mut dec = G722State::new();
        let samples = vec![0i16; 320];
        let mut bytes = Vec::new();
        encode(&mut enc, &samples, &mut bytes);
        let mut decoded = Vec::new();
        decode(&mut dec, &bytes, &mut decoded);
        assert!(
            decoded.iter().all(|&s| s.abs() < 50),
            "silence should decode near-silent, got {decoded:?}"
        );
    }

    #[test]
    fn state_persists_and_matters_across_calls() {
        // Encoding in one big call vs. two small calls with persisted state
        // must produce the same bytes -- proof the predictor state is
        // actually threaded through, not reset per call.
        let samples = sine(320, 800.0, 16000.0, 6000.0);

        let mut enc_whole = G722State::new();
        let mut whole = Vec::new();
        encode(&mut enc_whole, &samples, &mut whole);

        let mut enc_split = G722State::new();
        let mut split = Vec::new();
        encode(&mut enc_split, &samples[..160], &mut split);
        encode(&mut enc_split, &samples[160..], &mut split);

        assert_eq!(whole, split);
    }

    /// The same delay-compensated comparison as the tone test above, on a
    /// constant (DC) input — the simplest possible signal, and a check that
    /// the reconstruction genuinely converges to the input level rather than
    /// merely resembling it.
    #[test]
    fn a_constant_signal_converges_to_its_input_level() {
        let mut enc = G722State::new();
        let mut dec = G722State::new();
        let samples: Vec<i16> = vec![4000i16; 200];
        let mut bytes = Vec::new();
        encode(&mut enc, &samples, &mut bytes);
        let mut decoded = Vec::new();
        decode(&mut dec, &bytes, &mut decoded);

        let tail = &decoded[100..];
        let mean: f64 = tail.iter().map(|&s| s as f64).sum::<f64>() / tail.len() as f64;
        assert!(
            (mean - 4000.0).abs() < 100.0,
            "expected steady-state reconstruction near 4000, got mean {mean:.1}"
        );
    }
}
