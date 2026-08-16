//! The verdict bundle for one call. Three axes — packet counts, signal
//! detection, loopback confirmation — are kept independent and never
//! collapsed into a single boolean (data-model.md CallReport).

use std::time::Duration;

use gsm_sip_bridge::ims::media_stats::{DirectionVerdict, ReceiveStats};
use serde::Serialize;

use crate::media::codec::CodecProfile;
use crate::media::session::{LevelStats, ToneStats};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequireLevel {
    Signalling,
    Packets,
    ToneLoopback,
}

impl std::str::FromStr for RequireLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "signalling" => Ok(Self::Signalling),
            "packets" => Ok(Self::Packets),
            "tone-loopback" => Ok(Self::ToneLoopback),
            other => Err(format!("unknown require level: {other}")),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SignallingTimings {
    pub invite_to_180_ms: Option<u64>,
    pub invite_to_200_ms: Option<u64>,
    pub answer_to_first_rtp_ms: Option<u64>,
    pub final_status: Option<u16>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LevelProfile {
    pub peak_dbfs: f64,
    pub mean_dbfs: f64,
    pub noise_floor_dbfs: f64,
    pub silent_frame_pct: f64,
}

impl From<&LevelStats> for LevelProfile {
    fn from(s: &LevelStats) -> Self {
        Self {
            peak_dbfs: s.peak_dbfs,
            mean_dbfs: s.mean_dbfs,
            noise_floor_dbfs: s.noise_floor_dbfs,
            silent_frame_pct: s.silent_frame_pct,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RoundTripStats {
    pub min_ms: u64,
    pub median_ms: u64,
    pub max_ms: u64,
    pub samples: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToneReport {
    pub plan: &'static str,
    pub tx_symbols_sent: u64,
    pub rx_symbols_detected: u64,
    pub rx_symbol_error_pct: f64,
    pub detected: bool,
    pub first_detected_ms_after_answer: Option<u64>,
    pub round_trip_delay_ms: Option<RoundTripStats>,
}

impl From<&ToneStats> for ToneReport {
    fn from(s: &ToneStats) -> Self {
        let rx_symbol_error_pct = if s.expected_symbols == 0 {
            0.0
        } else {
            (1.0 - (s.rx_symbols_detected as f64 / s.expected_symbols as f64)).clamp(0.0, 1.0)
                * 100.0
        };
        let round_trip_delay_ms = if s.rtt_samples_ms.is_empty() {
            None
        } else {
            let mut sorted = s.rtt_samples_ms.clone();
            sorted.sort_unstable();
            Some(RoundTripStats {
                min_ms: *sorted.first().unwrap(),
                median_ms: sorted[sorted.len() / 2],
                max_ms: *sorted.last().unwrap(),
                samples: sorted.len() as u64,
            })
        };
        Self {
            plan: "grid8",
            tx_symbols_sent: s.tx_symbols_sent,
            rx_symbols_detected: s.rx_symbols_detected,
            rx_symbol_error_pct,
            detected: s.rx_symbols_detected > 0,
            first_detected_ms_after_answer: s.first_detected_ms_after_start,
            round_trip_delay_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaCounters {
    pub codec: &'static str,
    pub payload_type: u8,
    pub rtp_clock_hz: u32,
    pub audio_hz: u32,
    pub sent_packets: u64,
    pub sent_samples: u64,
    pub received_packets: u64,
    pub lost_packets: u64,
    pub loss_percent: f64,
    pub reordered_packets: u64,
    pub jitter_ms: f64,
    pub rx_level: Option<LevelProfile>,
    pub tone: Option<ToneReport>,
}

impl MediaCounters {
    pub fn new(codec: CodecProfile, sent_packets: u64, stats: &ReceiveStats) -> Self {
        Self {
            codec: codec.rtpmap,
            payload_type: codec.pt,
            rtp_clock_hz: codec.rtp_clock_hz,
            audio_hz: codec.audio_hz,
            sent_packets,
            sent_samples: sent_packets * codec.samples_per_frame as u64,
            received_packets: stats.received_packets,
            lost_packets: stats.lost_packets,
            loss_percent: stats.loss_percent(),
            reordered_packets: stats.reordered_packets,
            jitter_ms: stats.jitter.as_secs_f64() * 1000.0,
            rx_level: None,
            tone: None,
        }
    }

    /// Attaches the level profile and tone report from a media session that
    /// ran with the tone plan enabled (US4). Left unattached (`None`) for
    /// silent-placeholder sessions and for calls that never reached media.
    pub fn with_tone_and_level(mut self, level: &LevelStats, tone: &ToneStats) -> Self {
        self.rx_level = Some(level.into());
        self.tone = Some(tone.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RxAudioVerdict {
    Silent,
    NoiseOnly,
    ToneDetected,
    SpeechOrOther,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopbackVerdict {
    Confirmed,
    NotConfirmed,
}

#[derive(Debug, Clone, Serialize)]
pub struct Verdicts {
    pub packets: PacketsVerdict,
    pub rx_audio: RxAudioVerdict,
    pub loopback: LoopbackVerdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PacketsVerdict {
    BothWays,
    SendOnly,
    ReceiveOnly,
    Neither,
}

impl From<DirectionVerdict> for PacketsVerdict {
    fn from(v: DirectionVerdict) -> Self {
        match v {
            DirectionVerdict::BothWays => Self::BothWays,
            DirectionVerdict::SendOnly => Self::SendOnly,
            DirectionVerdict::ReceiveOnly => Self::ReceiveOnly,
            DirectionVerdict::Neither => Self::Neither,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Recordings {
    pub received: Option<String>,
    pub sent: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CallReport {
    pub signalling: SignallingTimings,
    pub media: MediaCounters,
    pub verdicts: Verdicts,
    pub success: bool,
    pub recordings: Recordings,
}

impl CallReport {
    pub fn build(
        signalling: SignallingTimings,
        media: MediaCounters,
        answered: bool,
        packets: DirectionVerdict,
        require: RequireLevel,
        recordings: Recordings,
    ) -> Self {
        let rx_audio = rx_audio_verdict(&media);
        let loopback = media
            .tone
            .as_ref()
            .filter(|t| t.detected && t.round_trip_delay_ms.is_some())
            .map(|_| LoopbackVerdict::Confirmed)
            .unwrap_or(LoopbackVerdict::NotConfirmed);
        let verdicts = Verdicts {
            packets: packets.into(),
            rx_audio,
            loopback,
        };
        let success = match require {
            RequireLevel::Signalling => answered,
            RequireLevel::Packets => answered && packets.is_success(),
            RequireLevel::ToneLoopback => {
                answered && packets.is_success() && verdicts.loopback == LoopbackVerdict::Confirmed
            }
        };
        Self {
            signalling,
            media,
            verdicts,
            success,
            recordings,
        }
    }

    pub fn render_text(&self, call_id: &str) -> String {
        let tone_line = match &self.media.tone {
            Some(t) => format!(
                "  tone           : {} (rx {} of ~{} symbols, {:.1}% error){}\n",
                if t.detected {
                    "detected"
                } else {
                    "not detected"
                },
                t.rx_symbols_detected,
                self.media.sent_packets, // best-effort context; exact expected count lives server-side
                t.rx_symbol_error_pct,
                match &t.round_trip_delay_ms {
                    Some(r) => format!(
                        ", rtt median {}ms (min {}, max {})",
                        r.median_ms, r.min_ms, r.max_ms
                    ),
                    None => String::new(),
                }
            ),
            None => String::new(),
        };
        format!(
            "\ncall report ({call_id})\n  direction      : {}\n  sent           : {} packets / {} samples\n  received       : {} packets\n  loss           : {} ({:.1}%)\n  reordered      : {}\n  jitter         : {:.1} ms\n{tone_line}  success        : {}\n",
            match self.verdicts.packets {
                PacketsVerdict::BothWays => "both ways",
                PacketsVerdict::SendOnly => "send-only — we transmitted but little or nothing came back",
                PacketsVerdict::ReceiveOnly => "receive-only — audio arrived but little of ours got out",
                PacketsVerdict::Neither => "neither — media never established",
            },
            self.media.sent_packets,
            self.media.sent_samples,
            self.media.received_packets,
            self.media.lost_packets,
            self.media.loss_percent,
            self.media.reordered_packets,
            self.media.jitter_ms,
            self.success,
        )
    }
}

/// "nothing arrived" and "something arrived that wasn't ours" must stay
/// distinguishable (FR-022) — this is the one place that distinction is
/// decided, from the level profile and (when the tone plan ran) detection.
fn rx_audio_verdict(media: &MediaCounters) -> RxAudioVerdict {
    let Some(level) = &media.rx_level else {
        return RxAudioVerdict::SpeechOrOther; // tone plan was off; no basis to judge
    };
    if level.silent_frame_pct >= 95.0 {
        return RxAudioVerdict::Silent;
    }
    match &media.tone {
        Some(t) if t.detected => RxAudioVerdict::ToneDetected,
        Some(_) => RxAudioVerdict::NoiseOnly,
        None => RxAudioVerdict::SpeechOrOther,
    }
}

pub fn duration_ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> ReceiveStats {
        ReceiveStats {
            received_packets: 100,
            lost_packets: 0,
            reordered_packets: 0,
            jitter: Duration::from_millis(2),
        }
    }

    #[test]
    fn silent_and_noise_only_and_tone_detected_stay_distinguishable() {
        let codec = crate::media::codec::PCMU;
        let base = MediaCounters::new(codec, 100, &stats());

        let silent_level = LevelStats {
            peak_dbfs: -80.0,
            mean_dbfs: -80.0,
            noise_floor_dbfs: -80.0,
            silent_frame_pct: 99.0,
        };
        let no_tone = ToneStats {
            expected_symbols: 10,
            ..Default::default()
        };
        let silent = base.clone().with_tone_and_level(&silent_level, &no_tone);
        assert_eq!(rx_audio_verdict(&silent), RxAudioVerdict::Silent);

        let noisy_level = LevelStats {
            peak_dbfs: -20.0,
            mean_dbfs: -30.0,
            noise_floor_dbfs: -40.0,
            silent_frame_pct: 5.0,
        };
        let noisy = base.clone().with_tone_and_level(&noisy_level, &no_tone);
        assert_eq!(rx_audio_verdict(&noisy), RxAudioVerdict::NoiseOnly);

        let tone_stats = ToneStats {
            rx_symbols_detected: 20,
            expected_symbols: 20,
            tx_symbols_sent: 20,
            rtt_samples_ms: vec![30, 32, 35],
            first_detected_ms_after_start: Some(120),
        };
        let with_tone = base.with_tone_and_level(&noisy_level, &tone_stats);
        assert_eq!(rx_audio_verdict(&with_tone), RxAudioVerdict::ToneDetected);
    }

    #[test]
    fn loopback_is_confirmed_only_when_a_round_trip_was_actually_measured() {
        let codec = crate::media::codec::PCMU;
        let level = LevelStats {
            peak_dbfs: -20.0,
            mean_dbfs: -25.0,
            noise_floor_dbfs: -40.0,
            silent_frame_pct: 5.0,
        };

        let never_returned = ToneStats {
            expected_symbols: 10,
            ..Default::default()
        };
        let media =
            MediaCounters::new(codec, 100, &stats()).with_tone_and_level(&level, &never_returned);
        let report = CallReport::build(
            SignallingTimings {
                invite_to_180_ms: None,
                invite_to_200_ms: None,
                answer_to_first_rtp_ms: None,
                final_status: Some(200),
            },
            media,
            true,
            DirectionVerdict::BothWays,
            RequireLevel::Packets,
            Recordings {
                received: None,
                sent: None,
            },
        );
        assert_eq!(report.verdicts.loopback, LoopbackVerdict::NotConfirmed);
        assert!(
            report.success,
            "packets require level must not be gated on loopback"
        );

        let looped = ToneStats {
            rx_symbols_detected: 5,
            expected_symbols: 10,
            tx_symbols_sent: 10,
            rtt_samples_ms: vec![40, 42],
            first_detected_ms_after_start: Some(80),
        };
        let media2 = MediaCounters::new(codec, 100, &stats()).with_tone_and_level(&level, &looped);
        let report2 = CallReport::build(
            SignallingTimings {
                invite_to_180_ms: None,
                invite_to_200_ms: None,
                answer_to_first_rtp_ms: None,
                final_status: Some(200),
            },
            media2,
            true,
            DirectionVerdict::BothWays,
            RequireLevel::ToneLoopback,
            Recordings {
                received: None,
                sent: None,
            },
        );
        assert_eq!(report2.verdicts.loopback, LoopbackVerdict::Confirmed);
        assert!(report2.success);
    }
}
