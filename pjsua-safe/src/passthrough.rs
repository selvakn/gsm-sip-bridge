//! Media passthrough hooks (specs/120-amr-passthrough-forwarder).
//!
//! PJSIP keeps doing SIP and SDP for the client leg; its media transport for
//! that call becomes a pjmedia *loop* transport that merely advertises the
//! application's forwarder socket, and its stream is paused so it neither
//! sends nor receives. The application relays the audio on its own sockets.
//!
//! Everything is keyed by call id and **absent by default**: a call with no
//! entry in the registry takes exactly the path it always took (the media
//! transport hook returns PJSIP's own transport). This is the only file in the
//! workspace that touches the loop transport.
//!
//! Proven on stock pjproject 2.16 by spikes S1/S2
//! (`specs/120-amr-passthrough-forwarder/spike-s1.md`).

use crate::error::PjsipError;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{LazyLock, Mutex, MutexGuard};

/// What the registry knows about a call.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// The client leg: advertise `rtp_port` and keep the stream paused. Only a
    /// re-INVITE that still offers `codec` is accepted.
    Forward {
        rtp_port: u16,
        codec: String,
        clock_rate: u32,
    },
    /// The veth leg of a forwarded call: nothing to advertise, but its stream
    /// is paused (it would otherwise keep sending silence).
    Quiet,
}

static REGISTRY: LazyLock<Mutex<HashMap<i32, Entry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// How many times each call's stream has been (re)created. A caller waiting
/// for a re-INVITE to take effect watches this advance.
static GENERATION: LazyLock<Mutex<HashMap<i32, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// The live `pjmedia_stream*` of each call, as an address, so a stream can be
/// paused after the fact. Updated under the lock in the create/destroy
/// callbacks, so a pause never races a destroy.
static STREAMS: LazyLock<Mutex<HashMap<i32, usize>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ask for the call's client-leg media to be carried by the application's
/// forwarder on `rtp_port` (RTCP on `rtp_port + 1`). Takes effect the next
/// time PJSIP creates this call's media transport, i.e. on the
/// `PJSUA_CALL_REINIT_MEDIA` re-INVITE.
pub fn register_forward(call_id: i32, rtp_port: u16, codec: &str, clock_rate: u32) {
    lock(&REGISTRY).insert(
        call_id,
        Entry::Forward {
            rtp_port,
            codec: codec.to_string(),
            clock_rate,
        },
    );
}

/// Mark a call as carrying no media: its stream is paused now and after every
/// re-creation. Returns whether a live stream was found to pause right away.
pub fn register_quiet(call_id: i32) -> bool {
    lock(&REGISTRY).insert(call_id, Entry::Quiet);
    pause_current_stream(call_id)
}

/// Forget a call's entry: its next media transport is PJSIP's own again.
pub fn clear(call_id: i32) {
    lock(&REGISTRY).remove(&call_id);
}

/// Whether the call has an entry (a forwarded or quiet call).
pub fn is_registered(call_id: i32) -> bool {
    lock(&REGISTRY).contains_key(&call_id)
}

/// How many times this call's media stream has been created.
pub fn media_generation(call_id: i32) -> u64 {
    lock(&GENERATION).get(&call_id).copied().unwrap_or(0)
}

/// Drop everything held for a call. Called when it disconnects, so a reused
/// PJSIP call id never inherits an entry.
#[cfg_attr(not(feature = "pjsip-linked"), allow(dead_code))]
pub(crate) fn on_call_disconnected(call_id: i32) {
    lock(&REGISTRY).remove(&call_id);
    lock(&GENERATION).remove(&call_id);
    lock(&STREAMS).remove(&call_id);
}

#[cfg_attr(not(feature = "pjsip-linked"), allow(dead_code))]
fn bump_generation(call_id: i32) {
    *lock(&GENERATION).entry(call_id).or_insert(0) += 1;
}

/// Whether a re-INVITE offer's audio section still offers the call's codec,
/// judged from its `a=rtpmap` values (`"96 AMR-WB/16000"`).
#[cfg_attr(not(feature = "pjsip-linked"), allow(dead_code))]
fn offer_keeps_codec<'a>(
    rtpmaps: impl IntoIterator<Item = &'a str>,
    name: &str,
    rate: u32,
) -> bool {
    rtpmaps.into_iter().any(|value| {
        let Some((_pt, enc)) = value.trim().split_once(' ') else {
            return false;
        };
        let mut parts = enc.trim().split('/');
        parts.next().is_some_and(|n| n.eq_ignore_ascii_case(name))
            && parts.next().and_then(|r| r.parse::<u32>().ok()) == Some(rate)
    })
}

