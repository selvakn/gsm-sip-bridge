use crate::account::Account;
#[cfg(feature = "pjsip-linked")]
use crate::endpoint::ensure_pjsip_thread;
use crate::endpoint::Disconnect;
use crate::error::PjsipError;

pub type SlotId = i32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    Null,
    Calling,
    Incoming,
    Early,
    Connecting,
    Confirmed,
    Disconnected,
}

pub trait CallStateListener: Send + Sync {
    fn on_call_state(&self, call_id: i32, state: CallState);
    fn on_call_media_state(&self, call_id: i32);
}

pub struct Call {
    #[allow(dead_code)]
    call_id: i32,
    state: CallState,
    /// Identifies this call to `on_call_state_cb` across call-id reuse: the
    /// `user_data` `Call::make` hands PJSUA. 0 for a call we did not place.
    #[allow(dead_code)]
    token: usize,
    /// Stub builds only: what [`Call::last_disconnect`] reports, set by tests
    /// via [`Call::set_last_disconnect`]. A linked build reads PJSIP instead.
    #[allow(dead_code)]
    last_disconnect: Option<Disconnect>,
}

impl Call {
    pub fn make(
        _account: &Account,
        dest_uri: &str,
        _listener: Option<Box<dyn CallStateListener>>,
        extra_headers: &[(&str, &str)],
    ) -> Result<Self, PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            ensure_pjsip_thread();
            use std::ffi::CString;

