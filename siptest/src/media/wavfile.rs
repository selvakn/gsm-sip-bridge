//! Reading a WAV file to transmit as call audio (`[media].play_file`).
//!
//! Deliberately minimal: 16-bit PCM, mono, any sample rate — the shape
//! `ffmpeg -ar 8000 -ac 1 -c:a pcm_s16le` produces, and the same shape
//! [`gsm_sip_bridge::ims::rtp::WavWriter`] writes, so a recording this tool
//! made can be played straight back. Anything else is rejected by name rather
//! than mis-decoded into noise on a live call.
//!
//! The transmit stream must stay a pure function of the frame index (see
//! `media::session`'s independence properties), which prerecorded audio is:
//! the samples are read once, up front, and indexed by frame thereafter.

use std::path::Path;

use crate::error::{SipTestError, SipTestResult};

/// 16-bit mono PCM and the rate it was sampled at.
#[derive(Debug)]
pub struct WavAudio {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
}

fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Reads `path` as 16-bit mono PCM.
///
/// Walks the RIFF chunk list rather than assuming `fmt ` and `data` sit at
/// fixed offsets: ffmpeg writes a `LIST`/`INFO` chunk between them, so the
/// fixed-offset shortcut would read the encoder's name as audio.
pub fn read(path: &Path) -> SipTestResult<WavAudio> {
    let bytes = std::fs::read(path)
        .map_err(|e| SipTestError::Config(format!("cannot read {}: {e}", path.display())))?;
    let bad = |what: String| SipTestError::Config(format!("{}: {what}", path.display()));

    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(bad("not a RIFF/WAVE file".into()));
    }

    let mut pos = 12;
    let mut sample_rate = None;
    let mut samples = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32_at(&bytes, pos + 4) as usize;
        let body = pos + 8;
        let end = body.saturating_add(len).min(bytes.len());
        match id {
            // `len >= 16` alone only proves the chunk *claims* 16 bytes; a
            // file truncated mid-chunk can still have fewer than that
            // actually present (`end` clamps to `bytes.len()`), and reading
            // past `end` would index out of bounds rather than report a
            // config error.
            b"fmt " if len >= 16 && end - body >= 16 => {
                let format = u16_at(&bytes, body);
                let channels = u16_at(&bytes, body + 2);
                let bits = u16_at(&bytes, body + 14);
                // 1 = PCM; 0xFFFE (WAVE_FORMAT_EXTENSIBLE) carries the real
                // tag in a sub-format GUID we do not parse, so refuse it
                // rather than guess.
                if format != 1 {
                    return Err(bad(format!("not uncompressed PCM (format tag {format})")));
                }
                if channels != 1 {
                    return Err(bad(format!("{channels} channels; mono only")));
                }
                if bits != 16 {
                    return Err(bad(format!("{bits}-bit samples; 16-bit only")));
                }
                let rate = u32_at(&bytes, body + 4);
                if rate == 0 {
                    return Err(bad("sample rate must be greater than zero".into()));
                }
                sample_rate = Some(rate);
            }
            b"data" => {
                samples = Some(
                    bytes[body..end]
                        .chunks_exact(2)
                        .map(|c| i16::from_le_bytes([c[0], c[1]]))
                        .collect::<Vec<i16>>(),
                );
            }
            _ => {}
        }
        // Chunks are word-aligned: an odd length is followed by a pad byte
        // that is not counted in the length.
        pos = body + len + (len & 1);
    }

    match (sample_rate, samples) {
        (Some(sample_rate), Some(samples)) if !samples.is_empty() => Ok(WavAudio {
            samples,
            sample_rate,
        }),
        (None, _) => Err(bad("no fmt chunk".into())),
        (_, None) => Err(bad("no data chunk".into())),
        _ => Err(bad("data chunk is empty".into())),
    }
}

/// Linearly resamples to `target_hz`. A no-op at a matching rate, which is
/// the case worth keeping exact — resampling 8 kHz speech to 8 kHz through
/// the interpolator would still soften it slightly.
pub fn resample(audio: &WavAudio, target_hz: u32) -> Vec<i16> {
    if audio.sample_rate == target_hz || audio.samples.len() < 2 {
        return audio.samples.clone();
    }
    let ratio = audio.sample_rate as f64 / target_hz as f64;
    let out_len = ((audio.samples.len() as f64) / ratio).floor() as usize;
    (0..out_len)
        .map(|i| {
            let src = i as f64 * ratio;
            let lo = src.floor() as usize;
            let hi = (lo + 1).min(audio.samples.len() - 1);
            let frac = src - lo as f64;
            let a = audio.samples[lo] as f64;
            let b = audio.samples[hi] as f64;
            (a + (b - a) * frac).round() as i16
        })
        .collect()
}