/// What PJSIP negotiated on the client leg, read from the live stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientMedia {
    /// `AMR-WB`, `AMR`, `PCMU`, ...
    pub encoding_name: String,
    pub clock_rate: u32,
    /// Where the client wants RTP; `None` before it is known.
    pub remote_rtp: Option<SocketAddr>,
    /// Payload type we send / receive for the audio codec.
    pub tx_pt: u8,
    pub rx_pt: u8,
    /// RFC 4733 telephone-event payload types, if negotiated.
    pub tx_event_pt: Option<u8>,
    pub rx_event_pt: Option<u8>,
    /// The client's `a=fmtp` for the codec, as `name=value; name=value`.
    pub remote_fmtp: String,
    pub frames_per_packet: u8,
    /// The media is active (not locally held, not remotely held, not inactive)
    /// and the client may receive.
    pub send_to_client: bool,
    /// The call's media status is `ACTIVE` or on hold from a re-INVITE, i.e.
    /// the stream exists.
    pub has_media: bool,
}

#[cfg(feature = "pjsip-linked")]
pub use linked::*;

#[cfg(not(feature = "pjsip-linked"))]
mod stub {
    use super::*;

    /// Stub builds have no PJSIP: nothing to read.
    pub fn client_media(_call_id: i32) -> Option<ClientMedia> {
        None
    }

    pub fn pause_current_stream(_call_id: i32) -> bool {
        false
    }

    pub fn reinvite_reinit_media(_call_id: i32) -> Result<(), PjsipError> {
        Err(PjsipError::CallMake(
            "re-INVITE needs a PJSIP-linked build".into(),
        ))
    }

    pub fn disconnect_from_conference(_call_id: i32) {}

    pub fn stream_packets(_call_id: i32) -> Option<(u32, u32)> {
        None
    }

    pub fn hold_call(_call_id: i32) -> Result<(), PjsipError> {
        Err(PjsipError::CallMake(
            "hold needs a PJSIP-linked build".into(),
        ))
    }

    pub fn unhold_call(_call_id: i32) -> Result<(), PjsipError> {
        Err(PjsipError::CallMake(
            "unhold needs a PJSIP-linked build".into(),
        ))
    }

    pub fn reinvite_plain(_call_id: i32) -> Result<(), PjsipError> {
        Err(PjsipError::CallMake(
            "re-INVITE needs a PJSIP-linked build".into(),
        ))
    }
}
#[cfg(not(feature = "pjsip-linked"))]
pub use stub::*;

#[cfg(feature = "pjsip-linked")]
mod linked {
    use super::*;
    use crate::endpoint::{ensure_pjsip_thread, pj_str_to_string};
    use crate::error::PJ_SUCCESS;
    use std::os::raw::{c_char, c_int, c_uint, c_void};

    /// Install the callbacks. Called once from `Endpoint::create`.
    pub(crate) fn install(cb: &mut pjsua_sys::pjsua_callback) {
        cb.on_create_media_transport = Some(on_create_media_transport_cb);
        cb.on_stream_created2 = Some(on_stream_created2_cb);
        cb.on_stream_destroyed = Some(on_stream_destroyed_cb);
        cb.on_call_rx_reinvite = Some(on_call_rx_reinvite_cb);
    }

    /// `pjmedia_transport_close` / `_get_info` are `static inline` in the
    /// header, so bindgen has no symbol for them: go through the vtable as the
    /// header does.
    unsafe fn transport_close(tp: *mut pjsua_sys::pjmedia_transport) {
        // SAFETY: `tp` is a live transport PJSIP handed to the hook; its `op`
        // table is set for the transport's whole life, and `destroy` is the
        // header's own `pjmedia_transport_close` body.
        if let Some(destroy) = (*(*tp).op).destroy {
            destroy(tp);
        }
    }

