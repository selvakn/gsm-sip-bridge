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
