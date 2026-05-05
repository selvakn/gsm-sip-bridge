use crate::account::Account;
use crate::error::PjsipError;
#[cfg(feature = "pjsip-linked")]
use crate::endpoint::ensure_pjsip_thread;

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

            unsafe {
                let uri_cstr = CString::new(dest_uri)
                    .map_err(|_| PjsipError::CallMake("invalid destination URI".into()))?;
                let uri = pjsua_sys::pj_str(uri_cstr.as_ptr() as *mut i8);

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
                    let mut hname = pjsua_sys::pj_str(name_c.as_ptr() as *mut i8);
                    let mut hvalue = pjsua_sys::pj_str(value_c.as_ptr() as *mut i8);
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

                let mut call_id: pjsua_sys::pjsua_call_id = -1;
                let status = pjsua_sys::pjsua_call_make_call(
                    _account.account_id(),
                    &uri,
                    std::ptr::null(),
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

    pub fn hangup(&mut self) -> Result<(), PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            ensure_pjsip_thread();
            unsafe {
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
            // SAFETY: call_id is valid if state is Confirmed
            if self.state == CallState::Confirmed {
                unsafe {
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

    pub fn set_state(&mut self, state: CallState) {
        self.state = state;
    }

    pub fn call_id(&self) -> i32 {
        self.call_id
    }
}