    unsafe fn transport_rtp_addr(tp: *mut pjsua_sys::pjmedia_transport) -> Option<(String, c_int)> {
        // SAFETY: `info` is zeroed like `pjmedia_transport_info_init` does
        // (sockets set to the invalid value); `get_info` only writes it. The
        // printed buffer is NUL-terminated by `pj_sockaddr_print`.
        let mut info: pjsua_sys::pjmedia_transport_info = std::mem::zeroed();
        info.sock_info.rtp_sock = -1;
        info.sock_info.rtcp_sock = -1;
        let get_info = (*(*tp).op).get_info?;
        if get_info(tp, &mut info) != PJ_SUCCESS {
            return None;
        }
        let af = info.sock_info.rtp_addr_name.addr.sa_family as c_int;
        let mut buf = [0 as c_char; 64];
        pjsua_sys::pj_sockaddr_print(
            &info.sock_info.rtp_addr_name as *const _ as *const c_void,
            buf.as_mut_ptr(),
            buf.len() as c_int,
            0,
        );
        let text = std::ffi::CStr::from_ptr(buf.as_ptr())
            .to_string_lossy()
            .into_owned();
        Some((text, af))
    }

    #[rustfmt::skip]
    unsafe extern "C" fn on_create_media_transport_cb( // SAFETY: PJSIP calls this on a registered PJSIP thread with a live call id and base transport; the returned transport replaces it
        call_id: pjsua_sys::pjsua_call_id,
        media_idx: c_uint,
        base_tp: *mut pjsua_sys::pjmedia_transport,
        _flags: c_uint,
    ) -> *mut pjsua_sys::pjmedia_transport {
        let port = match lock(&REGISTRY).get(&call_id) {
            Some(Entry::Forward { rtp_port, .. }) if media_idx == 0 => *rtp_port,
            _ => return base_tp,
        };
        let Some((addr, af)) = transport_rtp_addr(base_tp) else {
            tracing::warn!(call_id, "cannot read the base media transport's address; leaving the call on PJSIP's own media");
            return base_tp;
        };
        let Ok(addr_c) = std::ffi::CString::new(addr.clone()) else {
            return base_tp;
        };
        let mut setting: pjsua_sys::pjmedia_loop_tp_setting = std::mem::zeroed();
        pjsua_sys::pjmedia_loop_tp_setting_default(&mut setting);
        setting.af = af;
        setting.addr = pjsua_sys::pj_str(addr_c.as_ptr() as *mut c_char);
        setting.port = port as c_int;
        setting.disable_rx = 1;
        let mut tp: *mut pjsua_sys::pjmedia_transport = std::ptr::null_mut();
        let status = pjsua_sys::pjmedia_transport_loop_create2(
            pjsua_sys::pjsua_get_pjmedia_endpt(),
            &setting,
            &mut tp,
        );
        if status != PJ_SUCCESS || tp.is_null() {
            tracing::warn!(call_id, status, "loop transport creation failed; leaving the call on PJSIP's own media");
            return base_tp;
        }
        // PJSIP's own socket is never used for this call again.
        transport_close(base_tp);
        tracing::info!(call_id, advertised = %format!("{addr}:{port}"), "client leg media now advertises the forwarder");
        tp
    }

    #[rustfmt::skip]
    unsafe extern "C" fn on_stream_created2_cb( // SAFETY: PJSIP calls this with a live call id and a param whose stream pointer is valid for the callback
        call_id: pjsua_sys::pjsua_call_id,
        prm: *mut pjsua_sys::pjsua_on_stream_created_param,
    ) {
        if prm.is_null() || (*prm).stream.is_null() {
            return;
        }
        let stream = (*prm).stream;
        // The pause happens under the STREAMS lock so a concurrent
        // `on_stream_destroyed_cb` cannot free the stream underneath it.
        let mut streams = lock(&STREAMS);
        streams.insert(call_id, stream as usize);
        let registered = lock(&REGISTRY).contains_key(&call_id);
        if registered {
            let status = pjsua_sys::pjmedia_stream_pause(
                stream,
                pjsua_sys::pjmedia_dir_PJMEDIA_DIR_ENCODING_DECODING,
            );
            tracing::debug!(call_id, status, "forwarded call's PJSIP stream paused");
        }
        drop(streams);
        bump_generation(call_id);
    }

    #[rustfmt::skip]
    unsafe extern "C" fn on_stream_destroyed_cb( // SAFETY: PJSIP calls this just before it destroys `strm`; only the address is compared
        call_id: pjsua_sys::pjsua_call_id,
        strm: *mut pjsua_sys::pjmedia_stream,
        _stream_idx: c_uint,
    ) {
        let mut streams = lock(&STREAMS);
        if streams.get(&call_id) == Some(&(strm as usize)) {
            streams.remove(&call_id);
        }
    }

