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
unsafe extern "C" fn pjsip_log_callback(level: i32, data: *const i8, len: i32) {
    if data.is_null() || len <= 0 {
        return;
    }
    let slice = std::slice::from_raw_parts(data as *const u8, len as usize);
    let msg = String::from_utf8_lossy(slice);
    let msg = msg.trim();

    match level {
        0 | 1 => tracing::error!(target: "sip", "{}", msg),
        2 => tracing::warn!(target: "sip", "{}", msg),
        3 => tracing::info!(target: "sip", "{}", msg),
        4 => tracing::debug!(target: "sip", "{}", msg),
        _ => tracing::trace!(target: "sip", "{}", msg),
    }
}
