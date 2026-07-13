use crate::error::PjsipError;
#[cfg(feature = "pjsip-linked")]
use crate::error::PJ_SUCCESS;
use crate::log_bridge;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "pjsip-linked")]
use std::sync::atomic::{AtomicI32, AtomicU64};
#[cfg(feature = "pjsip-linked")]
use std::sync::{LazyLock, Mutex};

static SIP_PEER_DISCONNECTED: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "pjsip-linked")]
static RINGBACK_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Maps a call's `pjsua_call_id` to the peer call it should be
/// conference-bridged to, for the two-call bridging used by the inbound
/// VoWiFi-to-SIP feature (`specs/011-vowifi-sip-bridge/`, Agent B). Entries
/// are added in pairs by `Endpoint::pair_calls` (both directions) and
/// removed by `Endpoint::unpair_call`. A call with **no** entry here falls
/// back to the original, unconditional slot-0 (sound device) bridging that
/// the existing circuit-switched GSM-to-SIP bridge already relies on — that
/// is the only path exercised by the daemon today, so this table stays
/// empty and behavior is byte-for-byte unchanged for it.
#[cfg(feature = "pjsip-linked")]
static BRIDGE_PAIRS: LazyLock<Mutex<std::collections::HashMap<i32, i32>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

// Audio level monitor — populated by a per-call sampling thread (slot 0 = sound device).
// tx_level from slot 0 = ALSA capture → bridge = GSM→SIP
// rx_level from slot 0 = bridge → ALSA playback = SIP→GSM
#[cfg(feature = "pjsip-linked")]
static AUDIO_MONITOR_RUNNING: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "pjsip-linked")]
static AUDIO_CALL_SLOT: AtomicI32 = AtomicI32::new(-1);
#[cfg(feature = "pjsip-linked")]
static AUDIO_GSM_TO_SIP_SUM: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "pjsip-linked")]
static AUDIO_SIP_TO_GSM_SUM: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "pjsip-linked")]
static AUDIO_SAMPLE_COUNT: AtomicU64 = AtomicU64::new(0);

// Configured GSM→SIP software gain (stored as fixed-point: actual = value / 1000).
// Set once at endpoint creation and read in the media-state callback.
#[cfg(feature = "pjsip-linked")]
static CONF_TX_LEVEL_MILLI: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1000);

pub fn is_sip_peer_disconnected() -> bool {
    SIP_PEER_DISCONNECTED.swap(false, Ordering::AcqRel)
}

/// The conference bridge's clock rate, in Hz, as configured by the endpoint
/// that created it. Read by the ringback tone generator, whose port PJMEDIA
/// requires to run at exactly the bridge's rate — creating it at a hardcoded
/// 8000 while the bridge runs at 16000 makes `pjsua_conf_add_port` fail and
/// leaves a call ringing in silence.
#[cfg(feature = "pjsip-linked")]
static CONF_CLOCK_RATE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(8000);

