//! The verdict bundle for one call. Three axes — packet counts, signal
//! detection, loopback confirmation — are kept independent and never
//! collapsed into a single boolean (data-model.md CallReport). The `rx_audio`
//! and `loopback` axes are filled in once tone detection lands (US4); until
//! then they read `SpeechOrOther`/`NotConfirmed` honestly rather than
//! pretending to a verdict this build cannot produce.

use std::time::Duration;

use gsm_sip_bridge::ims::media_stats::{DirectionVerdict, ReceiveStats};
use serde::Serialize;

use crate::media::codec::CodecProfile;

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
        }
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
        let verdicts = Verdicts {
            packets: packets.into(),
            // Filled in by US4; honest placeholders until then.
            rx_audio: RxAudioVerdict::SpeechOrOther,
            loopback: LoopbackVerdict::NotConfirmed,
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
        format!(
            "\ncall report ({call_id})\n  direction      : {}\n  sent           : {} packets / {} samples\n  received       : {} packets\n  loss           : {} ({:.1}%)\n  reordered      : {}\n  jitter         : {:.1} ms\n  success        : {}\n",
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

pub fn duration_ms(d: Duration) -> u64 {
    d.as_millis() as u64
}