    #[rustfmt::skip]
    unsafe extern "C" fn on_call_rx_reinvite_cb( // SAFETY: PJSIP calls this with a live call id; `offer` is valid for the callback and only read; `code` is writable
        call_id: pjsua_sys::pjsua_call_id,
        offer: *const pjsua_sys::pjmedia_sdp_session,
        _rdata: *mut pjsua_sys::pjsip_rx_data,
        _reserved: *mut c_void,
        _async: *mut pjsua_sys::pj_bool_t,
        code: *mut pjsua_sys::pjsip_status_code,
        _opt: *mut pjsua_sys::pjsua_call_setting,
    ) {
        let (codec, rate) = match lock(&REGISTRY).get(&call_id) {
            Some(Entry::Forward { codec, clock_rate, .. }) => (codec.clone(), *clock_rate),
            _ => return,
        };
        if offer.is_null() || code.is_null() {
            return;
        }
        let mut rtpmaps = Vec::new();
        let sdp = &*offer;
        for i in 0..sdp.media_count as usize {
            let media = &*sdp.media[i];
            if pj_str_to_string(&media.desc.media) != "audio" {
                continue;
            }
            for a in 0..media.attr_count as usize {
                let attr = &*media.attr[a];
                if pj_str_to_string(&attr.name) == "rtpmap" {
                    rtpmaps.push(pj_str_to_string(&attr.value));
                }
            }
            break;
        }
        if offer_keeps_codec(rtpmaps.iter().map(String::as_str), &codec, rate) {
            return;
        }
        tracing::info!(call_id, codec = %codec, "re-INVITE would drop the forwarded codec; answering 488 and keeping the call as it is");
        *code = 488;
    }

    /// Pause the call's current stream, if one is live.
    pub fn pause_current_stream(call_id: i32) -> bool {
        ensure_pjsip_thread();
        let streams = lock(&STREAMS);
        let Some(&addr) = streams.get(&call_id) else {
            return false;
        };
        // SAFETY: the address was stored by `on_stream_created2_cb` and is
        // removed by `on_stream_destroyed_cb` under this same lock, so while we
        // hold the lock the stream has not been destroyed.
        let status = unsafe {
            pjsua_sys::pjmedia_stream_pause(
                addr as *mut pjsua_sys::pjmedia_stream,
                pjsua_sys::pjmedia_dir_PJMEDIA_DIR_ENCODING_DECODING,
            )
        };
        status == PJ_SUCCESS
    }

    /// Disconnect a call's conference slot from every other port, both ways.
    pub fn disconnect_from_conference(call_id: i32) {
        ensure_pjsip_thread();
        // SAFETY: pjsua is started; `info` is writable; the id array is sized
        // together with the count passed to `pjsua_enum_conf_ports`.
        unsafe {
            let mut info: pjsua_sys::pjsua_call_info = std::mem::zeroed();
            if pjsua_sys::pjsua_call_get_info(call_id, &mut info) != PJ_SUCCESS
                || info.conf_slot < 0
            {
                return;
            }
            let mut ids = [0 as pjsua_sys::pjsua_conf_port_id; 64];
            let mut count = ids.len() as c_uint;
            if pjsua_sys::pjsua_enum_conf_ports(ids.as_mut_ptr(), &mut count) != PJ_SUCCESS {
                return;
            }
            for &other in &ids[..count as usize] {
                if other != info.conf_slot {
                    pjsua_sys::pjsua_conf_disconnect(info.conf_slot, other);
                    pjsua_sys::pjsua_conf_disconnect(other, info.conf_slot);
                }
            }
        }
    }

    /// Re-INVITE the call with `PJSUA_CALL_REINIT_MEDIA`, so PJSIP creates its
    /// media transport again (through the hook above) and re-offers.
    pub fn reinvite_reinit_media(call_id: i32) -> Result<(), PjsipError> {
        ensure_pjsip_thread();
        // SAFETY: pjsua is started; `opt` is a stack call setting initialised
        // by `pjsua_call_setting_default`; a null message-data pointer is
        // accepted.
        let status = unsafe {
            let mut opt: pjsua_sys::pjsua_call_setting = std::mem::zeroed();
            pjsua_sys::pjsua_call_setting_default(&mut opt);
            // The same single audio stream `Call::make` places.
            opt.aud_cnt = 1;
            opt.vid_cnt = 0;
            opt.txt_cnt = 0;
            opt.flag |= pjsua_sys::pjsua_call_flag_PJSUA_CALL_REINIT_MEDIA as c_uint;
            pjsua_sys::pjsua_call_reinvite2(call_id, &opt, std::ptr::null())
        };
        if status == PJ_SUCCESS {
            Ok(())
        } else {
            Err(PjsipError::CallMake(format!(
                "pjsua_call_reinvite2 returned {status}"
            )))
        }
    }

