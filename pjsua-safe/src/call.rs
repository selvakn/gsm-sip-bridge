use crate::account::Account;
#[cfg(feature = "pjsip-linked")]
use crate::endpoint::ensure_pjsip_thread;
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

                let mut call_id: pjsua_sys::pjsua_call_id = -1;
                let status = pjsua_sys::pjsua_call_make_call(
                    _account.account_id(),
                    &uri,
                    &opt,
                    std::ptr::null_mut(),
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
        Self { call_id, state }
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

    pub fn call_id(&self) -> i32 {
        self.call_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
