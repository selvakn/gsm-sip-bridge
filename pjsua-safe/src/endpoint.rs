use crate::error::PjsipError;
#[cfg(feature = "pjsip-linked")]
use crate::error::PJ_SUCCESS;
use crate::log_bridge;
use std::sync::atomic::{AtomicBool, Ordering};

static SIP_PEER_DISCONNECTED: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "pjsip-linked")]
static RINGBACK_ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn is_sip_peer_disconnected() -> bool {
    SIP_PEER_DISCONNECTED.swap(false, Ordering::AcqRel)
}

#[derive(Debug, Clone)]
pub struct EndpointConfig {
    pub transport: TransportType,
    pub local_port: u16,
    pub tls_verify: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportType {
    Udp,
    Tcp,
    Tls,
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
            // SAFETY: pjsua_create must be called exactly once before any other pjsua API.
            // We ensure this by taking ownership in Endpoint::create which can only succeed once.
            unsafe {
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
                media_cfg.clock_rate = 8000;
                media_cfg.snd_clock_rate = 8000;
                media_cfg.channel_count = 1;
                media_cfg.no_vad = 1;

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
        unsafe {
            pjsua_sys::pjsua_conf_get_active_ports() as u32
        }
        #[cfg(not(feature = "pjsip-linked"))]
        0
    }

    pub fn set_sound_device(&self, capture_id: i32, playback_id: i32) -> Result<(), PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            self.ensure_thread_registered();
            unsafe {
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
            unsafe {
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

    pub fn find_audio_device(&self, alsa_hint: &str) -> Result<i32, PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            self.ensure_thread_registered();
            let card_num = alsa_hint
                .strip_prefix("hw:")
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.parse::<u32>().ok());

            let alsa_card_name = card_num.and_then(|n| read_alsa_card_name(n));

            unsafe {
                let count = pjsua_sys::pjmedia_aud_dev_count() as i32;
                tracing::debug!(count, alsa_hint, ?alsa_card_name, "enumerating PJSIP audio devices");

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
                unsafe {
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
        unsafe {
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
                            b"rust-async\0".as_ptr() as *const i8,
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
    std::fs::read_to_string(&path).ok().map(|s| s.trim().to_string())
}

#[cfg(feature = "pjsip-linked")]
unsafe extern "C" fn on_call_media_state_cb(call_id: pjsua_sys::pjsua_call_id) {
    let mut info: pjsua_sys::pjsua_call_info = std::mem::zeroed();
    let status = pjsua_sys::pjsua_call_get_info(call_id, &mut info);
    if status != PJ_SUCCESS {
        return;
    }

    if info.media_status == pjsua_sys::pjsua_call_media_status_PJSUA_CALL_MEDIA_ACTIVE {
        let call_slot = info.conf_slot as i32;
        pjsua_sys::pjsua_conf_connect(call_slot, 0);
        pjsua_sys::pjsua_conf_connect(0, call_slot);
        tracing::info!(call_id, call_slot, "call media active, audio connected to sound device");
    }
}

#[cfg(feature = "pjsip-linked")]
unsafe extern "C" fn on_call_state_cb(
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
unsafe fn start_ringback_tone() {
    use std::ffi::CString;

    if RINGBACK_ACTIVE.load(Ordering::Acquire) {
        return;
    }

    let pool = pjsua_sys::pjsua_pool_create(
        b"ringback\0".as_ptr() as *const i8,
        512,
        512,
    );
    if pool.is_null() {
        return;
    }

    let name = CString::new("ringback").unwrap();
    let mut port: *mut pjsua_sys::pjmedia_port = std::ptr::null_mut();
    let status = pjsua_sys::pjmedia_tonegen_create(
        pool,
        8000,  // clock rate
        1,     // channel count
        160,   // samples per frame (20ms)
        16,    // bits per sample
        0,     // options
        &mut port,
    );
    if status != PJ_SUCCESS || port.is_null() {
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

    let status = pjsua_sys::pjmedia_tonegen_play(port, 1, &mut tone, 0);
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
unsafe fn stop_ringback_tone() {
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