    /// Packets PJSIP's own stream has `(sent, received)` on this call. Both stay
    /// zero on a forwarded call, which is how a test (or an operator) can tell
    /// PJSIP is really carrying nothing.
    pub fn stream_packets(call_id: i32) -> Option<(u32, u32)> {
        ensure_pjsip_thread();
        // SAFETY: pjsua is started; `ss` is a zeroed stack value PJSIP writes.
        unsafe {
            let mut ss: pjsua_sys::pjsua_stream_stat = std::mem::zeroed();
            if pjsua_sys::pjsua_call_get_stream_stat(call_id, 0, &mut ss) != PJ_SUCCESS {
                return None;
            }
            Some((ss.rtcp.tx.pkt as u32, ss.rtcp.rx.pkt as u32))
        }
    }

    /// Put the call on hold (a re-INVITE with `a=sendonly`). Test support.
    pub fn hold_call(call_id: i32) -> Result<(), PjsipError> {
        ensure_pjsip_thread();
        // SAFETY: pjsua is started; a null message-data pointer is accepted.
        let status = unsafe { pjsua_sys::pjsua_call_set_hold(call_id, std::ptr::null()) };
        if status == PJ_SUCCESS {
            Ok(())
        } else {
            Err(PjsipError::CallMake(format!(
                "pjsua_call_set_hold returned {status}"
            )))
        }
    }

    /// Release a hold with a re-INVITE. Test support.
    pub fn unhold_call(call_id: i32) -> Result<(), PjsipError> {
        ensure_pjsip_thread();
        // SAFETY: pjsua is started; null call setting / message data are accepted.
        let status = unsafe {
            pjsua_sys::pjsua_call_reinvite(
                call_id,
                pjsua_sys::pjsua_call_flag_PJSUA_CALL_UNHOLD as c_uint,
                std::ptr::null(),
            )
        };
        if status == PJ_SUCCESS {
            Ok(())
        } else {
            Err(PjsipError::CallMake(format!(
                "pjsua_call_reinvite returned {status}"
            )))
        }
    }

    /// A plain re-INVITE with the current codec set. Test support.
    pub fn reinvite_plain(call_id: i32) -> Result<(), PjsipError> {
        ensure_pjsip_thread();
        // SAFETY: as `unhold_call`.
        let status = unsafe { pjsua_sys::pjsua_call_reinvite(call_id, 0, std::ptr::null()) };
        if status == PJ_SUCCESS {
            Ok(())
        } else {
            Err(PjsipError::CallMake(format!(
                "pjsua_call_reinvite returned {status}"
            )))
        }
    }