            unsafe // SAFETY: PJSIP initialized after ensure_pjsip_thread; stack structs and C strings valid for this FFI call
            {
                let uri_cstr = CString::new(dest_uri)
                    .map_err(|_| PjsipError::CallMake("invalid destination URI".into()))?;
                let uri = pjsua_sys::pj_str(uri_cstr.as_ptr() as *mut std::os::raw::c_char);

                let mut msg_data: pjsua_sys::pjsua_msg_data = std::mem::zeroed();
                pjsua_sys::pjsua_msg_data_init(&mut msg_data);

                let mut header_cstrings: Vec<(CString, CString)> = Vec::new();
                for (name, value) in extra_headers {
                    let name_c = CString::new(*name).unwrap_or_default();
                    let value_c = CString::new(*value).unwrap_or_default();
                    header_cstrings.push((name_c, value_c));
                }

                let mut generic_headers: Vec<pjsua_sys::pjsip_generic_string_hdr> =
                    Vec::with_capacity(header_cstrings.len());
                for _ in 0..header_cstrings.len() {
                    generic_headers.push(std::mem::zeroed());
                }

                for (i, (name_c, value_c)) in header_cstrings.iter().enumerate() {
                    let mut hname = pjsua_sys::pj_str(name_c.as_ptr() as *mut std::os::raw::c_char);
                    let mut hvalue = pjsua_sys::pj_str(value_c.as_ptr() as *mut std::os::raw::c_char);
                    pjsua_sys::pjsip_generic_string_hdr_init2(
                        &mut generic_headers[i],
                        &mut hname,
                        &mut hvalue,
                    );
                    pjsua_sys::pj_list_insert_before(
                        &mut msg_data.hdr_list as *mut _ as *mut pjsua_sys::pj_list_type,
                        &mut generic_headers[i] as *mut _ as *mut pjsua_sys::pj_list_type,
                    );
                }

                let msg_data_ptr = if extra_headers.is_empty() {
                    std::ptr::null()
                } else {
                    &msg_data as *const _
                };

                // One audio stream, nothing else. PJSUA 2.16 defaults
                // `txt_cnt` to 1, which puts a T.140 `m=text` section in every
                // offer — a stream nothing here bridges, whose payload types
                // are numbered independently of the audio section's (it reuses
                // 100 for `red/1000`, colliding with audio's L16), and which
                // our own SDP answer in `ims::agent` doesn't echo back at all.
                let mut opt: pjsua_sys::pjsua_call_setting = std::mem::zeroed();
                pjsua_sys::pjsua_call_setting_default(&mut opt);
                opt.aud_cnt = 1;
                opt.vid_cnt = 0;
                opt.txt_cnt = 0;

                let token = crate::endpoint::next_call_token();
                let mut call_id: pjsua_sys::pjsua_call_id = -1;
                let status = pjsua_sys::pjsua_call_make_call(
                    _account.account_id(),
                    &uri,
                    &opt,
                    token as *mut std::os::raw::c_void,
                    msg_data_ptr,
                    &mut call_id,
                );
                if status != crate::error::PJ_SUCCESS {
                    return Err(PjsipError::CallMake(format!(
                        "pjsua_call_make_call returned {status}"
                    )));
                }

                return Ok(Self {
                    call_id,
                    state: CallState::Calling,
                    token,
                    last_disconnect: None,
                });
            }
        }

        #[cfg(not(feature = "pjsip-linked"))]
        {
            let _ = extra_headers;
            tracing::info!(dest = %dest_uri, "outbound call initiated (stub mode)");
            Ok(Self {
                call_id: 0,
                state: CallState::Calling,
                token: 0,
                last_disconnect: None,
            })
        }
    }

    /// Wraps an already-received call (`Endpoint::poll_incoming_call`) so it
    /// can be `answer`ed, `hangup`, or polled like any other `Call` (spec
    /// 025, research.md R-007).
    ///
    /// Safe — no FFI. `Call`'s only invariant is "this `call_id` is valid in
    /// PJSUA", which already holds: PJSIP handed us this `call_id` via
    /// `on_incoming_call`, the same way `Call::make`'s `call_id` comes from
    /// `pjsua_call_make_call`.
    pub fn from_id(call_id: i32, state: CallState) -> Self {
        Self {
            call_id,
            state,
            token: 0,
            last_disconnect: None,
        }
    }

    /// Answers an incoming call with the given SIP status `code` (e.g. 180
    /// for ringing, 200 to accept). Mirrors `hangup`'s
    /// `#[cfg(feature = "pjsip-linked")]`/stub split.
    pub fn answer(&mut self, code: u32) -> Result<(), PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            ensure_pjsip_thread();
            unsafe // SAFETY: PJSIP initialized; call_id valid for answer on this call
            {
                let status = pjsua_sys::pjsua_call_answer(
                    self.call_id,
                    code,
                    std::ptr::null(),
                    std::ptr::null(),
                );
                if status != crate::error::PJ_SUCCESS {
                    return Err(PjsipError::CallAnswer(format!(
                        "pjsua_call_answer({code}) returned {status}"
                    )));
                }
            }
        }

        #[cfg(not(feature = "pjsip-linked"))]
        {
            tracing::info!(code, "call answered (stub mode)");
        }

        if (200..300).contains(&code) {
            self.state = CallState::Confirmed;
        }
        Ok(())
    }

    pub fn hangup(&mut self) -> Result<(), PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            ensure_pjsip_thread();
            unsafe // SAFETY: PJSIP initialized; call_id valid for hangup on this call
            {
                let status = pjsua_sys::pjsua_call_hangup(
                    self.call_id,
                    200,
                    std::ptr::null(),
                    std::ptr::null(),
                );
                // 171140 = PJSIP_ESESSIONTERMINATED (already disconnected)
                const PJSIP_ESESSIONTERMINATED: i32 = 171140;
                if status != crate::error::PJ_SUCCESS && status != PJSIP_ESESSIONTERMINATED {
                    return Err(PjsipError::CallHangup(format!(
                        "pjsua_call_hangup returned {status}"
                    )));
                }
            }
        }

        self.state = CallState::Disconnected;
        Ok(())
    }

    /// Best-effort extraction of the destination number an incoming call
    /// named, for outbound calling's PBX/phone entry points (spec 025).
    ///
    /// Reads `local_info` — PJSUA's already-rendered "Local URI" string,
    /// derived from the request's To header per RFC 3261 §12.1.1 UAS dialog
    /// construction — rather than the raw Request-URI: printing an
    /// arbitrary `pjsip_uri*` requires PJSIP's `PJ_INLINE` URI-printing
    /// functions, which have no linkable symbol without extra `bindgen`
    /// configuration this crate does not carry. This assumes the sender
    /// sets the To header's URI the same as the Request-URI for
    /// trunk-style routing — standard for a PBX/gateway trunk, but not a
    /// SIP guarantee in general.
    ///
    /// `None` if `local_info` doesn't parse as a `sip:`/`sips:` URI with a
    /// user part, or in stub builds (no real call info exists there).
    pub fn request_destination(&self) -> Option<String> {
        #[cfg(feature = "pjsip-linked")]
        {
            ensure_pjsip_thread();
            unsafe // SAFETY: PJSIP initialized; call_id valid; writable stack out-param; local_info.ptr is valid UTF-8-ish PJSIP-owned text for the info's lifetime
            {
                let mut info: pjsua_sys::pjsua_call_info = std::mem::zeroed();
                if pjsua_sys::pjsua_call_get_info(self.call_id, &mut info) != crate::error::PJ_SUCCESS
                {
                    return None;
                }
                // `slice::from_raw_parts` requires a non-null, aligned
                // pointer even for a zero-length slice — an empty
                // `local_info` is a plausible `pj_str_t { ptr: null, slen: 0 }`
                // this crate cannot rule out from the FFI signature alone.
                if info.local_info.ptr.is_null() {
                    return None;
                }
                let bytes = std::slice::from_raw_parts(
                    info.local_info.ptr as *const u8,
                    info.local_info.slen.max(0) as usize,
                );
                let text = std::str::from_utf8(bytes).ok()?;
                return parse_uri_user(text);
            }
        }

        #[cfg(not(feature = "pjsip-linked"))]
        {
            None
        }
    }

    pub fn conf_slot(&self) -> Option<SlotId> {
        #[cfg(feature = "pjsip-linked")]
        {
            // Idempotent — `pj_thread_is_registered` short-circuits. Applied at
            // every pjlib entry point rather than reasoning per caller about
            // which thread it runs on; that reasoning is exactly what left
            // `Endpoint::drop` calling pjlib from an unregistered Tokio worker.
            crate::endpoint::ensure_pjsip_thread();
            if self.state == CallState::Confirmed {
                unsafe // SAFETY: call_id valid when Confirmed; writable stack pjsua_call_info for out-param
                {
                    let info = std::mem::zeroed::<pjsua_sys::pjsua_call_info>();
                    let status =
                        pjsua_sys::pjsua_call_get_info(self.call_id, &info as *const _ as *mut _);
                    if status == crate::error::PJ_SUCCESS {
                        return Some(info.conf_slot as SlotId);
                    }
                }
            }
            return None;
        }

        #[cfg(not(feature = "pjsip-linked"))]
        {
            if self.state == CallState::Confirmed {
                Some(1)
            } else {
                None
            }
        }
    }

    pub fn state(&self) -> CallState {
        self.state
    }

    /// The call's state as PJSIP sees it *right now*, rather than the cached
    /// `state` field (which only changes when someone calls `set_state`).
    ///
    /// Needed to tell "the INVITE has been sent" apart from "a human actually
    /// picked up": the inbound VoWiFi bridge must not answer the carrier until
    /// the PBX leg reaches `Confirmed`, or the caller's ringback is cut off and
    /// replaced by dead air while the extension is still ringing.
    pub fn poll_state(&self) -> CallState {
        #[cfg(feature = "pjsip-linked")]
        {
            ensure_pjsip_thread();
            unsafe // SAFETY: PJSIP initialized; call_id owned by this Call; writable stack out-param
            {
                let mut info = std::mem::zeroed::<pjsua_sys::pjsua_call_info>();
                if pjsua_sys::pjsua_call_get_info(self.call_id, &mut info) != crate::error::PJ_SUCCESS
                {
                    // The call is gone as far as PJSIP is concerned.
                    return CallState::Disconnected;
                }
                // The id now belongs to a newer call: ours has ended.
                if pjsua_sys::pjsua_call_get_user_data(self.call_id) as usize != self.token {
                    return CallState::Disconnected;
                }
                // PJSIP marks the call DISCONNECTED before running
                // `on_call_state_cb`, which is where the final status is
                // captured. Keep reporting the call as in progress until that
                // capture exists, so `last_status` after `Disconnected` never
                // races it. Only for calls we placed (token != 0); the window
                // is the few microseconds the callback takes.
                if info.state == pjsua_sys::pjsip_inv_state_PJSIP_INV_STATE_DISCONNECTED
                    && self.token != 0
                    && crate::endpoint::disconnect_status(self.token).is_none()
                {
                    return CallState::Early;
                }
                #[allow(non_upper_case_globals)]
                return match info.state {
                    pjsua_sys::pjsip_inv_state_PJSIP_INV_STATE_NULL => CallState::Null,
                    pjsua_sys::pjsip_inv_state_PJSIP_INV_STATE_CALLING => CallState::Calling,
                    pjsua_sys::pjsip_inv_state_PJSIP_INV_STATE_INCOMING => CallState::Incoming,
                    pjsua_sys::pjsip_inv_state_PJSIP_INV_STATE_EARLY => CallState::Early,
                    pjsua_sys::pjsip_inv_state_PJSIP_INV_STATE_CONNECTING => CallState::Connecting,
                    pjsua_sys::pjsip_inv_state_PJSIP_INV_STATE_CONFIRMED => CallState::Confirmed,
                    _ => CallState::Disconnected,
                };
            }
        }

        // Stub builds have no real call to poll; report the cached state so
        // unit tests can drive it with `set_state`.
        #[cfg(not(feature = "pjsip-linked"))]
        {
            self.state
        }
    }

    pub fn set_state(&mut self, state: CallState) {
        self.state = state;
    }

    /// The final SIP status that ended this call — e.g. `486` when the far
    /// end rejected it, `408` when it timed out — or `None` if the call has
    /// not ended or PJSIP recorded no status for it.
    ///
    /// PJSUA resets the call's slot as soon as its `DISCONNECTED` callback
    /// returns, and may hand the id to another call, so the live call info
    /// is not trusted here. The status is the one captured inside that
    /// callback (`endpoint`'s `DISCONNECT_STATUS`), found by this call's
    /// token. Only calls placed with [`Call::make`] have one; any other call
    /// reports `None`.
    pub fn last_status(&self) -> Option<u16> {
        self.last_disconnect().and_then(|d| d.status)
    }

    /// Everything the disconnect callback captured for this call: the final
    /// status and, when the peer's failure response carried
    /// `Reason: Q.850;cause=N`, that cause. `None` until the call has ended
    /// and the callback has run, and for calls not placed with
    /// [`Call::make`]. Read from the same per-token capture as
    /// [`Call::last_status`].
    pub fn last_disconnect(&self) -> Option<Disconnect> {
        #[cfg(feature = "pjsip-linked")]
        {
            if self.token == 0 {
                return None;
            }
            crate::endpoint::disconnect_status(self.token)
        }

        #[cfg(not(feature = "pjsip-linked"))]
        {
            self.last_disconnect
        }
    }

    /// Stub builds: sets what [`Call::last_status`] reports, leaving any
    /// cause unset. A linked build ignores it — PJSIP is the only source of
    /// truth there.
    pub fn set_last_status(&mut self, status: Option<u16>) {
        self.set_last_disconnect(Disconnect {
            status,
            ..Disconnect::default()
        });
    }

    /// Stub builds: sets what [`Call::last_disconnect`] reports. A linked
    /// build ignores it.
    pub fn set_last_disconnect(&mut self, disconnect: Disconnect) {
        self.last_disconnect = Some(disconnect);
    }

    pub fn call_id(&self) -> i32 {
        self.call_id
    }
}

