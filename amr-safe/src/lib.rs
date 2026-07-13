//! Safe wrapper around the AMR-WB encoder/decoder FFI in `amr-sys`. Every
//! `unsafe` call site into that crate lives here, mirroring this
//! workspace's `pjsua-sys`/`pjsua-safe` split (see `gsm-sip-bridge`'s
//! zero-`unsafe` policy, enforced by `tools/count-unsafe.sh`).
//!
//! Gated behind the `amr-linked` feature (default off, same convention as
//! `pjsua-safe`'s `pjsip-linked`) so the workspace keeps building in
//! environments without `libvo-amrwbenc`/`libopencore-amrwb` installed —
//! see `docker/Dockerfile` for where a linked build installs them.
//!
//! Unlike `pjsua-safe`'s stubs (which return dummy *success* so daemon unit
//! tests don't need a real PJSIP), encode/decode here return a clear `Err`
//! when unlinked: this crate underpins a live call's actual audio, where a
//! stub that silently "succeeds" with garbage/zeroed samples would be
//! actively misleading rather than merely inert.

use std::fmt;

/// 20ms of audio at AMR-WB's fixed 16kHz sample rate — the frame size both
/// `E_IF_encode` and `D_IF_decode` operate on.
pub const FRAME_SAMPLES: usize = 320;
pub const SAMPLE_RATE: u32 = 16000;
/// The largest possible encoded frame (mode 8, 23.85kbps) is 61 bytes
/// (1 TOC/header byte + 60 bytes of packed speech data) — confirmed
/// empirically against the real library, not assumed from the spec.
#[cfg(feature = "amr-linked")]
const MAX_FRAME_BYTES: usize = 64;

/// AMR-WB's 9 codec modes (TS 26.201), used both as the encoder's rate
/// selector and as the `FT` (frame type) field decoded from a received
/// frame's TOC byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    R660 = 0,
    R885 = 1,
    R1265 = 2,
    R1425 = 3,
    R1585 = 4,
    R1825 = 5,
    R1985 = 6,
    R2305 = 7,
    R2385 = 8,
}

/// 20ms of audio at AMR-**narrowband**'s fixed 8kHz sample rate.
pub const NB_FRAME_SAMPLES: usize = 160;
pub const NB_SAMPLE_RATE: u32 = 8000;
/// Mode 7 (12.2kbps) is the largest AMR-NB frame: 1 header byte + 31 bytes.
#[cfg(feature = "amr-linked")]
const NB_MAX_FRAME_BYTES: usize = 32;

/// AMR-NB's 8 speech modes (TS 26.101). Distinct from AMR-WB's `Mode`: the
/// same numeric index means a different bit rate *and* a different frame bit
/// count in each codec, so mixing them up would produce frames the far end
/// silently mis-decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NbMode {
    R475 = 0,
    R515 = 1,
    R590 = 2,
    R670 = 3,
    R740 = 4,
    R795 = 5,
    R1020 = 6,
    R1220 = 7,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotLinked;

impl fmt::Display for NotLinked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "amr-safe was built without the amr-linked feature (libvo-amrwbenc/libopencore-amrwb not available)"
        )
    }
}
impl std::error::Error for NotLinked {}

pub fn is_available() -> bool {
    cfg!(feature = "amr-linked")
}

pub struct WbEncoder {
    #[cfg(feature = "amr-linked")]
    state: *mut std::os::raw::c_void,
}

// SAFETY: the underlying C library's per-instance state is a self-contained
// struct with no thread-locals or shared globals (it's designed for
// reentrant multi-instance use, e.g. by transcoding tools like ffmpeg) — it
// just must not be touched from two threads *simultaneously*, which we
// don't do (one encoder instance is used from exactly one thread at a
// time in ims::call).
#[cfg(feature = "amr-linked")]
unsafe impl Send for WbEncoder {}

impl WbEncoder {
    #[cfg(feature = "amr-linked")]
    pub fn new() -> Result<Self, NotLinked> {
        // SAFETY: E_IF_init takes no arguments and returns an opaque handle
        // owned exclusively by this WbEncoder until Drop.
        let state = unsafe { amr_sys::E_IF_init() };
        Ok(Self { state })
    }

    #[cfg(not(feature = "amr-linked"))]
    pub fn new() -> Result<Self, NotLinked> {
        Err(NotLinked)
    }

    /// Encodes one 20ms frame at the given mode, returning the TOC-prefixed
    /// encoded bytes (the same layout as one RFC 4867 octet-aligned ToC
    /// entry + frame data — see `ims::amr` for the RTP payload framing that
    /// builds on this directly).
    #[cfg(feature = "amr-linked")]
    pub fn encode(&mut self, mode: Mode, pcm: &[i16; FRAME_SAMPLES]) -> Vec<u8> {
        let mut out = [0u8; MAX_FRAME_BYTES];
        // SAFETY: `self.state` is a valid, exclusively-owned encoder handle;
        // `pcm` has exactly FRAME_SAMPLES elements as E_IF_encode requires;
        // `out` has room for the largest possible frame (confirmed above).
        let n = unsafe {
            amr_sys::E_IF_encode(self.state, mode as i32, pcm.as_ptr(), out.as_mut_ptr(), 0)
        };
        out[..n as usize].to_vec()
    }