    fn fmtp_to_string(f: &pjsua_sys::pjmedia_codec_fmtp) -> String {
        let n = (f.cnt as usize).min(f.param.len());
        f.param[..n]
            .iter()
            .map(|p| {
                // SAFETY: PJSIP fills each name/value pj_str_t together with
                // its length; they live in the stream-info pool.
                let (name, val) = unsafe { (pj_str_to_string(&p.name), pj_str_to_string(&p.val)) };
                if val.is_empty() {
                    name
                } else {
                    format!("{name}={val}")
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// The client leg's negotiated media, read from the live stream.
    pub fn client_media(call_id: i32) -> Option<ClientMedia> {
        ensure_pjsip_thread();
        // SAFETY: pjsua is started; both out-structs are zeroed stack values
        // PJSIP writes; the audio union arm is only read when the stream type
        // is audio; the codec param pointer, when non-null, is pool memory
        // valid until the next call into PJSIP on this call.
        unsafe {
            let mut ci: pjsua_sys::pjsua_call_info = std::mem::zeroed();
            if pjsua_sys::pjsua_call_get_info(call_id, &mut ci) != PJ_SUCCESS || ci.media_cnt == 0 {
                return None;
            }
            let mut si: pjsua_sys::pjsua_stream_info = std::mem::zeroed();
            if pjsua_sys::pjsua_call_get_stream_info(call_id, 0, &mut si) != PJ_SUCCESS
                || si.type_ != pjsua_sys::pjmedia_type_PJMEDIA_TYPE_AUDIO
            {
                return None;
            }
            let aud = si.info.aud;
            let fmt = aud.fmt;
            let mut buf = [0 as c_char; 96];
            pjsua_sys::pj_sockaddr_print(
                &aud.rem_addr as *const _ as *const c_void,
                buf.as_mut_ptr(),
                buf.len() as c_int,
                3,
            );
            let remote_rtp = std::ffi::CStr::from_ptr(buf.as_ptr())
                .to_string_lossy()
                .parse::<SocketAddr>()
                .ok()
                .filter(|a| !a.ip().is_unspecified() && a.port() != 0);
            let (remote_fmtp, frames) = if aud.param.is_null() {
                (String::new(), 1)
            } else {
                let p = &*aud.param;
                (
                    fmtp_to_string(&p.setting.enc_fmtp),
                    p.setting.frm_per_pkt.max(1),
                )
            };
            let media = ci.media[0];
            let active = media.status == pjsua_sys::pjsua_call_media_status_PJSUA_CALL_MEDIA_ACTIVE;
            let encoding = media.dir & pjsua_sys::pjmedia_dir_PJMEDIA_DIR_ENCODING != 0;
            let event = |pt: c_int| u8::try_from(pt).ok().filter(|&p| p != 0);
            Some(ClientMedia {
                encoding_name: pj_str_to_string(&fmt.encoding_name),
                clock_rate: fmt.clock_rate,
                remote_rtp,
                tx_pt: aud.tx_pt as u8,
                rx_pt: aud.rx_pt as u8,
                tx_event_pt: event(aud.tx_event_pt),
                rx_event_pt: event(aud.rx_event_pt),
                remote_fmtp,
                frames_per_packet: frames,
                send_to_client: active && encoding,
                has_media: active
                    || media.status
                        == pjsua_sys::pjsua_call_media_status_PJSUA_CALL_MEDIA_REMOTE_HOLD
                    || media.status
                        == pjsua_sys::pjsua_call_media_status_PJSUA_CALL_MEDIA_LOCAL_HOLD,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Call ids are process-global, so each test uses its own.

    #[test]
    fn a_call_with_no_entry_is_on_the_normal_path() {
        assert!(!is_registered(9001));
    }

    #[test]
    fn registering_and_clearing_a_forward_entry() {
        register_forward(9002, 40010, "AMR-WB", 16000);
        assert!(is_registered(9002));
        clear(9002);
        assert!(!is_registered(9002));
    }

    #[test]
    fn a_disconnect_forgets_the_entry_so_a_reused_call_id_starts_clean() {
        register_forward(9003, 40010, "AMR-WB", 16000);
        lock(&GENERATION).insert(9003, 4);
        on_call_disconnected(9003);
        assert!(!is_registered(9003));
        assert_eq!(media_generation(9003), 0);
    }

    #[test]
    fn the_generation_advances_per_stream() {
        assert_eq!(media_generation(9004), 0);
        bump_generation(9004);
        bump_generation(9004);
        assert_eq!(media_generation(9004), 2);
        on_call_disconnected(9004);
    }

    #[test]
    fn a_quiet_call_with_no_live_stream_reports_nothing_to_pause() {
        assert!(!register_quiet(9005));
        assert!(is_registered(9005));
        on_call_disconnected(9005);
    }

    #[test]
    fn the_reoffer_must_still_offer_the_forwarded_codec() {
        let amr_wb = ["96 AMR-WB/16000", "101 telephone-event/8000"];
        assert!(offer_keeps_codec(amr_wb, "AMR-WB", 16000));
        assert!(offer_keeps_codec(["97 amr-wb/16000/1"], "AMR-WB", 16000));
        // A different codec, a different rate, or a same-prefixed name.
        assert!(!offer_keeps_codec(["9 G722/8000"], "AMR-WB", 16000));
        assert!(!offer_keeps_codec(["96 AMR/8000"], "AMR-WB", 16000));
        assert!(!offer_keeps_codec(["96 AMR-WB/8000"], "AMR-WB", 16000));
        assert!(!offer_keeps_codec(["96 AMR-WB+/16000"], "AMR-WB", 16000));
        assert!(!offer_keeps_codec(Vec::<&str>::new(), "AMR-WB", 16000));
    }
}