#[derive(Debug, Clone)]
pub struct EndpointConfig {
    pub transport: TransportType,
    pub local_port: u16,
    pub tls_verify: bool,
    /// Clock rate (Hz) of the PJMEDIA conference bridge and of the sound
    /// device. Everything crossing the bridge is resampled to this rate, so it
    /// is the ceiling on audio bandwidth for the whole process: at 8000 a
    /// wideband codec is negotiable but pointless, since PJMEDIA downsamples
    /// it into the bridge and back out again.
    ///
    /// The circuit-switched GSM bridge uses 8000 — its audio comes off the
    /// modem's 8 kHz USB sound device, so there is no wideband to preserve.
    /// The VoWiFi bridge's Agent B uses 16000, which is what lets a carrier's
    /// AMR-WB call reach the PBX as G.722 without being squeezed through 8 kHz
    /// on the way (`specs/011-vowifi-sip-bridge/`).
    pub clock_rate: u32,
    /// PJMEDIA jitter-buffer initial pre-fill (ms). 0 = PJMEDIA default (~80 ms).
    pub jb_init_ms: i32,
    /// PJMEDIA jitter-buffer minimum pre-fetch frames.
    pub jb_min_pre: i32,
    /// PJMEDIA jitter-buffer hard ceiling (ms). -1 = unbounded.
    pub jb_max_ms: i32,
    /// When `true`, PJMEDIA VAD and noise suppression are active on the capture path.
    pub vad_enabled: bool,
    /// Software gain applied to the GSM→SIP path on the PJSUA conference bridge
    /// via `pjsua_conf_adjust_tx_level(sound_dev_slot, tx_level)`.
    /// 1.0 = unity, <1.0 attenuates, >1.0 amplifies.
    pub tx_level: f32,
    /// ALSA capture (GSM→SIP) ring-buffer depth in ms, applied to `pjsua_media_config.snd_rec_latency`.
    /// Larger values absorb scheduling jitter / XRUNs at the cost of one-way latency.
    pub snd_rec_latency_ms: u32,
    /// ALSA playback (SIP→GSM) ring-buffer depth in ms, applied to `pjsua_media_config.snd_play_latency`.
    pub snd_play_latency_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportType {
    Udp,
    Tcp,
    Tls,
}

/// One codec PJSIP has registered, as `Endpoint::codecs` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecInfo {
    /// Unique id, e.g. `"PCMU/8000/1"` or `"G722/16000/1"` — the string
    /// `Endpoint::set_codec_priority` takes.
    pub id: String,
    /// 0-255; 0 means disabled.
    pub priority: u8,
}

/// A `pj_str_t` is a pointer/length pair over memory PJSIP owns, and is not
/// NUL-terminated — so it must be read by length, not as a C string.
#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
unsafe fn pj_str_to_string(s: &pjsua_sys::pj_str_t) -> String { // SAFETY: caller passes a pj_str_t PJSIP filled in; ptr/slen are valid together
    if s.ptr.is_null() || s.slen <= 0 {
        return String::new();
    }
    let bytes = std::slice::from_raw_parts(s.ptr as *const u8, s.slen as usize);
    String::from_utf8_lossy(bytes).into_owned()
}

pub struct Endpoint {
    #[allow(dead_code)]
    config: EndpointConfig,
    started: bool,
}

