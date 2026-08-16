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

/// The RTP clock/audio-rate mismatch this module's own doc comment warns
/// about, now real: `a=rtpmap:9 G722/8000` names the 8kHz clock, but the
/// audio underneath is 16kHz (`media/g722.rs`).
pub const G722: CodecProfile = CodecProfile {
    pt: 9,
    rtpmap: "G722/8000",
    rtp_clock_hz: 8000,
    audio_hz: 16000,
    samples_per_frame: 320,
    ts_increment: 160,
    bytes_per_frame: 160,
};

/// Resolves `[media].codec` / `--codec` / a `POST /calls` override to a
/// single definite profile for an **outbound** call, where siptest is the
/// one making the offer (contracts/sip-flows.md C-4: one codec per offer,
/// no multi-codec negotiation). `"auto"` prefers G.722 — the wideband path
/// Agent B actually prioritises (`G722/16000/1` at weight 200,
/// research.md R5) — over PCMU.
pub fn resolve_codec(name: &str) -> Result<CodecProfile, String> {
    match name {
        "auto" | "g722" => Ok(G722),
        "pcmu" => Ok(PCMU),
        other => Err(format!(
            "unknown codec: {other} (expected auto, pcmu, or g722)"
        )),
    }
}

/// Picks a codec for an **inbound** call, constrained by what the caller's
/// offer actually contains — `"auto"` still prefers G.722, but only if it
/// was offered; unlike the outbound side, we cannot make a peer offer
/// something it didn't.
pub fn select_inbound_codec(name: &str, offer: &crate::sdp::SdpOffer) -> Option<CodecProfile> {
    let try_g722 = || offer.offers(G722.pt).then_some(G722);
    let try_pcmu = || offer.offers(PCMU.pt).then_some(PCMU);
    match name {
        "pcmu" => try_pcmu(),
        "g722" => try_g722(),
        _ => try_g722().or_else(try_pcmu), // "auto" and any unrecognised value
    }
}

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

/// A codec instance holding whatever per-call state its algorithm needs.
/// PCMU is memoryless so its `encode`/`decode` ignore `self`; G.722's ADPCM
/// predictors are not — they must persist across the whole call, so this
/// trait (not a pair of free functions) is what makes G.722 safe to add:
/// the `CodecProfile` distinguishes the wire format, this distinguishes the
/// runtime behaviour.
pub trait CodecCoder: Send {
    fn encode(&mut self, samples: &[i16]) -> Vec<u8>;
    fn decode(&mut self, payload: &[u8]) -> Vec<i16>;
}

#[derive(Default)]
pub struct PcmuCoder;

impl CodecCoder for PcmuCoder {
    fn encode(&mut self, samples: &[i16]) -> Vec<u8> {
        encode_pcmu(samples)
    }
    fn decode(&mut self, payload: &[u8]) -> Vec<i16> {
        decode_pcmu(payload)
    }
}

#[derive(Default)]
pub struct G722Coder {
    state: crate::media::g722::G722State,
}

impl CodecCoder for G722Coder {
    fn encode(&mut self, samples: &[i16]) -> Vec<u8> {
        let mut out = Vec::new();
        crate::media::g722::encode(&mut self.state, samples, &mut out);
        out
    }
    fn decode(&mut self, payload: &[u8]) -> Vec<i16> {
        let mut out = Vec::new();
        crate::media::g722::decode(&mut self.state, payload, &mut out);
        out
    }
}

/// Builds a fresh, stateful coder matching `profile.pt` — one instance per
/// direction per call, never shared or reset mid-call.
pub fn new_coder(profile: CodecProfile) -> Box<dyn CodecCoder> {
    if profile.pt == G722.pt {
        Box::new(G722Coder::default())
    } else {
        Box::new(PcmuCoder)
    }
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

    /// The trap this whole module exists to name: G.722's clock and audio
    /// rates must differ, on purpose, everywhere downstream reads them.
    #[test]
    fn g722_audio_rate_and_clock_rate_deliberately_differ() {
        assert_eq!(G722.rtp_clock_hz, 8000);
        assert_eq!(G722.audio_hz, 16000);
        assert_ne!(G722.rtp_clock_hz, G722.audio_hz);
        assert_eq!(G722.samples_per_frame, 320);
        assert_eq!(G722.ts_increment, 160);
        assert_eq!(G722.bytes_per_frame, 160);
    }

    #[test]
    fn resolve_codec_prefers_g722_for_auto_and_honours_an_explicit_choice() {
        assert_eq!(resolve_codec("auto").unwrap().pt, G722.pt);
        assert_eq!(resolve_codec("g722").unwrap().pt, G722.pt);
        assert_eq!(resolve_codec("pcmu").unwrap().pt, PCMU.pt);
        assert!(resolve_codec("opus").is_err());
    }

    #[test]
    fn select_inbound_codec_prefers_g722_when_offered_but_falls_back_to_pcmu() {
        let both = crate::sdp::SdpOffer {
            remote_rtp: "127.0.0.1:1".parse().unwrap(),
            payload_types: vec![9, 0],
        };
        assert_eq!(select_inbound_codec("auto", &both).unwrap().pt, G722.pt);

        let pcmu_only = crate::sdp::SdpOffer {
            remote_rtp: "127.0.0.1:1".parse().unwrap(),
            payload_types: vec![0],
        };
        assert_eq!(
            select_inbound_codec("auto", &pcmu_only).unwrap().pt,
            PCMU.pt
        );
        assert!(select_inbound_codec("g722", &pcmu_only).is_none());
    }

    #[test]
    fn new_coder_selects_by_payload_type_and_persists_state_across_calls() {
        let mut pcmu = new_coder(PCMU);
        let samples = vec![1000i16; 160];
        let a = pcmu.encode(&samples);
        let b = pcmu.encode(&samples);
        assert_eq!(
            a, b,
            "PCMU is memoryless: identical input must encode identically"
        );

        let mut g722 = new_coder(G722);
        let s1 = vec![1000i16; 320];
        let s2 = vec![1000i16; 320];
        let g1 = g722.encode(&s1);
        let g2 = g722.encode(&s2);
        assert_ne!(
            g1, g2,
            "G.722's ADPCM predictor must carry state across calls, so identical \
             input encoded twice in a row should not produce identical bytes"
        );
    }
}