/// Loads `path` and resamples it to `target_hz` in one step — what a caller
/// building a media session actually wants.
pub fn load_for(path: &Path, target_hz: u32) -> SipTestResult<Vec<i16>> {
    let audio = read(path)?;
    Ok(resample(&audio, target_hz))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(sample_rate: u32, samples: &[i16], extra_chunk: bool) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1u16.to_le_bytes()); // PCM
        fmt.extend_from_slice(&1u16.to_le_bytes()); // mono
        fmt.extend_from_slice(&sample_rate.to_le_bytes());
        fmt.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
        fmt.extend_from_slice(&2u16.to_le_bytes()); // block align
        fmt.extend_from_slice(&16u16.to_le_bytes()); // bits

        let mut body = Vec::new();
        body.extend_from_slice(b"WAVE");
        body.extend_from_slice(b"fmt ");
        body.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        body.extend_from_slice(&fmt);
        if extra_chunk {
            // What ffmpeg puts between `fmt ` and `data`.
            body.extend_from_slice(b"LIST");
            body.extend_from_slice(&10u32.to_le_bytes());
            body.extend_from_slice(b"INFOISFTx\0");
        }
        body.extend_from_slice(b"data");
        body.extend_from_slice(&(data.len() as u32).to_le_bytes());
        body.extend_from_slice(&data);

        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    fn write_temp(bytes: &[u8], name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn reads_16_bit_mono_pcm() {
        let path = write_temp(
            &wav(8000, &[0, 100, -100, 32767], false),
            "siptest-plain.wav",
        );
        let audio = read(&path).unwrap();
        assert_eq!(audio.sample_rate, 8000);
        assert_eq!(audio.samples, vec![0, 100, -100, 32767]);
    }

    /// ffmpeg writes a `LIST`/`INFO` chunk between `fmt ` and `data`. Reading
    /// `data` from a fixed offset would take that metadata for audio and play
    /// the encoder's name down the line as noise.
    #[test]
    fn skips_chunks_between_fmt_and_data() {
        let path = write_temp(&wav(8000, &[1, 2, 3, 4], true), "siptest-list.wav");
        let audio = read(&path).unwrap();
        assert_eq!(audio.samples, vec![1, 2, 3, 4]);
    }

    #[test]
    fn rejects_what_it_cannot_decode() {
        let mut stereo = wav(8000, &[1, 2], false);
        stereo[22] = 2; // channels = 2
        let path = write_temp(&stereo, "siptest-stereo.wav");
        let err = read(&path).unwrap_err().to_string();
        assert!(err.contains("channels"), "{err}");
    }

    /// A `fmt ` chunk header can claim 16 bytes (`len >= 16`) while the file
    /// is truncated before those bytes actually exist — `end` clamps to
    /// `bytes.len()`, so the claimed length alone is not proof the bytes are
    /// there. Reading past `end` would index out of bounds and panic; this
    /// must report a config error instead.
    #[test]
    fn a_fmt_chunk_truncated_before_its_claimed_length_is_a_config_error_not_a_panic() {
        let mut full = wav(8000, &[1, 2], false);
        // The `fmt ` chunk starts at byte 20 (RIFF header 12 + "fmt " 4 +
        // length 4) and claims 16 bytes; keep only the first 10 of them, then
        // drop everything after (the `data` chunk included).
        full.truncate(20 + 10);
        let path = write_temp(&full, "siptest-truncated-fmt.wav");
        let err = read(&path).unwrap_err().to_string();
        assert!(
            err.contains("no fmt chunk") || err.contains("no data chunk"),
            "{err}"
        );
    }

    /// A zero sample rate must be rejected at parse time, not accepted and
    /// left to blow up `resample`'s division later — `out_len` would compute
    /// as `usize::MAX` and the allocation would panic mid-call instead of
    /// failing at startup with a clear cause.
    #[test]
    fn a_zero_sample_rate_is_a_config_error() {
        let path = write_temp(&wav(0, &[1, 2, 3], false), "siptest-zero-rate.wav");
        let err = read(&path).unwrap_err().to_string();
        assert!(err.contains("sample rate"), "{err}");
    }

    #[test]
    fn resampling_a_matching_rate_is_exact() {
        let audio = WavAudio {
            samples: vec![1, 2, 3],
            sample_rate: 8000,
        };
        assert_eq!(resample(&audio, 8000), vec![1, 2, 3]);
    }

    /// Halving the rate halves the sample count and keeps the endpoints —
    /// enough to catch an inverted ratio, which would stretch a 5-second
    /// message into 10 seconds of half-pitch audio.
    #[test]
    fn resampling_down_halves_the_length() {
        let audio = WavAudio {
            samples: (0..100).map(|i| i as i16).collect(),
            sample_rate: 16000,
        };
        let out = resample(&audio, 8000);
        assert_eq!(out.len(), 50);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], 2);
    }
}