    #[cfg(not(feature = "amr-linked"))]
    pub fn encode(&mut self, _mode: Mode, _pcm: &[i16; FRAME_SAMPLES]) -> Vec<u8> {
        unreachable!("WbEncoder::new() always errors when amr-linked is disabled")
    }
}

#[cfg(feature = "amr-linked")]
impl Drop for WbEncoder {
    fn drop(&mut self) {
        // SAFETY: `self.state` was created by E_IF_init in `new()` and is
        // exclusively owned by this instance; dropped exactly once.
        unsafe { amr_sys::E_IF_exit(self.state) };
    }
}

pub struct WbDecoder {
    #[cfg(feature = "amr-linked")]
    state: *mut std::os::raw::c_void,
}

// SAFETY: see WbEncoder's Send impl — same reasoning applies.
#[cfg(feature = "amr-linked")]
unsafe impl Send for WbDecoder {}

impl WbDecoder {
    #[cfg(feature = "amr-linked")]
    pub fn new() -> Result<Self, NotLinked> {
        // SAFETY: D_IF_init takes no arguments and returns an opaque handle
        // owned exclusively by this WbDecoder until Drop.
        let state = unsafe { amr_sys::D_IF_init() };
        Ok(Self { state })
    }

    #[cfg(not(feature = "amr-linked"))]
    pub fn new() -> Result<Self, NotLinked> {
        Err(NotLinked)
    }

    /// Decodes one TOC-prefixed frame (see `encode`'s doc comment) to 320
    /// samples (20ms @ 16kHz) of linear PCM.
    #[cfg(feature = "amr-linked")]
    pub fn decode(&mut self, toc_and_data: &[u8]) -> [i16; FRAME_SAMPLES] {
        let mut synth = [0i16; FRAME_SAMPLES];
        // SAFETY: `self.state` is a valid, exclusively-owned decoder handle;
        // `toc_and_data` is a valid byte slice the library reads up to the
        // frame length encoded in its own TOC byte; `synth` has exactly
        // FRAME_SAMPLES elements as D_IF_decode requires.
        unsafe {
            amr_sys::D_IF_decode(
                self.state,
                toc_and_data.as_ptr(),
                synth.as_mut_ptr(),
                0, // bfi: 0 = good frame
            )
        };
        synth
    }

    #[cfg(not(feature = "amr-linked"))]
    pub fn decode(&mut self, _toc_and_data: &[u8]) -> [i16; FRAME_SAMPLES] {
        unreachable!("WbDecoder::new() always errors when amr-linked is disabled")
    }
}

#[cfg(feature = "amr-linked")]
impl Drop for WbDecoder {
    fn drop(&mut self) {
        // SAFETY: `self.state` was created by D_IF_init in `new()` and is
        // exclusively owned by this instance; dropped exactly once.
        unsafe { amr_sys::D_IF_exit(self.state) };
    }
}

pub struct NbEncoder {
    #[cfg(feature = "amr-linked")]
    state: *mut std::os::raw::c_void,
}

// SAFETY: see WbEncoder's Send impl — opencore-amrnb's state is likewise a
// self-contained, reentrant per-instance struct.
#[cfg(feature = "amr-linked")]
unsafe impl Send for NbEncoder {}

impl NbEncoder {
    #[cfg(feature = "amr-linked")]
    pub fn new() -> Result<Self, NotLinked> {
        // SAFETY: Encoder_Interface_init returns an opaque handle owned
        // exclusively by this NbEncoder until Drop. dtx=0: no discontinuous
        // transmission, so every frame is a speech frame.
        let state = unsafe { amr_sys::Encoder_Interface_init(0) };
        Ok(Self { state })
    }

    #[cfg(not(feature = "amr-linked"))]
    pub fn new() -> Result<Self, NotLinked> {
        Err(NotLinked)
    }

    /// Encodes one 20ms frame, returning the header-byte-prefixed frame (the
    /// same layout as an RFC 4867 octet-aligned ToC entry + frame data — see
    /// `ims::amr_rtp`, which repacks it for whichever framing was negotiated).
    #[cfg(feature = "amr-linked")]
    pub fn encode(&mut self, mode: NbMode, pcm: &[i16; NB_FRAME_SAMPLES]) -> Vec<u8> {
        let mut out = [0u8; NB_MAX_FRAME_BYTES];
        // SAFETY: `self.state` is a valid, exclusively-owned encoder handle;
        // `pcm` has exactly NB_FRAME_SAMPLES elements as the library requires;
        // `out` has room for the largest possible frame. force_speech=1 keeps
        // it from emitting comfort-noise frames we'd have to special-case.
        let n = unsafe {
            amr_sys::Encoder_Interface_Encode(
                self.state,
                mode as i32,
                pcm.as_ptr(),
                out.as_mut_ptr(),
                1,
            )
        };
        out[..n.max(0) as usize].to_vec()
    }