/// Extracts the `user` part from a `sip:`/`sips:` URI, with or without a
/// display-name/angle-bracket wrapper, ignoring URI parameters and
/// headers. `None` for anything else (e.g. a `tel:` URI, which this
/// project's PBX-facing routing does not use).
///
/// Only called from `request_destination`'s `pjsip-linked` branch — a stub
/// build has no real call info to parse, so this is otherwise dead code
/// there; still unit-tested unconditionally below.
#[cfg_attr(not(feature = "pjsip-linked"), allow(dead_code))]
fn parse_uri_user(text: &str) -> Option<String> {
    let scheme_at = text.find("sip:").or_else(|| text.find("sips:"))?;
    let (_, rest) = text[scheme_at..].split_once(':')?;
    let user = rest.split(['@', '>', ';']).next()?;
    if user.is_empty() {
        None
    } else {
        Some(user.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "pjsip-linked"))]
    #[test]
    fn a_stub_call_reports_the_final_status_it_was_given() {
        let mut call = Call::from_id(3, CallState::Early);
        assert_eq!(call.last_status(), None);
        call.set_last_status(Some(603));
        assert_eq!(call.last_status(), Some(603));
    }

    #[cfg(not(feature = "pjsip-linked"))]
    #[test]
    fn a_stub_call_reports_the_disconnect_cause_it_was_given() {
        let mut call = Call::from_id(3, CallState::Early);
        assert_eq!(call.last_disconnect(), None);
        let rejected = Disconnect {
            status: Some(403),
            q850: Some(21),
            response_seen: true,
        };
        call.set_last_disconnect(rejected);
        assert_eq!(call.last_disconnect(), Some(rejected));
        assert_eq!(call.last_status(), Some(403));
    }

    #[test]
    fn parse_uri_user_bare() {
        assert_eq!(
            parse_uri_user("sip:9000000000@192.168.1.1:5062"),
            Some("9000000000".to_string())
        );
    }

    #[test]
    fn parse_uri_user_with_display_name_and_angle_brackets() {
        assert_eq!(
            parse_uri_user("\"PBX\" <sip:9000000000@192.168.1.1:5062>"),
            Some("9000000000".to_string())
        );
    }

    #[test]
    fn parse_uri_user_strips_uri_parameters() {
        assert_eq!(
            parse_uri_user("sip:9000000000;user=phone@192.168.1.1"),
            Some("9000000000".to_string())
        );
    }

    #[test]
    fn parse_uri_user_none_for_non_sip_scheme() {
        assert_eq!(parse_uri_user("tel:+919000000000"), None);
    }

    #[test]
    fn parse_uri_user_none_for_empty_user() {
        assert_eq!(parse_uri_user("sip:@192.168.1.1"), None);
    }

    #[test]
    fn from_id_carries_the_given_id_and_state() {
        let call = Call::from_id(42, CallState::Incoming);
        assert_eq!(call.call_id(), 42);
        assert_eq!(call.state(), CallState::Incoming);
    }

    #[test]
    fn answering_with_a_2xx_code_marks_the_call_confirmed() {
        let mut call = Call::from_id(1, CallState::Incoming);
        call.answer(200).unwrap();
        assert_eq!(call.state(), CallState::Confirmed);
    }

    #[test]
    fn answering_with_a_1xx_code_leaves_state_unchanged() {
        let mut call = Call::from_id(1, CallState::Incoming);
        call.answer(180).unwrap();
        assert_eq!(call.state(), CallState::Incoming);
    }
}
