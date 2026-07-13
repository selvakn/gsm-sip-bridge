//! Raw FFI declarations for the AMR-WB encoder (VisualOn's `vo-amrwbenc`)
//! and decoder (`opencore-amrwb`) — the two small, ABI-stable C libraries
//! Debian packages for AMR-WB, since neither project alone provides both
//! directions (opencore-amrwb's encoder was stripped for patent reasons
//! years ago, leaving decode-only; vo-amrwbenc fills the encode side).
//!
//! Both APIs come from the same 3GPP reference code lineage and have been
//! unchanged for well over a decade, confirmed against the actual installed
//! headers (`/usr/include/vo-amrwbenc/enc_if.h`,
//! `/usr/include/opencore-amrwb/dec_if.h`) rather than assumed from memory —
//! small enough (3 functions each) that hand-writing beats bindgen here.
//!
//! No safe wrapper lives in this crate — see `amr-safe`, which is where
//! every `unsafe` call site into these functions lives, mirroring this
//! workspace's `pjsua-sys`/`pjsua-safe` split.

use std::os::raw::{c_int, c_uchar, c_void};

unsafe extern "C" {
    /// Allocates and initializes encoder state. Never returns null in
    /// practice (the reference implementation aborts internally on
    /// allocation failure) but treat a null return defensively regardless.
    pub fn E_IF_init() -> *mut c_void;

    /// Encodes 320 samples (20ms @ 16kHz) of 16-bit linear PCM at the given
    /// `mode` (0-8, see `amr-safe`'s `Mode` enum for the bit-rate mapping).
    /// `out` must have room for at least 61 bytes (the largest AMR-WB frame,
    /// mode 8 @ 23.85kbps, is 60 bytes of packed speech data + 1 TOC/header
    /// byte). Returns the number of bytes actually written to `out`.
    /// `dtx` (discontinuous transmission / comfort noise) is 0 or 1.
    pub fn E_IF_encode(
        state: *mut c_void,
        mode: c_int,
        speech: *const i16,
        out: *mut c_uchar,
        dtx: c_int,
    ) -> c_int;

    pub fn E_IF_exit(state: *mut c_void);

    pub fn D_IF_init() -> *mut c_void;

    /// Decodes one frame — `bits` is the TOC-prefixed encoded frame exactly
    /// as `E_IF_encode` produced it (this is also bit-for-bit the same
    /// layout as one RFC 4867 octet-aligned ToC entry + its frame data, see
    /// `ims::amr`), writing 320 samples (20ms @ 16kHz) of 16-bit linear PCM
    /// to `synth`. `bfi` (bad frame indicator) is 0 for a good frame.
    pub fn D_IF_decode(state: *mut c_void, bits: *const c_uchar, synth: *mut i16, bfi: c_int);

    pub fn D_IF_exit(state: *mut c_void);
}

// AMR **narrowband** (`opencore-amrnb`), which — unlike AMR-WB — ships both
// directions in one library. Needed because a carrier's mobile-terminating
// VoWiFi INVITE does not always offer AMR-WB: Airtel was observed offering
// `AMR/8000` alone on some calls, which is unanswerable without this.
//
// Same 3GPP reference lineage and the same MIME/storage frame layout as the
// wideband functions above (one ToC-style header byte, then packed speech),
// so `amr-safe` wraps both with near-identical code — only the frame size
// (160 samples @ 8kHz) and the per-mode bit counts differ.
// Declarations confirmed against `/usr/include/opencore-amrnb/interf_enc.h`
// and `interf_dec.h`.
unsafe extern "C" {
    /// `dtx`: 0 disables discontinuous transmission (we always send speech).
    pub fn Encoder_Interface_init(dtx: c_int) -> *mut c_void;

    /// Encodes 160 samples (20ms @ 8kHz) at `mode` (0-7, see `amr-safe`'s
    /// `NbMode`). `out` needs room for 32 bytes (mode 7 @ 12.2kbps, the
    /// largest: 1 header byte + 31 bytes of speech). `force_speech` = 1
    /// suppresses comfort-noise frames. Returns bytes written.
    pub fn Encoder_Interface_Encode(
        state: *mut c_void,
        mode: c_int,
        speech: *const i16,
        out: *mut c_uchar,
        force_speech: c_int,
    ) -> c_int;

    pub fn Encoder_Interface_exit(state: *mut c_void);

    pub fn Decoder_Interface_init() -> *mut c_void;

    /// Decodes one header-byte-prefixed frame to 160 samples (20ms @ 8kHz).
    /// `bfi` (bad frame indicator) is 0 for a good frame.
    pub fn Decoder_Interface_Decode(
        state: *mut c_void,
        bits: *const c_uchar,
        synth: *mut i16,
        bfi: c_int,
    );

    pub fn Decoder_Interface_exit(state: *mut c_void);
}