impl Endpoint {
    pub fn create(config: EndpointConfig) -> Result<Self, PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            unsafe // SAFETY: Single init path; zeroed configs passed to PJSIP default/init/start APIs until success or destroy
            {
                let status = pjsua_sys::pjsua_create();
                if status != PJ_SUCCESS {
                    return Err(PjsipError::InitFailed(format!(
                        "pjsua_create returned {status}"
                    )));
                }

                let mut cfg: pjsua_sys::pjsua_config = std::mem::zeroed();
                pjsua_sys::pjsua_config_default(&mut cfg);
                cfg.cb.on_call_media_state = Some(on_call_media_state_cb);
                cfg.cb.on_call_state = Some(on_call_state_cb);

                let mut log_cfg: pjsua_sys::pjsua_logging_config = std::mem::zeroed();
                pjsua_sys::pjsua_logging_config_default(&mut log_cfg);
                log_cfg.level = 4;
                log_cfg.console_level = 0;
                log_cfg.cb = log_bridge::get_log_callback();

                let mut media_cfg: pjsua_sys::pjsua_media_config = std::mem::zeroed();
                pjsua_sys::pjsua_media_config_default(&mut media_cfg);
                media_cfg.clock_rate = config.clock_rate;
                media_cfg.snd_clock_rate = config.clock_rate;
                media_cfg.channel_count = 1;
                CONF_CLOCK_RATE.store(config.clock_rate, Ordering::Relaxed);
                media_cfg.no_vad = if config.vad_enabled { 0 } else { 1 };
                media_cfg.ec_tail_len = 0;
                media_cfg.quality = 10;
                media_cfg.ptime = 20;
                // Size the ALSA sound-device ring buffers. Defaults come from config
                // (150 ms) — a bump over PJSUA's 100/140 ms to ride out scheduling
                // jitter / XRUNs on the EC20 USB-audio capture path.
                media_cfg.snd_rec_latency = config.snd_rec_latency_ms;
                media_cfg.snd_play_latency = config.snd_play_latency_ms;
                tracing::info!(
                    target: "sip",
                    snd_rec_latency_ms = config.snd_rec_latency_ms,
                    snd_play_latency_ms = config.snd_play_latency_ms,
                    "configured ALSA sound-device latency"
                );
                media_cfg.jb_init = config.jb_init_ms;
                media_cfg.jb_min_pre = config.jb_min_pre;
                media_cfg.jb_max = config.jb_max_ms;

                // Store the configured tx_level so the media-state callback can apply it.
                CONF_TX_LEVEL_MILLI.store((config.tx_level * 1000.0) as u32, Ordering::Relaxed);

                let status = pjsua_sys::pjsua_init(&cfg, &log_cfg, &media_cfg);
                if status != PJ_SUCCESS {
                    pjsua_sys::pjsua_destroy();
                    return Err(PjsipError::InitFailed(format!(
                        "pjsua_init returned {status}"
                    )));
                }

                let mut tp_cfg: pjsua_sys::pjsua_transport_config = std::mem::zeroed();
                pjsua_sys::pjsua_transport_config_default(&mut tp_cfg);
                tp_cfg.port = config.local_port as u32;

                let tp_type = match config.transport {
                    TransportType::Udp => pjsua_sys::pjsip_transport_type_e_PJSIP_TRANSPORT_UDP,
                    TransportType::Tcp => pjsua_sys::pjsip_transport_type_e_PJSIP_TRANSPORT_TCP,
                    TransportType::Tls => pjsua_sys::pjsip_transport_type_e_PJSIP_TRANSPORT_TLS,
                };

                let mut tp_id: pjsua_sys::pjsua_transport_id = -1;
                let status = pjsua_sys::pjsua_transport_create(tp_type, &tp_cfg, &mut tp_id);
                if status != PJ_SUCCESS {
                    pjsua_sys::pjsua_destroy();
                    return Err(PjsipError::TransportCreate(format!(
                        "pjsua_transport_create returned {status}"
                    )));
                }

                let status = pjsua_sys::pjsua_start();
                if status != PJ_SUCCESS {
                    pjsua_sys::pjsua_destroy();
                    return Err(PjsipError::InitFailed(format!(
                        "pjsua_start returned {status}"
                    )));
                }
            }
        }

        #[cfg(not(feature = "pjsip-linked"))]
        {
            log_bridge::install_log_bridge();
            tracing::info!(
                transport = ?config.transport,
                port = config.local_port,
                "PJSIP endpoint created (stub mode - no real PJSIP linked)"
            );
        }

        Ok(Self {
            config,
            started: true,
        })
    }

    pub fn is_started(&self) -> bool {
        self.started
    }

    pub fn ensure_thread_registered(&self) {
        ensure_pjsip_thread();
    }

    pub fn conf_slot_count(&self) -> u32 {
        #[cfg(feature = "pjsip-linked")]
        unsafe // SAFETY: pjsua started; pjsua_conf_get_active_ports valid post-init
        {
            pjsua_sys::pjsua_conf_get_active_ports() as u32
        }
        #[cfg(not(feature = "pjsip-linked"))]
        0
    }

    pub fn set_sound_device(&self, capture_id: i32, playback_id: i32) -> Result<(), PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            self.ensure_thread_registered();
            unsafe // SAFETY: Registered PJSIP thread; device IDs valid for pjsua_set_snd_dev
            {
                let status = pjsua_sys::pjsua_set_snd_dev(capture_id, playback_id);
                if status != PJ_SUCCESS {
                    return Err(PjsipError::MediaPort(format!(
                        "pjsua_set_snd_dev returned {status}"
                    )));
                }
            }
            return Ok(());
        }

        #[cfg(not(feature = "pjsip-linked"))]
        {
            let _ = (capture_id, playback_id);
            Ok(())
        }
    }

    pub fn set_null_sound_device(&self) -> Result<(), PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            self.ensure_thread_registered();
            unsafe // SAFETY: Registered PJSIP thread; null sound device is supported after pjsua start
            {
                let status = pjsua_sys::pjsua_set_null_snd_dev();
                if status != PJ_SUCCESS {
                    return Err(PjsipError::MediaPort(format!(
                        "pjsua_set_null_snd_dev returned {status}"
                    )));
                }
            }
            return Ok(());
        }

        #[cfg(not(feature = "pjsip-linked"))]
        Ok(())
    }

    /// Every audio codec PJSIP has registered, with its current priority.
    /// Useful both for choosing what to prioritize and for logging what this
    /// build actually shipped with — whether G.722 or Opus is present depends
    /// on how pjproject was configured, not on anything in this crate.
    pub fn codecs(&self) -> Vec<CodecInfo> {
        #[cfg(feature = "pjsip-linked")]
        {
            self.ensure_thread_registered();
            unsafe // SAFETY: pjsua started; array and count are sized together per the pjsua_enum_codecs contract
            {
                let mut infos: [pjsua_sys::pjsua_codec_info; 32] = std::mem::zeroed();
                let mut count: std::os::raw::c_uint = infos.len() as std::os::raw::c_uint;
                if pjsua_sys::pjsua_enum_codecs(infos.as_mut_ptr(), &mut count) != PJ_SUCCESS {
                    return Vec::new();
                }
                return infos[..count as usize]
                    .iter()
                    .map(|info| CodecInfo {
                        id: pj_str_to_string(&info.codec_id),
                        priority: info.priority,
                    })
                    .collect();
            }
        }
        #[cfg(not(feature = "pjsip-linked"))]
        Vec::new()
    }

    /// Set a codec's priority: 0 disables it entirely, higher values win, and
    /// the order codecs appear in our SDP offer follows it. `codec_id` is the
    /// id `codecs()` reports (e.g. `"G722/16000/1"`); PJSIP also accepts a
    /// partial id, which matches every codec sharing that prefix.
    ///
    /// Priorities are endpoint-global, not per-call. Agent B's two calls (PBX
    /// and veth) therefore share one offer order — which is fine, because the
    /// only peer that *must* land on a specific codec is Agent A, and it picks
    /// from our offer by its own rules rather than by our ordering.
    pub fn set_codec_priority(&self, codec_id: &str, priority: u8) -> Result<(), PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            self.ensure_thread_registered();
            unsafe // SAFETY: pjsua started; pj_str_t borrows codec_id, which outlives the call
            {
                let id = pjsua_sys::pj_str_t {
                    ptr: codec_id.as_ptr() as *mut std::os::raw::c_char,
                    slen: codec_id.len() as pjsua_sys::pj_ssize_t,
                };
                let status = pjsua_sys::pjsua_codec_set_priority(&id, priority);
                if status != PJ_SUCCESS {
                    return Err(PjsipError::MediaPort(format!(
                        "pjsua_codec_set_priority({codec_id}, {priority}) returned {status}"
                    )));
                }
            }
            return Ok(());
        }

        #[cfg(not(feature = "pjsip-linked"))]
        {
            let _ = (codec_id, priority);
            Ok(())
        }
    }

    /// Register two calls to be conference-bridged to *each other* once both
    /// have active media, instead of the default slot-0 (sound device)
    /// bridging `on_call_media_state_cb` otherwise applies. Used by the
    /// inbound VoWiFi-to-SIP bridge (Agent B) to connect its PBX-side leg to
    /// its veth-side leg. Idempotent to call before either call's media has
    /// gone active — the actual `pjsua_conf_connect` calls happen lazily,
    /// the first time either call's media-active callback observes its peer
    /// already has an active conf slot too.
    pub fn pair_calls(&self, call_a: i32, call_b: i32) {
        #[cfg(feature = "pjsip-linked")]
        {
            let mut pairs = BRIDGE_PAIRS.lock().unwrap_or_else(|e| e.into_inner());
            pairs.insert(call_a, call_b);
            pairs.insert(call_b, call_a);
        }
        #[cfg(not(feature = "pjsip-linked"))]
        {
            let _ = (call_a, call_b);
        }
    }

    /// Remove a call's pairing (both directions), e.g. once it hangs up.
    /// Safe to call even if the call was never paired.
    pub fn unpair_call(&self, call_id: i32) {
        #[cfg(feature = "pjsip-linked")]
        {
            let mut pairs = BRIDGE_PAIRS.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(peer) = pairs.remove(&call_id) {
                pairs.remove(&peer);
            }
        }
        #[cfg(not(feature = "pjsip-linked"))]
        {
            let _ = call_id;
        }
    }

    pub fn find_audio_device(&self, alsa_hint: &str) -> Result<i32, PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            self.ensure_thread_registered();
            let card_num = alsa_hint
                .strip_prefix("hw:")
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.parse::<u32>().ok());

            let alsa_card_name = card_num.and_then(|n| read_alsa_card_name(n));

            unsafe // SAFETY: Registered PJSIP thread; device indices and pjmedia_aud_dev_info out-params match API contract
            {
                let count = pjsua_sys::pjmedia_aud_dev_count() as i32;
                tracing::debug!(
                    count,
                    alsa_hint,
                    ?alsa_card_name,
                    "enumerating PJSIP audio devices"
                );

                for i in 0..count {
                    let mut info: pjsua_sys::pjmedia_aud_dev_info = std::mem::zeroed();
                    let status = pjsua_sys::pjmedia_aud_dev_get_info(i, &mut info);
                    if status != PJ_SUCCESS {
                        continue;
                    }
                    let name = std::ffi::CStr::from_ptr(info.name.as_ptr())
                        .to_string_lossy()
                        .to_string();
                    tracing::debug!(dev_id = i, name = %name, "PJSIP audio device");

                    if let Some(ref card_name) = alsa_card_name {
                        if name.contains(card_name.as_str()) {
                            return Ok(i);
                        }
                    }

                    if let Some(card) = card_num {
                        let card_str = format!("card {card}");
                        let hw_str = format!("hw:{card}");
                        if name.contains(&card_str) || name.contains(&hw_str) {
                            return Ok(i);
                        }
                    }

                    if name.contains(alsa_hint) {
                        return Ok(i);
                    }
                }
            }

            return Err(PjsipError::MediaPort(format!(
                "audio device not found for '{alsa_hint}'"
            )));
        }

        #[cfg(not(feature = "pjsip-linked"))]
        {
            let _ = alsa_hint;
            Ok(0)
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        if self.started {
            #[cfg(feature = "pjsip-linked")]
            {
                unsafe // SAFETY: pjsua_destroy pairs with successful create/init when started is true
                {
                    pjsua_sys::pjsua_destroy();
                }
            }
            tracing::info!("PJSIP endpoint destroyed");
            self.started = false;
        }
    }
}

