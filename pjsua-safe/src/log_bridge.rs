use std::sync::atomic::{AtomicU64, Ordering};

/// Running count of ALSA capture overruns (GSM→SIP audio loss) seen in PJMEDIA logs.
static CAPTURE_OVERRUNS: AtomicU64 = AtomicU64::new(0);
/// Running count of ALSA playback underruns (SIP→GSM audio loss) seen in PJMEDIA logs.
static PLAYBACK_UNDERRUNS: AtomicU64 = AtomicU64::new(0);

/// Returns the cumulative `(capture_overruns, playback_underruns)` counts observed since
/// process start. Exposed for monitoring/tests; the per-event detail is emitted as WARN logs.
pub fn xrun_counts() -> (u64, u64) {
    (
        CAPTURE_OVERRUNS.load(Ordering::Relaxed),
        PLAYBACK_UNDERRUNS.load(Ordering::Relaxed),
    )
}

/// Inspects a PJMEDIA log line for ALSA XRUN signatures and, when found, bumps the
/// matching counter and emits a structured WARN carrying the running total so the audio
/// pipeline's health is visible in the log-based monitoring stack.
///
/// Returns `true` when the line was an XRUN (so callers can avoid double-logging it as
/// a plain message).
#[cfg(feature = "pjsip-linked")]
fn track_xrun(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    let is_overrun = lower.contains("overrun");
    let is_underrun = lower.contains("underrun") || lower.contains("underflow");
    if !is_overrun && !is_underrun {
        return false;
    }
    if is_overrun {
        let total = CAPTURE_OVERRUNS.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::warn!(
            target: "sip",
            kind = "alsa_capture_overrun",
            direction = "gsm_to_sip",
            total,
            "ALSA XRUN: {msg}"
        );
    }
    if is_underrun {
        let total = PLAYBACK_UNDERRUNS.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::warn!(
            target: "sip",
            kind = "alsa_playback_underrun",
            direction = "sip_to_gsm",
            total,
            "ALSA XRUN: {msg}"
        );
    }
    true
}

/// Bridges PJSIP internal logging to the `tracing` framework.
///
/// When PJSIP is compiled in, the log callback is configured during
/// Endpoint::create via pjsua_init's logging_config parameter.
/// This function is provided for the stub-mode path.
pub fn install_log_bridge() {
    #[cfg(feature = "pjsip-linked")]
    {
        tracing::debug!(target: "sip", "PJSIP log bridge active (configured via pjsua_init)");
    }

    #[cfg(not(feature = "pjsip-linked"))]
    {
        tracing::debug!(target: "sip", "PJSIP log bridge installed (stub mode)");
    }
}

#[cfg(feature = "pjsip-linked")]
pub fn get_log_callback() -> pjsua_sys::pj_log_func {
    Some(pjsip_log_callback)
}

#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
unsafe extern "C" fn pjsip_log_callback(level: std::os::raw::c_int, data: *const std::os::raw::c_char, len: std::os::raw::c_int) { // SAFETY: PJSIP passes buffer/len; null and len<=0 rejected before from_raw_parts
    if data.is_null() || len <= 0 {
        return;
    }
    let slice = std::slice::from_raw_parts(data as *const u8, len as usize);
    let msg = String::from_utf8_lossy(slice);
    let msg = msg.trim();

    // XRUN lines get a dedicated structured WARN (with running totals); skip re-logging
    // them as a plain message to avoid duplicate lines.
    if track_xrun(msg) {
        return;
    }

    match level {
        0 | 1 => tracing::error!(target: "sip", "{}", msg),
        2 => tracing::warn!(target: "sip", "{}", msg),
        3 => tracing::info!(target: "sip", "{}", msg),
        4 => tracing::debug!(target: "sip", "{}", msg),
        _ => tracing::trace!(target: "sip", "{}", msg),
    }
}
