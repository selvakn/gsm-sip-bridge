//! Codec identity, kept in one struct so every consumer takes the field it
//! actually needs. This is what makes G.722 safe to add later: its
//! `a=rtpmap` says `G722/8000` (RFC 3551's historical error) but it carries
//! 16 kHz audio on an 8 kHz RTP clock. Getting that wrong is silent and
//! corrupts the very measurement this tool exists to make — so the two rates
//! are named separately here from the first codec, PCMU, where they happen to
//! coincide.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecProfile {
    pub pt: u8,
    pub rtpmap: &'static str,
    /// What the RTP timestamp counts in.
    pub rtp_clock_hz: u32,
    /// What the samples actually are — feeds `WavWriter`, the Goertzel bank,
    /// and the level meter.
    pub audio_hz: u32,
    pub samples_per_frame: usize,
    pub ts_increment: u32,
    pub bytes_per_frame: usize,
}

pub const PCMU: CodecProfile = CodecProfile {
    pt: 0,
    rtpmap: "PCMU/8000",
    rtp_clock_hz: 8000,
    audio_hz: 8000,
    samples_per_frame: 160,
    ts_increment: 160,
    bytes_per_frame: 160,
};

pub fn encode_pcmu(samples: &[i16]) -> Vec<u8> {
    samples
        .iter()
        .map(|&s| gsm_sip_bridge::ims::rtp::linear_to_ulaw(s))
        .collect()
}

pub fn decode_pcmu(payload: &[u8]) -> Vec<i16> {
    payload
        .iter()
        .map(|&b| gsm_sip_bridge::ims::rtp::ulaw_to_linear(b))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcmu_audio_rate_and_clock_rate_coincide() {
        assert_eq!(PCMU.rtp_clock_hz, PCMU.audio_hz);
        assert_eq!(PCMU.audio_hz, 8000);
        assert_eq!(PCMU.samples_per_frame, PCMU.ts_increment as usize);
        assert_eq!(PCMU.samples_per_frame, 160);
    }

    #[test]
    fn pcmu_round_trips_within_ulaw_quantisation_error() {
        let samples: Vec<i16> = (0..160).map(|i| ((i * 37) % 4000) as i16 - 2000).collect();
        let encoded = encode_pcmu(&samples);
        let decoded = decode_pcmu(&encoded);
        assert_eq!(decoded.len(), samples.len());
        for (orig, back) in samples.iter().zip(decoded.iter()) {
            assert!((*orig - *back).abs() < 200, "orig={orig} back={back}");
        }
    }
}