pub fn ensure_pjsip_thread() {
    #[cfg(feature = "pjsip-linked")]
    {
        unsafe // SAFETY: pj_thread_register uses thread-local storage so descriptor and handle live for the thread
        {
            if pjsua_sys::pj_thread_is_registered() == 0 {
                thread_local! {
                    static THREAD_DESC: std::cell::RefCell<[u8; 256]> = std::cell::RefCell::new([0u8; 256]);
                    static THREAD_HANDLE: std::cell::RefCell<*mut pjsua_sys::pj_thread_t> =
                        std::cell::RefCell::new(std::ptr::null_mut());
                }
                THREAD_DESC.with(|desc| {
                    THREAD_HANDLE.with(|handle| {
                        let mut desc = desc.borrow_mut();
                        let mut handle = handle.borrow_mut();
                        pjsua_sys::pj_thread_register(
                            b"rust-async\0".as_ptr() as *const std::os::raw::c_char,
                            desc.as_mut_ptr() as *mut _,
                            &mut *handle,
                        );
                    });
                });
            }
        }
    }
}

#[cfg(feature = "pjsip-linked")]
fn read_alsa_card_name(card_num: u32) -> Option<String> {
    let path = format!("/proc/asound/card{card_num}/id");
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// The audio codec PJSIP actually negotiated for `call_id` — name (e.g.
/// "PCMU", "AMR-WB"), clock rate in Hz, and channel count — read from the
/// live stream rather than inferred from what we offered, so it reports what
/// the two ends really settled on.
///
/// # Safety
/// Caller must ensure PJSUA is initialized and `call_id` is a valid, active
/// call (both hold inside `on_call_media_state_cb`).
#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
unsafe fn negotiated_audio_codec(call_id: pjsua_sys::pjsua_call_id) -> Option<(String, u32, u32)> { // SAFETY: caller guarantees pjsua is initialized and call_id is a live call (see the doc above); stack stream_info is writable for get_stream_info
    let mut si: pjsua_sys::pjsua_stream_info = std::mem::zeroed();
    // Media index 0: these are audio-only calls, so the first stream is it.
    if pjsua_sys::pjsua_call_get_stream_info(call_id, 0, &mut si) != PJ_SUCCESS {
        return None;
    }
    if si.type_ != pjsua_sys::pjmedia_type_PJMEDIA_TYPE_AUDIO {
        return None;
    }
    let fmt = si.info.aud.fmt;
    if fmt.encoding_name.ptr.is_null() || fmt.encoding_name.slen <= 0 {
        return None;
    }
    let name = std::slice::from_raw_parts(
        fmt.encoding_name.ptr as *const u8,
        fmt.encoding_name.slen as usize,
    );
    Some((
        String::from_utf8_lossy(name).into_owned(),
        fmt.clock_rate,
        fmt.channel_cnt,
    ))
}

#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
unsafe extern "C" fn on_call_media_state_cb(call_id: pjsua_sys::pjsua_call_id) { // SAFETY: PJSIP invokes with valid call_id after init; stack call_info writable for get_info
    let mut info: pjsua_sys::pjsua_call_info = std::mem::zeroed();
    let status = pjsua_sys::pjsua_call_get_info(call_id, &mut info);
    if status != PJ_SUCCESS {
        return;
    }

    if info.media_status == pjsua_sys::pjsua_call_media_status_PJSUA_CALL_MEDIA_ACTIVE {
        let call_slot = info.conf_slot as i32;

        match negotiated_audio_codec(call_id) {
            Some((name, clock_rate, channels)) => tracing::info!(
                call_id,
                codec = %name,
                sample_rate = clock_rate,
                channels,
                "SIP call media active — negotiated audio codec"
            ),
            None => tracing::warn!(call_id, "SIP call media active but the negotiated codec could not be read"),
        }

        let peer_call_id = {
            let pairs = BRIDGE_PAIRS.lock().unwrap_or_else(|e| e.into_inner());
            pairs.get(&call_id).copied()
        };

        if let Some(peer_id) = peer_call_id {
            // Two-call bridging (inbound VoWiFi-to-SIP feature, Agent B):
            // connect this call's slot directly to its paired peer's slot,
            // bypassing slot 0 (the sound device — absent/null in this
            // process) entirely. Deliberately skips the tx_level adjustment
            // and audio-level monitor below, both of which are specific to
            // the single-call, real-sound-device GSM bridge.
            let mut peer_info: pjsua_sys::pjsua_call_info = std::mem::zeroed();
            if pjsua_sys::pjsua_call_get_info(peer_id, &mut peer_info) == PJ_SUCCESS
                && peer_info.media_status == pjsua_sys::pjsua_call_media_status_PJSUA_CALL_MEDIA_ACTIVE
            {
                let peer_slot = peer_info.conf_slot as i32;
                pjsua_sys::pjsua_conf_connect(call_slot, peer_slot);
                pjsua_sys::pjsua_conf_connect(peer_slot, call_slot);
                tracing::info!(
                    call_id,
                    peer_id,
                    call_slot,
                    peer_slot,
                    "paired calls' media active, conference-connected to each other"
                );
            } else {
                // Peer isn't active yet — its own media-active callback will
                // find this call already active (via the same BRIDGE_PAIRS
                // lookup) and complete the connection symmetrically then.
                tracing::debug!(
                    call_id,
                    peer_id,
                    "call media active, awaiting paired peer's media to become active"
                );
            }
            return;
        }

        pjsua_sys::pjsua_conf_connect(call_slot, 0);
        pjsua_sys::pjsua_conf_connect(0, call_slot);

        // Apply configured GSM→SIP software gain on the sound-device slot (slot 0).
        let tx_level = CONF_TX_LEVEL_MILLI.load(Ordering::Relaxed) as f32 / 1000.0;
        if (tx_level - 1.0_f32).abs() > 0.001 {
            pjsua_sys::pjsua_conf_adjust_tx_level(0, tx_level);
            tracing::info!(call_id, tx_level, "GSM→SIP conference tx_level adjusted");
        }

        tracing::info!(
            call_id,
            call_slot,
            "call media active, audio connected to sound device"
        );

        // Reset accumulators and start per-second signal-level sampler
        AUDIO_GSM_TO_SIP_SUM.store(0, Ordering::Relaxed);
        AUDIO_SIP_TO_GSM_SUM.store(0, Ordering::Relaxed);
        AUDIO_SAMPLE_COUNT.store(0, Ordering::Relaxed);
        AUDIO_CALL_SLOT.store(call_slot, Ordering::Relaxed);
        AUDIO_MONITOR_RUNNING.store(true, Ordering::Release);

        std::thread::spawn(|| {
            ensure_pjsip_thread();
            while AUDIO_MONITOR_RUNNING.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if !AUDIO_MONITOR_RUNNING.load(Ordering::Acquire) {
                    break;
                }
                let mut tx: u32 = 0; // GSM→SIP (ALSA capture → bridge)
                let mut rx: u32 = 0; // SIP→GSM (bridge → ALSA playback)
                // SAFETY: pjsua is running; slot 0 is always the sound device
                if pjsua_sys::pjsua_conf_get_signal_level(0, &mut tx, &mut rx)
                    == PJ_SUCCESS
                {
                    AUDIO_GSM_TO_SIP_SUM.fetch_add(tx as u64, Ordering::Relaxed);
                    AUDIO_SIP_TO_GSM_SUM.fetch_add(rx as u64, Ordering::Relaxed);
                    AUDIO_SAMPLE_COUNT.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }
}

#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
unsafe extern "C" fn on_call_state_cb( // SAFETY: PJSIP invokes with valid call_id after init; stack call_info writable; event is library-managed
    call_id: pjsua_sys::pjsua_call_id,
    _event: *mut pjsua_sys::pjsip_event,
) {
    let mut info: pjsua_sys::pjsua_call_info = std::mem::zeroed();
    let status = pjsua_sys::pjsua_call_get_info(call_id, &mut info);
    if status != PJ_SUCCESS {
        return;
    }

    let state = info.state;
    tracing::info!(call_id, state, "SIP call state changed");

    match state {
        s if s == pjsua_sys::pjsip_inv_state_PJSIP_INV_STATE_CALLING
            || s == pjsua_sys::pjsip_inv_state_PJSIP_INV_STATE_EARLY =>
        {
            if !RINGBACK_ACTIVE.load(Ordering::Acquire) {
                start_ringback_tone();
            }
        }
        s if s == pjsua_sys::pjsip_inv_state_PJSIP_INV_STATE_CONFIRMED => {
            stop_ringback_tone();
        }
        s if s == pjsua_sys::pjsip_inv_state_PJSIP_INV_STATE_DISCONNECTED => {
            stop_ringback_tone();

            {
                let mut pairs = BRIDGE_PAIRS.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(peer) = pairs.remove(&call_id) {
                    pairs.remove(&peer);
                }
            }

            AUDIO_MONITOR_RUNNING.store(false, Ordering::Release);
            let count = AUDIO_SAMPLE_COUNT.load(Ordering::Relaxed);
            let gsm_to_sip = AUDIO_GSM_TO_SIP_SUM.load(Ordering::Relaxed);
            let sip_to_gsm = AUDIO_SIP_TO_GSM_SUM.load(Ordering::Relaxed);
            if count > 0 {
                tracing::info!(
                    call_id,
                    gsm_to_sip_avg = gsm_to_sip / count,
                    sip_to_gsm_avg = sip_to_gsm / count,
                    gsm_to_sip_total = gsm_to_sip,
                    sip_to_gsm_total = sip_to_gsm,
                    samples = count,
                    "call audio levels (0=silence 255=max)",
                );
            } else {
                // The level monitor samples the *sound-device* slot, which only
                // the circuit-switched GSM bridge uses. A VoWiFi bridge call is
                // conference-connected to its peer call with a null sound device
                // (see `pair_calls`), so it legitimately collects no samples —
                // this is not evidence that its media failed.
                tracing::debug!(
                    call_id,
                    "call ended with no sound-device audio samples (expected for a paired bridge call)"
                );
            }

            tracing::info!(call_id, "SIP peer disconnected, signaling GSM hangup");
            SIP_PEER_DISCONNECTED.store(true, Ordering::Release);
        }
        _ => {}
    }
}

#[cfg(feature = "pjsip-linked")]
static mut RINGBACK_SLOT: i32 = -1;
#[cfg(feature = "pjsip-linked")]
static mut RINGBACK_PORT: *mut pjsua_sys::pjmedia_port = std::ptr::null_mut();

#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
unsafe fn start_ringback_tone() { // SAFETY: Called only from PJSIP call-state callback after pjsua start; statics follow start/stop pairing
    use std::ffi::CString;

    if RINGBACK_ACTIVE.load(Ordering::Acquire) {
        return;
    }

    let pool = pjsua_sys::pjsua_pool_create(b"ringback\0".as_ptr() as *const std::os::raw::c_char, 512, 512);
    if pool.is_null() {
        return;
    }

    let name = CString::new("ringback").unwrap();
    let mut port: *mut pjsua_sys::pjmedia_port = std::ptr::null_mut();
    // A conference port must run at the bridge's own clock rate, or
    // `pjsua_conf_add_port` below rejects it.
    let clock_rate = CONF_CLOCK_RATE.load(Ordering::Relaxed);
    let status = pjsua_sys::pjmedia_tonegen_create(
        pool,
        clock_rate,
        1,               // channel count
        clock_rate / 50, // samples per frame (20ms)
        16,              // bits per sample
        0,               // options
        &mut port,
    );
    if status != PJ_SUCCESS || port.is_null() {
        tracing::warn!(status, clock_rate, "ringback tone generator creation failed");
        return;
    }

    let mut tone = pjsua_sys::pjmedia_tone_desc {
        freq1: 400,
        freq2: 0,
        on_msec: 1000,
        off_msec: 4000,
        volume: 0,
        flags: 0,
    };

    const PJMEDIA_TONEGEN_LOOP: u32 = 1;
    let status = pjsua_sys::pjmedia_tonegen_play(port, 1, &mut tone, PJMEDIA_TONEGEN_LOOP);
    if status != PJ_SUCCESS {
        return;
    }

    let mut slot: i32 = -1;
    let status = pjsua_sys::pjsua_conf_add_port(pool, port, &mut slot);
    if status != PJ_SUCCESS {
        return;
    }

    pjsua_sys::pjsua_conf_connect(slot, 0);

    RINGBACK_PORT = port;
    RINGBACK_SLOT = slot;
    RINGBACK_ACTIVE.store(true, Ordering::Release);
    tracing::info!(slot, "ringback tone started");

    let _ = name;
}

#[cfg(feature = "pjsip-linked")]
#[rustfmt::skip]
unsafe fn stop_ringback_tone() { // SAFETY: Complements start_ringback_tone; conf slot valid when active; statics reset under RINGBACK_ACTIVE guard
    if !RINGBACK_ACTIVE.load(Ordering::Acquire) {
        return;
    }

    if RINGBACK_SLOT >= 0 {
        pjsua_sys::pjsua_conf_disconnect(RINGBACK_SLOT, 0);
        pjsua_sys::pjsua_conf_remove_port(RINGBACK_SLOT);
        RINGBACK_SLOT = -1;
    }

    RINGBACK_PORT = std::ptr::null_mut();
    RINGBACK_ACTIVE.store(false, Ordering::Release);
    tracing::info!("ringback tone stopped");
}