    #[cfg(not(feature = "amr-linked"))]
    pub fn encode(&mut self, _mode: NbMode, _pcm: &[i16; NB_FRAME_SAMPLES]) -> Vec<u8> {
        unreachable!("NbEncoder::new() always errors when amr-linked is disabled")
    }
}

#[cfg(feature = "amr-linked")]
impl Drop for NbEncoder {
    fn drop(&mut self) {
        // SAFETY: created by Encoder_Interface_init in `new()`, exclusively
        // owned by this instance, dropped exactly once.
        unsafe { amr_sys::Encoder_Interface_exit(self.state) };
    }
}

pub struct NbDecoder {
    #[cfg(feature = "amr-linked")]
    state: *mut std::os::raw::c_void,
}

// SAFETY: see WbEncoder's Send impl — same reasoning.
#[cfg(feature = "amr-linked")]
unsafe impl Send for NbDecoder {}

impl NbDecoder {
    #[cfg(feature = "amr-linked")]
    pub fn new() -> Result<Self, NotLinked> {
        // SAFETY: Decoder_Interface_init returns an opaque handle owned
        // exclusively by this NbDecoder until Drop.
        let state = unsafe { amr_sys::Decoder_Interface_init() };
        Ok(Self { state })
    }

    #[cfg(not(feature = "amr-linked"))]
    pub fn new() -> Result<Self, NotLinked> {
        Err(NotLinked)
    }

    /// Decodes one header-byte-prefixed frame (see `NbEncoder::encode`) to
    /// 160 samples (20ms @ 8kHz) of linear PCM.
    #[cfg(feature = "amr-linked")]
    pub fn decode(&mut self, toc_and_data: &[u8]) -> [i16; NB_FRAME_SAMPLES] {
        let mut synth = [0i16; NB_FRAME_SAMPLES];
        // SAFETY: `self.state` is a valid, exclusively-owned decoder handle;
        // the library reads `toc_and_data` up to the frame length encoded in
        // its own header byte; `synth` has exactly NB_FRAME_SAMPLES elements.
        unsafe {
            amr_sys::Decoder_Interface_Decode(
                self.state,
                toc_and_data.as_ptr(),
                synth.as_mut_ptr(),
                0, // bfi: 0 = good frame
            )
        };
        synth
    }

    #[cfg(not(feature = "amr-linked"))]
    pub fn decode(&mut self, _toc_and_data: &[u8]) -> [i16; NB_FRAME_SAMPLES] {
        unreachable!("NbDecoder::new() always errors when amr-linked is disabled")
    }
}

#[cfg(feature = "amr-linked")]
impl Drop for NbDecoder {
    fn drop(&mut self) {
        // SAFETY: created by Decoder_Interface_init in `new()`, exclusively
        // owned by this instance, dropped exactly once.
        unsafe { amr_sys::Decoder_Interface_exit(self.state) };
    }
}

#[cfg(all(test, feature = "amr-linked"))]
mod tests {
    use super::*;

    #[test]
    fn nb_encode_produces_a_header_prefixed_frame_of_the_documented_size() {
        let mut enc = NbEncoder::new().unwrap();
        let pcm = [0i16; NB_FRAME_SAMPLES];
        let frame = enc.encode(NbMode::R1220, &pcm);
        // Mode 7 (12.2kbps): 1 header byte + 31 bytes of speech.
        assert_eq!(frame.len(), 32);
        // Header byte: F=0, FT=7 (bits 6-3), Q=1 (bit 2).
        assert_eq!((frame[0] >> 3) & 0x0f, 7);
    }

    #[test]
    fn nb_encode_then_decode_roundtrips_without_panicking() {
        let mut enc = NbEncoder::new().unwrap();
        let mut dec = NbDecoder::new().unwrap();
        let pcm = [0i16; NB_FRAME_SAMPLES];
        let frame = enc.encode(NbMode::R1220, &pcm);
        let synth = dec.decode(&frame);
        assert_eq!(synth.len(), NB_FRAME_SAMPLES);
    }
}

#[cfg(all(test, feature = "amr-linked"))]
mod wb_tests {
    use super::*;

    #[test]
    fn encode_produces_a_toc_prefixed_frame_of_plausible_size() {
        let mut enc = WbEncoder::new().unwrap();
        let pcm = [0i16; FRAME_SAMPLES];
        let frame = enc.encode(Mode::R2385, &pcm);
        // Mode 8 (23.85kbps) is the largest frame: 61 bytes, empirically
        // confirmed against the real library.
        assert_eq!(frame.len(), 61);
        // TOC byte: F=0, FT=8 (bits 6-3), Q=1 (bit 2), padding=00.
        assert_eq!((frame[0] >> 3) & 0x0f, 8);
    }

    #[test]
    fn encode_then_decode_roundtrips_without_panicking() {
        let mut enc = WbEncoder::new().unwrap();
        let mut dec = WbDecoder::new().unwrap();
        let pcm = [0i16; FRAME_SAMPLES];
        let frame = enc.encode(Mode::R1265, &pcm);
        let synth = dec.decode(&frame);
        assert_eq!(synth.len(), FRAME_SAMPLES);
    }
}
